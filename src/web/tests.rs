use super::*;
use std::io::Read;

struct Harness {
    st: Arc<AppState>,
    addr: SocketAddr,
    token: String,
    dir: PathBuf,
    server: Option<Server>,
}

impl Harness {
    fn new(tag: &str, max_conn: usize) -> Self {
        let dir = std::env::temp_dir().join(format!("rvcsi-web-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ui = dir.join("ui");
        std::fs::create_dir_all(ui.join("sub")).unwrap();
        std::fs::write(ui.join("index.html"), "<html>home</html>").unwrap();
        std::fs::write(ui.join("model3d.html"), "<html>3d</html>").unwrap();
        std::fs::write(ui.join("app.js"), "console.log(1)").unwrap();
        std::fs::write(ui.join("sub/index.html"), "sub").unwrap();
        std::fs::write(ui.join(".hidden"), "nope").unwrap();
        std::fs::write(dir.join("secret.txt"), "TOP SECRET").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("secret.txt"), ui.join("leak.txt")).unwrap();
        let mut cfg = Config::default();
        cfg.http.bind = "127.0.0.1:0".into();
        cfg.http.ui_dir = ui.display().to_string();
        cfg.http.max_connections = max_conn;
        cfg.state_dir = dir.join("state").display().to_string();
        let token = auth::load_or_create_token(Path::new(&cfg.state_dir)).unwrap();
        let st = AppState::new(cfg, token.clone()).unwrap();
        let server = serve(Arc::clone(&st)).unwrap();
        Self {
            addr: server.addr,
            st,
            token,
            dir,
            server: Some(server),
        }
    }

    fn raw(&self, req: &str) -> (u16, String, String) {
        let mut c = TcpStream::connect(self.addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c.write_all(req.as_bytes()).unwrap();
        let mut out = Vec::new();
        let _ = c.read_to_end(&mut out);
        let text = String::from_utf8_lossy(&out).into_owned();
        let status = text
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        (status, head.to_string(), body.to_string())
    }

    fn get(&self, path: &str, extra: &str) -> (u16, String, String) {
        self.raw(&format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}\r\n"
        ))
    }
    fn authed_get(&self, path: &str) -> (u16, String, String) {
        self.get(path, &format!("Authorization: Bearer {}\r\n", self.token))
    }
    fn post(&self, path: &str, body: &str, extra: &str) -> (u16, String, String) {
        self.raw(&format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}\r\n{body}",
            self.token,
            body.len()
        ))
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.st.stop.store(true, Ordering::Relaxed);
        if let Some(s) = self.server.take() {
            s.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn report() -> Report {
    let mut e = crate::Engine::new(crate::dsp::DspConfig::default(), "edge2025").unwrap();
    e.push(crate::Sample {
        t: 1.0,
        link: "aa:bb:cc:dd:ee:ff".into(),
        kind: crate::Kind::Rssi,
        value: -55.0,
    });
    e.step(2.0)
}

fn ws_connect(h: &Harness, path: &str, extra: &str) -> Result<TcpStream, u16> {
    let mut c = TcpStream::connect(h.addr).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    c.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{extra}\r\n").as_bytes(),
    )
    .unwrap();
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        c.read_exact(&mut b).unwrap();
        head.push(b[0]);
    }
    let head = String::from_utf8(head).unwrap();
    let code: u16 = head.split(' ').nth(1).unwrap().parse().unwrap();
    if code == 101 {
        assert!(head.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
        Ok(c)
    } else {
        Err(code)
    }
}

fn ws_read_text(c: &mut TcpStream) -> String {
    loop {
        let mut h = [0u8; 2];
        c.read_exact(&mut h).unwrap();
        let mut len = usize::from(h[1] & 0x7F);
        if len == 126 {
            let mut b = [0u8; 2];
            c.read_exact(&mut b).unwrap();
            len = usize::from(u16::from_be_bytes(b));
        }
        let mut p = vec![0u8; len];
        c.read_exact(&mut p).unwrap();
        if h[0] & 0x0F == 1 {
            return String::from_utf8(p).unwrap();
        }
    }
}

fn ws_send_text(c: &mut TcpStream, text: &str) {
    let mask = [1u8, 2, 3, 4];
    let mut f = vec![0x81, 0x80 | text.len() as u8];
    f.extend_from_slice(&mask);
    f.extend(text.bytes().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    c.write_all(&f).unwrap();
}

#[test]
fn public_routes_and_static_serving() {
    let h = Harness::new("static", 24);
    assert_eq!(h.get("/healthz", "").0, 200);
    for p in ["/health", "/health/live", "/health/ready"] {
        let (c, _, b) = h.get(p, "");
        assert_eq!((c, b.as_str()), (200, r#"{"status":"ok"}"#), "{p}");
    }
    let (code, head, body) = h.get("/", "");
    assert_eq!((code, body.as_str()), (200, "<html>home</html>"));
    assert!(head.contains("text/html") && head.contains("X-Content-Type-Options: nosniff"));
    assert_eq!(
        h.get("/model3d", "").2,
        "<html>3d</html>",
        "extensionless page lookup"
    );
    assert_eq!(h.get("/sub", "").2, "sub");
    assert!(h.get("/app.js", "").1.contains("text/javascript"));
    assert_eq!(h.get("/nope", "").0, 404);
    let (c, head, body) = h.raw("HEAD /app.js HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(c == 200 && body.is_empty() && head.contains("Content-Length: 14"));
}

#[test]
fn path_traversal_hidden_files_and_symlinks_are_refused() {
    let h = Harness::new("trav", 24);
    for p in [
        "/../secret.txt",
        "/%2e%2e/secret.txt",
        "/..%2fsecret.txt",
        "/sub/../../secret.txt",
        "/.hidden",
        "/%2ehidden",
        "/a%5c..%5csecret.txt",
        "/leak.txt",
    ] {
        let (code, _, body) = h.get(p, "");
        assert_ne!(code, 200, "{p} must not be served");
        assert!(!body.contains("TOP SECRET"), "{p} leaked");
    }
}

#[test]
fn dns_rebinding_hosts_are_refused() {
    let h = Harness::new("host", 24);
    for host in [
        "evil.example.com",
        "127.0.0.1.evil.com",
        "evil.com:8765",
        "",
    ] {
        let (code, _, _) = h.raw(&format!("GET /healthz HTTP/1.1\r\nHost: {host}\r\n\r\n"));
        assert_eq!(code, 421, "host {host:?}");
    }
    assert_eq!(
        h.raw("GET /healthz HTTP/1.1\r\nHost: [::1]:8765\r\n\r\n").0,
        200
    );
    assert_eq!(
        h.raw("GET /healthz HTTP/1.1\r\nHost: 192.168.1.5:8765\r\n\r\n")
            .0,
        200
    );
    assert_eq!(
        h.raw("GET /healthz HTTP/1.1\r\n\r\n").0,
        421,
        "missing Host"
    );
}

#[test]
fn api_requires_credentials_and_accepts_bearer_or_cookie() {
    let h = Harness::new("auth", 24);
    assert_eq!(h.get("/api/v1/status", "").0, 401);
    assert_eq!(
        h.get("/api/v1/status", "Authorization: Bearer wrong\r\n").0,
        401
    );
    assert_eq!(
        h.get("/api/v1/map", "Cookie: rvcsi_session=wrong\r\n").0,
        401
    );
    let (c, _, body) = h.authed_get("/api/v1/status");
    assert_eq!(c, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (v["status"].as_str(), v["source_state"].as_str()),
        (Some("ok"), Some("stale"))
    );
    let (c, head, _) = h.get(&format!("/login?token={}", h.token), "");
    assert_eq!(c, 303);
    let cookie = head
        .lines()
        .find(|l| l.starts_with("Set-Cookie:"))
        .unwrap()
        .to_string();
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
    let kv = cookie
        .trim_start_matches("Set-Cookie: ")
        .split(';')
        .next()
        .unwrap();
    assert_eq!(h.get("/api/v1/info", &format!("Cookie: {kv}\r\n")).0, 200);
    assert_eq!(h.get("/api/v1/nope", &format!("Cookie: {kv}\r\n")).0, 404);
}

#[test]
fn brute_force_is_throttled() {
    let h = Harness::new("brute", 24);
    for _ in 0..10 {
        assert_eq!(h.get("/login?token=wrong", "").0, 401);
    }
    assert_eq!(h.get("/login?token=wrong", "").0, 429);
    assert_eq!(h.get(&format!("/login?token={}", h.token), "").0, 429);
    assert_eq!(h.authed_get("/api/v1/status").0, 429);
    assert_eq!(h.get("/healthz", "").0, 200, "public routes unaffected");
}

#[test]
fn state_changing_requests_resist_csrf() {
    let h = Harness::new("csrf", 24);
    let body = r#"{"kind":"bed","x":1,"z":2}"#;
    assert_eq!(h.post("/api/v1/map/object", body, "").0, 200);
    assert_eq!(
        h.post("/api/v1/map/object", body, "Origin: http://evil.com\r\n")
            .0,
        403
    );
    assert_eq!(
        h.post("/api/v1/map/object", body, "Origin: http://127.0.0.1\r\n")
            .0,
        200,
        "same-origin allowed"
    );
    let (c, _, _) = h.raw(&format!(
        "POST /api/v1/map/object HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
        h.token,
        body.len()
    ));
    assert_eq!(c, 415, "simple-form content types refused");
    assert_eq!(h.post("/api/v1/map/object", "{bad", "").0, 400);
    assert_eq!(
        h.raw("POST /index.html HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n")
            .0,
        405
    );
}

#[test]
fn map_api_roundtrip() {
    let h = Harness::new("map", 24);
    let (_, _, b) = h.post(
        "/api/v1/map/object",
        r#"{"kind":"bed","x":1.5,"z":-2,"yaw":0.25}"#,
        "",
    );
    let id = serde_json::from_str::<Value>(&b).unwrap()["id"]
        .as_u64()
        .unwrap();
    let (_, _, b) = h.post(
        "/api/v1/map/object",
        &format!(r#"{{"id":{id},"kind":"bed","x":4,"z":4,"yaw":0}}"#),
        "",
    );
    assert_eq!(serde_json::from_str::<Value>(&b).unwrap()["id"], id);
    let m: Value = serde_json::from_str(&h.authed_get("/api/v1/map").2).unwrap();
    assert_eq!(
        (
            m["objects"].as_array().unwrap().len(),
            m["objects"][0]["x"].clone()
        ),
        (1, json!(4.0))
    );
    assert_eq!(
        h.post("/api/v1/map/object", r#"{"kind":"nuke","x":0,"z":0}"#, "")
            .0,
        400
    );
    assert_eq!(
        h.post(
            "/api/v1/map/object/delete",
            &format!(r#"{{"id":{id}}}"#),
            ""
        )
        .0,
        200
    );
    assert_eq!(h.post("/api/v1/map/reset", "{}", "").0, 200);
    assert!(
        serde_json::from_str::<Value>(&h.authed_get("/api/v1/map").2).unwrap()["objects"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn oversized_and_malformed_requests_get_clean_errors() {
    let h = Harness::new("limits", 24);
    let big = "a".repeat(40_000);
    assert_eq!(
        h.raw(&format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nX: {big}\r\n\r\n"
        ))
        .0,
        413
    );
    assert_eq!(
        h.raw(
            "POST /api/v1/map/object HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 99999\r\n\r\n"
        )
        .0,
        413
    );
    assert_eq!(h.raw("GARBAGE\r\n\r\n").0, 400);
    assert_eq!(h.raw("DELETE / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").0, 405);
    assert_eq!(
        h.raw("POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\n\r\n")
            .0,
        501
    );
}

#[test]
fn connection_cap_and_slowloris_do_not_starve_the_node() {
    let h = Harness::new("cap", 2);
    let idle1 = TcpStream::connect(h.addr).unwrap();
    let idle2 = TcpStream::connect(h.addr).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        h.get("/healthz", "").0,
        503,
        "third connection is shed while two are held open"
    );
    drop((idle1, idle2));
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        h.get("/healthz", "").0,
        200,
        "capacity returns once idle peers leave"
    );
}

#[test]
fn websocket_requires_auth_origin_and_valid_handshake() {
    let h = Harness::new("wsauth", 24);
    assert_eq!(ws_connect(&h, "/ws/sensing", "").err(), Some(401));
    assert_eq!(
        ws_connect(&h, "/ws/sensing?ticket=forged", "").err(),
        Some(401)
    );
    assert_eq!(
        ws_connect(
            &h,
            "/ws/sensing",
            &format!(
                "Authorization: Bearer {}\r\nOrigin: http://evil.com\r\n",
                h.token
            )
        )
        .err(),
        Some(403)
    );
    assert!(ws_connect(
        &h,
        "/ws/sensing",
        &format!(
            "Authorization: Bearer {}\r\nOrigin: http://127.0.0.1\r\n",
            h.token
        )
    )
    .is_ok());
}

#[test]
fn websocket_streams_frames_with_single_use_ticket() {
    let h = Harness::new("wsflow", 24);
    h.st.publish(&report()); // late joiners get the latest frame immediately
    let (c, _, b) = h.post("/api/v1/ws-ticket", "", "");
    assert_eq!(c, 200);
    let ticket = serde_json::from_str::<Value>(&b).unwrap()["ticket"]
        .as_str()
        .unwrap()
        .to_string();
    let mut ws = ws_connect(&h, &format!("/ws/sensing?ticket={ticket}"), "").unwrap();
    let first: Value = serde_json::from_str(&ws_read_text(&mut ws)).unwrap();
    assert_eq!(first["type"], "sensing_update");
    assert_eq!(
        ws_connect(&h, &format!("/ws/sensing?ticket={ticket}"), "").err(),
        Some(401),
        "ticket is single use"
    );
    h.st.publish(&report());
    let next: Value = serde_json::from_str(&ws_read_text(&mut ws)).unwrap();
    assert_eq!(next["nodes"][0]["rssi_dbm"], -55.0);
    ws_send_text(&mut ws, r#"{"type":"ping"}"#);
    let pong: Value = serde_json::from_str(&ws_read_text(&mut ws)).unwrap();
    assert_eq!(pong["type"], "pong");
    ws_send_text(&mut ws, r#"{"type":"get_status"}"#);
    let status: Value = serde_json::from_str(&ws_read_text(&mut ws)).unwrap();
    assert_eq!(status["data"]["node"], "edge2025");
    // an unmasked client frame is a protocol error and closes the session
    ws.write_all(&[0x81, 0x02, b'h', b'i']).unwrap();
    let mut hdr = [0u8; 2];
    ws.read_exact(&mut hdr).unwrap();
    assert_eq!(hdr[0] & 0x0F, 8, "server answers with a close frame");
}

#[test]
fn sse_delivers_latest_then_live_events() {
    let h = Harness::new("sse", 24);
    h.st.publish(&report());
    let mut c = TcpStream::connect(h.addr).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    c.write_all(
        format!(
            "GET /events HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\n\r\n",
            h.token
        )
        .as_bytes(),
    )
    .unwrap();
    let mut seen = String::new();
    let mut buf = [0u8; 2048];
    while seen.matches("data: ").count() < 1 {
        let n = c.read(&mut buf).unwrap();
        seen.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    assert!(seen.contains("text/event-stream"));
    h.st.publish(&report());
    while seen.matches("data: ").count() < 2 {
        let n = c.read(&mut buf).unwrap();
        seen.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    let line = seen.lines().rfind(|l| l.starts_with("data: ")).unwrap();
    let v: Value = serde_json::from_str(&line[6..]).unwrap();
    assert_eq!(v["presence"], false);
    assert!(v["map"]["phone_estimate"].is_null());
    let (c, _, _) = h.get("/events", "");
    assert_eq!(c, 401);
}

#[test]
fn source_state_reflects_real_vs_synthetic_vs_stale() {
    let h = Harness::new("state", 24);
    let status =
        |h: &Harness| serde_json::from_str::<Value>(&h.authed_get("/api/v1/status").2).unwrap();
    assert_eq!(status(&h)["source_state"], "stale");
    let hl = Arc::new(Health::new("proc_wireless:wlan0".into()));
    hl.samples.store(5, Ordering::Relaxed);
    hl.last_ok_unix_ms
        .store(crate::sample::unix_ms(), Ordering::Relaxed);
    h.st.healths.lock().unwrap().push(hl);
    assert_eq!(status(&h)["source_state"], "live_verified");
    h.st.synthetic_only.store(true, Ordering::Relaxed);
    let s = status(&h);
    assert_eq!(
        (s["source_state"].as_str(), s["source"].as_str()),
        (Some("synthetic"), Some("simulated"))
    );
}

#[test]
fn host_and_static_helpers() {
    assert!(
        host_ok("localhost")
            && host_ok("LOCALHOST:8765")
            && host_ok("10.0.0.2:80")
            && host_ok("[fe80::1]:80")
            && host_ok("[::1]")
    );
    assert!(
        !host_ok("example.com")
            && !host_ok("[::1")
            && !host_ok("")
            && !host_ok("localhost.evil.com")
    );
}
