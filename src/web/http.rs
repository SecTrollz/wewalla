//! A deliberately small, strict HTTP/1.1 request reader. One request per connection
//! (`Connection: close`), hard limits on every dimension, and rejection of the
//! ambiguities that enable request smuggling.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

pub const MAX_HEAD: usize = 16 * 1024;
pub const MAX_BODY: usize = 16 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_TARGET: usize = 2048;

#[derive(Debug, PartialEq, Eq)]
pub enum HttpErr {
    Bad(&'static str),
    TooLarge,
    NotImplemented(&'static str),
    Timeout,
    Closed,
}

impl HttpErr {
    pub fn status(&self) -> (u16, &'static str) {
        match self {
            HttpErr::Bad(_) => (400, "Bad Request"),
            HttpErr::TooLarge => (413, "Payload Too Large"),
            HttpErr::NotImplemented(_) => (501, "Not Implemented"),
            HttpErr::Timeout => (408, "Request Timeout"),
            HttpErr::Closed => (400, "Bad Request"),
        }
    }
}

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.header("cookie")?
            .split(';')
            .filter_map(|kv| kv.trim().split_once('='))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v)
    }
    pub fn query_get(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    pub fn is_upgrade(&self) -> bool {
        self.header("upgrade")
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
            && self.header("connection").is_some_and(|v| {
                v.split(',')
                    .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
            })
    }
    #[cfg(test)]
    pub fn for_test(method: &str, path: &str, headers: &[(&str, &str)]) -> Self {
        Self {
            method: method.into(),
            path: path.into(),
            query: vec![],
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_ascii_lowercase(), (*v).to_string()))
                .collect(),
            body: vec![],
        }
    }
}

pub fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(h, 16).ok()?);
                i += 3;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    if out.contains(&0) {
        return None;
    }
    String::from_utf8(out).ok()
}

fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

pub fn parse_head(head: &str) -> Result<Request, HttpErr> {
    let mut lines = head.split("\r\n");
    let first = lines.next().ok_or(HttpErr::Bad("empty"))?;
    let mut parts = first.split(' ');
    let (method, target, version) = (parts.next(), parts.next(), parts.next());
    let (Some(method), Some(target), Some(version), None) = (method, target, version, parts.next())
    else {
        return Err(HttpErr::Bad("request line"));
    };
    if !is_token(method) || !matches!(version, "HTTP/1.1" | "HTTP/1.0") {
        return Err(HttpErr::Bad("method/version"));
    }
    if !target.starts_with('/')
        || target.len() > MAX_TARGET
        || target.bytes().any(|b| b < 0x21 || b == 0x7f)
    {
        return Err(HttpErr::Bad("target"));
    }
    let (raw_path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let path = percent_decode(raw_path).ok_or(HttpErr::Bad("path encoding"))?;
    let mut query = Vec::new();
    for kv in raw_query.split('&').filter(|s| !s.is_empty()).take(16) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        query.push((
            percent_decode(k).ok_or(HttpErr::Bad("query encoding"))?,
            percent_decode(v).ok_or(HttpErr::Bad("query encoding"))?,
        ));
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if headers.len() >= MAX_HEADERS {
            return Err(HttpErr::TooLarge);
        }
        if line.starts_with([' ', '\t']) {
            return Err(HttpErr::Bad("obsolete line folding"));
        }
        let (n, v) = line.split_once(':').ok_or(HttpErr::Bad("header"))?;
        if !is_token(n) || v.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f) {
            return Err(HttpErr::Bad("header chars"));
        }
        let n = n.to_ascii_lowercase();
        if matches!(
            n.as_str(),
            "host" | "content-length" | "authorization" | "origin" | "cookie" | "upgrade"
        ) && headers.iter().any(|(x, _)| *x == n)
        {
            return Err(HttpErr::Bad("duplicate header"));
        }
        headers.push((n, v.trim().to_string()));
    }
    Ok(Request {
        method: method.to_string(),
        path,
        query,
        headers,
        body: Vec::new(),
    })
}

pub fn read_request(s: &mut TcpStream, deadline: Instant) -> Result<Request, HttpErr> {
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        if buf.len() > MAX_HEAD {
            return Err(HttpErr::TooLarge);
        }
        let left = deadline
            .checked_duration_since(Instant::now())
            .ok_or(HttpErr::Timeout)?;
        s.set_read_timeout(Some(left.max(Duration::from_millis(1))))
            .map_err(|_| HttpErr::Closed)?;
        match s.read(&mut chunk) {
            Ok(0) => return Err(HttpErr::Closed),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(HttpErr::Timeout)
            }
            Err(_) => return Err(HttpErr::Closed),
        }
    };
    if head_end > MAX_HEAD {
        return Err(HttpErr::TooLarge);
    }
    let head = std::str::from_utf8(&buf[..head_end]).map_err(|_| HttpErr::Bad("non-utf8 head"))?;
    let mut req = parse_head(head)?;
    if req.header("transfer-encoding").is_some() {
        return Err(HttpErr::NotImplemented("transfer-encoding"));
    }
    let clen = match req.header("content-length") {
        None => 0,
        Some(v) if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) && v.len() <= 8 => v
            .parse::<usize>()
            .map_err(|_| HttpErr::Bad("content-length"))?,
        Some(_) => return Err(HttpErr::Bad("content-length")),
    };
    if clen > MAX_BODY {
        return Err(HttpErr::TooLarge);
    }
    let mut body = buf[head_end + 4..].to_vec();
    if body.len() > clen {
        return Err(HttpErr::Bad("pipelined data"));
    }
    while body.len() < clen {
        let left = deadline
            .checked_duration_since(Instant::now())
            .ok_or(HttpErr::Timeout)?;
        s.set_read_timeout(Some(left.max(Duration::from_millis(1))))
            .map_err(|_| HttpErr::Closed)?;
        match s.read(&mut chunk) {
            Ok(0) => return Err(HttpErr::Closed),
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(HttpErr::Timeout)
            }
            Err(_) => return Err(HttpErr::Closed),
        }
        if body.len() > clen {
            return Err(HttpErr::Bad("pipelined data"));
        }
    }
    req.body = body;
    Ok(req)
}

pub const SECURITY_HEADERS: &[(&str, &str)] = &[
    (
        "Content-Security-Policy",
        "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; \
         font-src 'self' data:; connect-src 'self' ws: wss:; worker-src 'self' blob:; object-src 'none'; base-uri 'none'; \
         form-action 'self'; frame-ancestors 'none'",
    ),
    ("X-Content-Type-Options", "nosniff"),
    ("Referrer-Policy", "no-referrer"),
    ("X-Frame-Options", "DENY"),
    ("Cross-Origin-Opener-Policy", "same-origin"),
    ("Cross-Origin-Resource-Policy", "same-origin"),
];

fn clean(v: &str) -> bool {
    !v.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
}

pub fn write_response(
    s: &mut TcpStream,
    status: u16,
    reason: &str,
    extra: &[(&str, &str)],
    body: &[u8],
    head_only: bool,
) -> std::io::Result<()> {
    let mut h = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (n, v) in SECURITY_HEADERS.iter().chain(extra.iter()) {
        debug_assert!(clean(n) && clean(v));
        if clean(n) && clean(v) {
            h.push_str(&format!("{n}: {v}\r\n"));
        }
    }
    h.push_str("\r\n");
    s.set_write_timeout(Some(Duration::from_secs(10)))?;
    s.write_all(h.as_bytes())?;
    if !head_only {
        s.write_all(body)?;
    }
    s.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        (c, l.accept().unwrap().0)
    }

    fn parse(raw: &str) -> Result<Request, HttpErr> {
        let (mut c, mut s) = pair();
        c.write_all(raw.as_bytes()).unwrap();
        drop(c);
        read_request(&mut s, Instant::now() + Duration::from_secs(2))
    }

    #[test]
    fn parses_a_normal_request() {
        let r = parse("POST /api/v1/map/object?x=1&y=a%20b HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 4\r\nCookie: a=b; rvcsi_session=tok\r\n\r\nbody").unwrap();
        assert_eq!(
            (r.method.as_str(), r.path.as_str()),
            ("POST", "/api/v1/map/object")
        );
        assert_eq!(r.query_get("y"), Some("a b"));
        assert_eq!(r.body, b"body");
        assert_eq!(r.cookie("rvcsi_session"), Some("tok"));
        assert_eq!(r.header("host"), Some("127.0.0.1"));
    }

    #[test]
    fn rejects_smuggling_and_malformed_input() {
        let cases = [
            "GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx",
            "GET / HTTP/1.1\r\nHost: a\r\n folded: x\r\n\r\n",
            "GET /a b HTTP/1.1\r\n\r\n",
            "GET http://evil/ HTTP/1.1\r\n\r\n",
            "GET /%00 HTTP/1.1\r\n\r\n",
            "GET /%zz HTTP/1.1\r\n\r\n",
            "GET / HTTP/2.0\r\n\r\n",
            "GET / HTTP/1.1\r\nBad Name: x\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: -1\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: 1\r\n\r\nxx",
        ];
        for c in cases {
            assert!(parse(c).is_err(), "should reject: {c:?}");
        }
        assert_eq!(
            parse("POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap_err(),
            HttpErr::NotImplemented("transfer-encoding")
        );
        assert_eq!(
            parse("POST / HTTP/1.1\r\nContent-Length: 999999\r\n\r\n").unwrap_err(),
            HttpErr::TooLarge
        );
    }

    #[test]
    fn oversized_head_and_header_flood_are_rejected() {
        let big = format!(
            "GET / HTTP/1.1\r\nX: {}\r\n\r\n",
            "a".repeat(MAX_HEAD + 100)
        );
        assert_eq!(parse(&big).unwrap_err(), HttpErr::TooLarge);
        let flood: String = (0..80).map(|i| format!("H{i}: v\r\n")).collect();
        assert_eq!(
            parse(&format!("GET / HTTP/1.1\r\n{flood}\r\n")).unwrap_err(),
            HttpErr::TooLarge
        );
    }

    #[test]
    fn slow_client_hits_the_deadline() {
        let (mut c, mut s) = pair();
        c.write_all(b"GET / HTTP/1.1\r\nHost: x").unwrap(); // never finishes
        let t = Instant::now();
        let e = read_request(&mut s, Instant::now() + Duration::from_millis(300)).unwrap_err();
        assert_eq!(e, HttpErr::Timeout);
        assert!(t.elapsed() < Duration::from_secs(2));
        drop(c);
    }

    #[test]
    fn upgrade_detection() {
        let r = Request::for_test(
            "GET",
            "/ws",
            &[
                ("upgrade", "WebSocket"),
                ("connection", "keep-alive, Upgrade"),
            ],
        );
        assert!(r.is_upgrade());
        assert!(!Request::for_test("GET", "/ws", &[("upgrade", "websocket")]).is_upgrade());
    }

    #[test]
    fn response_has_security_headers_and_length() {
        let (mut c, mut s) = pair();
        write_response(
            &mut s,
            200,
            "OK",
            &[("Content-Type", "text/plain")],
            b"hi",
            false,
        )
        .unwrap();
        drop(s);
        let mut out = String::new();
        c.read_to_string(&mut out).unwrap();
        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(
            out.contains("Content-Length: 2\r\n")
                && out.contains("X-Content-Type-Options: nosniff")
                && out.contains("frame-ancestors 'none'")
        );
        assert!(out.ends_with("\r\n\r\nhi"));
    }
}
