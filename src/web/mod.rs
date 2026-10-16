//! Local web server: static UI, `/ws/sensing`, SSE `/events`, a small authenticated
//! JSON API. std-only; thread per connection with a hard connection cap.
//!
//! Security posture
//!  * loopback bind unless the operator opts in (`http.allow_lan`); no TLS is provided
//!  * everything except static UI assets, `/healthz` and `/login` needs the node token
//!    (Bearer header or HttpOnly SameSite=Strict cookie); WebSockets may use a
//!    single-use 30 s ticket instead
//!  * `Host` must be an IP literal or `localhost` (DNS-rebinding defence); `Origin`, when
//!    present, must equal `Host` on state-changing requests and WebSocket upgrades
//!  * bounded request size/time, one request per connection, no path escapes

pub mod auth;
pub mod crypto;
pub mod frames;
pub mod http;
pub mod hub;
pub mod map;
pub mod ws;

use crate::config::Config;
use crate::engine::Report;
use crate::export::mqtt::MqttStatus;
use crate::ingest::Health;
use auth::{Auth, Verdict};
use http::{read_request, write_response, HttpErr, Request};
use hub::Hub;
use map::MapStore;
use serde_json::{json, Value};
use std::io::Write;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_STATIC: u64 = 16 * 1024 * 1024;

#[derive(Debug)]
pub struct AppState {
    pub cfg: Config,
    pub auth: Auth,
    pub ui_root: Option<PathBuf>,
    pub hub_ws: Hub,
    pub hub_sse: Hub,
    pub latest: Mutex<Option<Report>>,
    pub map: MapStore,
    pub healths: Mutex<Vec<Arc<Health>>>,
    pub mqtt: Mutex<Option<Arc<MqttStatus>>>,
    pub udp_sent: AtomicU64,
    pub started: Instant,
    pub synthetic_only: AtomicBool,
    pub stop: AtomicBool,
    active: AtomicUsize,
}

impl AppState {
    pub fn new(cfg: Config, token: String) -> Result<Arc<Self>, String> {
        let ui_root = match std::fs::canonicalize(&cfg.http.ui_dir) {
            Ok(p) if p.is_dir() => Some(p),
            _ => {
                eprintln!(
                    "[web] ui_dir {:?} not found: serving API only",
                    cfg.http.ui_dir
                );
                None
            }
        };
        let map = MapStore::open(Some(cfg.state_path("map.json")))?;
        Ok(Arc::new(Self {
            cfg,
            auth: Auth::new(token),
            ui_root,
            hub_ws: Hub::default(),
            hub_sse: Hub::default(),
            latest: Mutex::new(None),
            map,
            healths: Mutex::new(Vec::new()),
            mqtt: Mutex::new(None),
            udp_sent: AtomicU64::new(0),
            started: Instant::now(),
            synthetic_only: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            active: AtomicUsize::new(0),
        }))
    }

    pub fn publish(&self, r: &Report) {
        let on = self.cfg.dsp.presence_on;
        let sim = self.synthetic_only.load(Ordering::Relaxed);
        self.hub_ws
            .publish(frames::sensing_update(r, on, sim).into());
        self.hub_sse.publish(frames::event(r, on).into());
        if let Ok(mut l) = self.latest.lock() {
            *l = Some(r.clone());
        }
    }

    fn source_state(&self) -> &'static str {
        let fresh = self.healths.lock().is_ok_and(|h| {
            let now = crate::sample::unix_ms();
            h.iter().any(|s| {
                now.saturating_sub(s.last_ok_unix_ms.load(Ordering::Relaxed)) < 10_000
                    && s.samples.load(Ordering::Relaxed) > 0
            })
        });
        match (fresh, self.synthetic_only.load(Ordering::Relaxed)) {
            (true, true) => "synthetic",
            (true, false) => "live_verified",
            (false, _) => "stale",
        }
    }

    fn status(&self) -> Value {
        let srcs: Vec<_> = self
            .healths
            .lock()
            .map(|h| h.iter().map(|s| s.snapshot()).collect())
            .unwrap_or_default();
        let mqtt = self.mqtt.lock().ok().and_then(|m| m.clone()).map(|m| {
            json!({"connected": m.connected.load(Ordering::Relaxed), "published": m.published.load(Ordering::Relaxed),
                   "connects": m.connects.load(Ordering::Relaxed), "dropped": m.dropped.load(Ordering::Relaxed)})
        });
        let rep = self.latest.lock().ok().and_then(|l| l.clone());
        json!({
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
            "node": self.cfg.node_id,
            "device": self.cfg.device_name,
            "uptime_s": self.started.elapsed().as_secs(),
            "source": if self.synthetic_only.load(Ordering::Relaxed) { "simulated" } else { "wifi" },
            "source_state": self.source_state(),
            "mode": rep.as_ref().map(|r| r.mode),
            "sources": srcs,
            "mqtt": mqtt,
            "udp": {"sent": self.udp_sent.load(Ordering::Relaxed)},
            "stream": {"ws_clients": self.hub_ws.subscriber_count(), "sse_clients": self.hub_sse.subscriber_count(),
                       "dropped_frames": self.hub_ws.dropped.load(Ordering::Relaxed) + self.hub_sse.dropped.load(Ordering::Relaxed)},
        })
    }
}

#[derive(Debug)]
pub struct Server {
    pub addr: SocketAddr,
    handle: std::thread::JoinHandle<()>,
}
impl Server {
    pub fn join(self) {
        let _ = self.handle.join();
    }
}

struct ConnGuard<'a>(&'a AtomicUsize);
impl Drop for ConnGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub fn serve(state: Arc<AppState>) -> Result<Server, String> {
    let listener = TcpListener::bind(&state.cfg.http.bind)
        .map_err(|e| format!("http bind {}: {e}", state.cfg.http.bind))?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let handle = std::thread::Builder::new()
        .name("http-accept".into())
        .spawn(move || {
            while !state.stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut s, _)) => {
                        if state.active.fetch_add(1, Ordering::SeqCst)
                            >= state.cfg.http.max_connections
                        {
                            state.active.fetch_sub(1, Ordering::SeqCst);
                            let _ = write_response(
                                &mut s,
                                503,
                                "Service Unavailable",
                                &[("Retry-After", "5")],
                                b"busy\n",
                                false,
                            );
                            continue;
                        }
                        let st = Arc::clone(&state);
                        let spawned =
                            std::thread::Builder::new()
                                .name("http-conn".into())
                                .spawn(move || {
                                    let _g = ConnGuard(&st.active);
                                    let _ = s.set_nonblocking(false);
                                    let _ = s.set_nodelay(true);
                                    handle(s, &st);
                                });
                        if spawned.is_err() {
                            state.active.fetch_sub(1, Ordering::SeqCst);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20))
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(100)),
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Server { addr, handle })
}

// ------------------------------------------------------------------ request handling

struct Resp {
    status: u16,
    reason: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn new(status: u16, reason: &'static str) -> Self {
        Self {
            status,
            reason,
            headers: vec![("Cache-Control", "no-store".into())],
            body: Vec::new(),
        }
    }
    fn text(status: u16, reason: &'static str, msg: &str) -> Self {
        let mut r = Self::new(status, reason);
        r.headers
            .push(("Content-Type", "text/plain; charset=utf-8".into()));
        r.body = format!("{msg}\n").into_bytes();
        r
    }
    fn json(v: &Value) -> Self {
        let mut r = Self::new(200, "OK");
        r.headers.push(("Content-Type", "application/json".into()));
        r.body = v.to_string().into_bytes();
        r
    }
}

pub fn host_ok(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((ip, tail)) if tail.is_empty() || tail.starts_with(':') => ip,
            _ => return false,
        }
    } else {
        host.rsplit_once(':').map_or(host, |(h, p)| {
            if p.bytes().all(|b| b.is_ascii_digit()) {
                h
            } else {
                host
            }
        })
    };
    name.eq_ignore_ascii_case("localhost") || name.parse::<IpAddr>().is_ok()
}

fn origin_ok(req: &Request) -> bool {
    match (req.header("origin"), req.header("host")) {
        (None, _) => true, // non-browser clients send none; cookies are SameSite=Strict
        (Some(o), Some(h)) => o
            .strip_prefix("http://")
            .or_else(|| o.strip_prefix("https://"))
            .is_some_and(|x| x.eq_ignore_ascii_case(h)),
        (Some(_), None) => false,
    }
}

fn content_type(p: &Path) -> &'static str {
    match p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "webmanifest" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "wasm" => "application/wasm",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Resolve a request path inside `root`, refusing anything that escapes it (including via symlinks).
pub fn resolve_static(root: &Path, req_path: &str) -> Option<PathBuf> {
    let mut rel = PathBuf::new();
    for seg in req_path.split('/').filter(|s| !s.is_empty()) {
        if seg == ".." || seg == "." || seg.contains('\\') || seg.starts_with('.') {
            return None;
        }
        rel.push(seg);
    }
    let mut candidates = Vec::new();
    if rel.as_os_str().is_empty() {
        candidates.push(root.join("index.html"));
    } else {
        candidates.push(root.join(&rel));
        candidates.push(root.join(&rel).with_extension("html"));
        candidates.push(root.join(&rel).join("index.html"));
    }
    for c in candidates {
        if let Ok(real) = std::fs::canonicalize(&c) {
            if real.starts_with(root) && real.is_file() {
                return Some(real);
            }
        }
    }
    None
}

fn static_resp(state: &AppState, req: &Request) -> Resp {
    let Some(root) = &state.ui_root else {
        return Resp::text(404, "Not Found", "no UI installed");
    };
    let Some(file) = resolve_static(root, &req.path) else {
        return Resp::text(404, "Not Found", "not found");
    };
    match std::fs::metadata(&file) {
        Ok(m) if m.len() <= MAX_STATIC => match std::fs::read(&file) {
            Ok(body) => {
                let mut r = Resp::new(200, "OK");
                r.headers = vec![
                    ("Cache-Control", "no-cache".into()),
                    ("Content-Type", content_type(&file).into()),
                ];
                r.body = body;
                r
            }
            Err(_) => Resp::text(500, "Internal Server Error", "read error"),
        },
        _ => Resp::text(413, "Payload Too Large", "file too large"),
    }
}

fn send(s: &mut TcpStream, r: &Resp, head_only: bool) {
    let extra: Vec<(&str, &str)> = r.headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
    let _ = write_response(s, r.status, r.reason, &extra, &r.body, head_only);
}

fn is_protected(path: &str) -> bool {
    path.starts_with("/api/") || path.starts_with("/ws/") || path == "/events"
}

fn handle(mut s: TcpStream, st: &Arc<AppState>) {
    let req = match read_request(&mut s, Instant::now() + Duration::from_secs(10)) {
        Ok(r) => r,
        Err(HttpErr::Closed) => return,
        Err(e) => {
            let (code, reason) = e.status();
            send(&mut s, &Resp::text(code, reason, reason), false);
            return;
        }
    };
    if !req.header("host").is_some_and(host_ok) {
        send(
            &mut s,
            &Resp::text(421, "Misdirected Request", "bad host"),
            false,
        );
        return;
    }
    let head_only = req.method == "HEAD";
    if !matches!(req.method.as_str(), "GET" | "HEAD" | "POST") {
        send(
            &mut s,
            &Resp::text(405, "Method Not Allowed", "method not allowed"),
            false,
        );
        return;
    }
    let is_post = req.method == "POST";
    if (is_post || req.is_upgrade()) && !origin_ok(&req) {
        send(
            &mut s,
            &Resp::text(403, "Forbidden", "cross-origin request refused"),
            false,
        );
        return;
    }

    // ---- public routes
    match (req.method.as_str(), req.path.as_str()) {
        // Liveness probes the retained UI issues. Carry no data beyond "the process is up".
        ("GET" | "HEAD", "/health" | "/health/live" | "/health/ready") => {
            return send(&mut s, &Resp::json(&json!({"status": "ok"})), head_only)
        }
        ("GET" | "HEAD", "/healthz") => {
            return send(&mut s, &Resp::text(200, "OK", "ok"), head_only)
        }
        ("GET", "/login") => {
            let verdict = req
                .query_get("token")
                .map_or(Verdict::Denied, |t| st.auth.login(t));
            let r = match verdict {
                Verdict::Ok => {
                    let mut r = Resp::new(303, "See Other");
                    r.headers.push(("Location", "/".into()));
                    r.headers.push(("Set-Cookie", st.auth.session_cookie()));
                    r
                }
                Verdict::Throttled => {
                    Resp::text(429, "Too Many Requests", "too many failed attempts")
                }
                Verdict::Denied => Resp::text(401, "Unauthorized", "invalid token"),
            };
            return send(&mut s, &r, false);
        }
        _ => {}
    }
    if !is_protected(&req.path) {
        if is_post {
            return send(
                &mut s,
                &Resp::text(405, "Method Not Allowed", "method not allowed"),
                false,
            );
        }
        return send(&mut s, &static_resp(st, &req), head_only);
    }

    // ---- authenticated routes
    let ticket_ok = req.path == "/ws/sensing"
        && req
            .query_get("ticket")
            .is_some_and(|t| st.auth.redeem_ticket(t));
    if !ticket_ok {
        match st.auth.check(&req) {
            Verdict::Ok => {}
            Verdict::Throttled => {
                return send(
                    &mut s,
                    &Resp::text(429, "Too Many Requests", "too many failed attempts"),
                    false,
                )
            }
            Verdict::Denied => {
                let mut r = Resp::text(401, "Unauthorized", "authentication required");
                r.headers.push(("WWW-Authenticate", "Bearer".into()));
                return send(&mut s, &r, false);
            }
        }
    }
    if is_post
        && !req
            .header("content-type")
            .is_some_and(|c| c.starts_with("application/json"))
        && !req.body.is_empty()
    {
        return send(
            &mut s,
            &Resp::text(415, "Unsupported Media Type", "application/json required"),
            false,
        );
    }
    route(s, st, &req, head_only);
}

fn body_json(req: &Request) -> Result<Value, Resp> {
    if req.body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&req.body).map_err(|_| Resp::text(400, "Bad Request", "invalid json"))
}

fn route(mut s: TcpStream, st: &Arc<AppState>, req: &Request, head_only: bool) {
    let get = matches!(req.method.as_str(), "GET" | "HEAD");
    let resp = match (req.path.as_str(), get) {
        ("/ws/sensing", true) => return serve_ws(s, st, req),
        ("/events", true) => return serve_sse(s, st),
        ("/api/v1/status", true) => Resp::json(&st.status()),
        ("/api/v1/info", true) => Resp::json(
            &json!({"name": "rvcsi", "version": env!("CARGO_PKG_VERSION"), "node": st.cfg.node_id, "device": st.cfg.device_name}),
        ),
        ("/api/v1/sensing/latest", true) => match st.hub_ws.latest() {
            Some(f) => {
                let mut r = Resp::new(200, "OK");
                r.headers.push(("Content-Type", "application/json".into()));
                r.body = f.as_bytes().to_vec();
                r
            }
            None => Resp::new(204, "No Content"),
        },
        ("/api/v1/map", true) => Resp::json(&st.map.snapshot()),
        ("/api/v1/ws-ticket", false) => match st.auth.mint_ticket() {
            Ok(t) => Resp::json(&json!({"ticket": t, "expires_in": 30})),
            Err(e) => Resp::text(429, "Too Many Requests", &e),
        },
        ("/api/v1/map/object", false) => match body_json(req) {
            Ok(b) => match st.map.upsert(&b) {
                Ok(id) => Resp::json(&json!({"ok": true, "id": id})),
                Err(e) => Resp::text(400, "Bad Request", &e),
            },
            Err(r) => r,
        },
        ("/api/v1/map/object/delete", false) => match body_json(req) {
            Ok(b) => match b
                .get("id")
                .and_then(Value::as_u64)
                .map(|id| st.map.delete(id))
            {
                Some(Ok(removed)) => Resp::json(&json!({"ok": removed})),
                Some(Err(e)) => Resp::text(500, "Internal Server Error", &e),
                None => Resp::text(400, "Bad Request", "id required"),
            },
            Err(r) => r,
        },
        ("/api/v1/map/reset", false) => match st.map.reset() {
            Ok(()) => Resp::json(&json!({"ok": true})),
            Err(e) => Resp::text(500, "Internal Server Error", &e),
        },
        (p, _) if p.starts_with("/api/") || p.starts_with("/ws/") => {
            Resp::text(404, "Not Found", "not found")
        }
        _ => Resp::text(404, "Not Found", "not found"),
    };
    send(&mut s, &resp, head_only);
}

fn serve_ws(mut s: TcpStream, st: &Arc<AppState>, req: &Request) {
    let key = req.header("sec-websocket-key").filter(|k| {
        k.len() == 24
            && k.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
    });
    let (true, Some(key), Some("13")) =
        (req.is_upgrade(), key, req.header("sec-websocket-version"))
    else {
        return send(
            &mut s,
            &Resp::text(400, "Bad Request", "websocket upgrade required"),
            false,
        );
    };
    let Some((rx, latest)) = st.hub_ws.subscribe() else {
        return send(
            &mut s,
            &Resp::text(503, "Service Unavailable", "too many streaming clients"),
            false,
        );
    };
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        crypto::ws_accept(key)
    );
    if s.write_all(head.as_bytes()).is_err() {
        return;
    }
    let st2 = Arc::clone(st);
    ws::serve(s, &rx, latest, &st.stop, move |text| {
        let v: Value = serde_json::from_str(text).ok()?;
        match v.get("type").and_then(Value::as_str)? {
            "ping" => {
                Some(json!({"type": "pong", "timestamp": crate::sample::unix_ms()}).to_string())
            }
            "get_status" => Some(json!({"type": "status", "data": st2.status()}).to_string()),
            _ => None,
        }
    });
}

fn serve_sse(mut s: TcpStream, st: &Arc<AppState>) {
    let Some((rx, latest)) = st.hub_sse.subscribe() else {
        return send(
            &mut s,
            &Resp::text(503, "Service Unavailable", "too many streaming clients"),
            false,
        );
    };
    let mut head = String::from("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\nX-Accel-Buffering: no\r\n");
    for (n, v) in http::SECURITY_HEADERS {
        head.push_str(&format!("{n}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
    if s.write_all(head.as_bytes()).is_err() {
        return;
    }
    if let Some(l) = latest {
        if s.write_all(format!("data: {l}\n\n").as_bytes()).is_err() {
            return;
        }
    }
    let mut idle = 0u32;
    while !st.stop.load(Ordering::Relaxed) {
        let out = match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(m) if m.is_empty() => continue,
            Ok(m) => {
                idle = 0;
                format!("data: {m}\n\n")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                idle += 1;
                if idle < 15 {
                    continue;
                }
                idle = 0;
                ": keepalive\n\n".to_string()
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        };
        if s.write_all(out.as_bytes()).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests;
