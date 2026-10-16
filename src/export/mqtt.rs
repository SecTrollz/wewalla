//! Minimal MQTT 3.1.1 publisher (QoS 0). Hand-rolled on purpose: the node only ever
//! needs CONNECT / PUBLISH / PINGREQ / DISCONNECT toward a local broker, and a ~200-line
//! audited client has a far smaller attack and dependency surface than an async stack.

use super::{ha, report_json};
use crate::config::MqttCfg;
use crate::engine::Report;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

const MAX_PACKET: usize = 256 * 1024;

fn put_str(buf: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    debug_assert!(b.len() <= u16::MAX as usize);
    buf.extend_from_slice(&(b.len() as u16).to_be_bytes());
    buf.extend_from_slice(b);
}

fn put_remaining(buf: &mut Vec<u8>, mut n: usize) {
    loop {
        let mut b = (n % 128) as u8;
        n /= 128;
        if n > 0 {
            b |= 0x80;
        }
        buf.push(b);
        if n == 0 {
            break;
        }
    }
}

fn frame(first: u8, body: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(body.len() + 5);
    p.push(first);
    put_remaining(&mut p, body.len());
    p.extend_from_slice(body);
    p
}

#[derive(Debug)]
pub struct Will<'a> {
    pub topic: &'a str,
    pub payload: &'a str,
}

pub fn encode_connect(
    client_id: &str,
    keepalive: u16,
    user: Option<&str>,
    pass: Option<&str>,
    will: Option<Will>,
) -> Vec<u8> {
    let mut b = Vec::new();
    put_str(&mut b, "MQTT");
    b.push(4); // protocol level 3.1.1
    let mut flags = 0x02; // clean session
    if will.is_some() {
        flags |= 0x04 | 0x20; // will + will-retain, QoS 0
    }
    if user.is_some() {
        flags |= 0x80;
    }
    if pass.is_some() && user.is_some() {
        flags |= 0x40;
    }
    b.push(flags);
    b.extend_from_slice(&keepalive.to_be_bytes());
    put_str(&mut b, client_id);
    if let Some(w) = &will {
        put_str(&mut b, w.topic);
        put_str(&mut b, w.payload);
    }
    if let Some(u) = user {
        put_str(&mut b, u);
        if let Some(p) = pass {
            put_str(&mut b, p);
        }
    }
    frame(0x10, &b)
}

pub fn encode_publish(topic: &str, payload: &[u8], retain: bool) -> Vec<u8> {
    let mut b = Vec::with_capacity(topic.len() + payload.len() + 2);
    put_str(&mut b, topic);
    b.extend_from_slice(payload);
    frame(0x30 | u8::from(retain), &b)
}

pub const PINGREQ: [u8; 2] = [0xC0, 0x00];
pub const DISCONNECT: [u8; 2] = [0xE0, 0x00];

#[derive(Debug, Default)]
pub struct MqttStatus {
    pub connected: AtomicBool,
    pub published: AtomicU64,
    pub connects: AtomicU64,
    pub dropped: AtomicU64,
}

#[derive(Debug)]
struct Conn {
    s: TcpStream,
}

impl Conn {
    fn open(cfg: &MqttCfg, node: &str, avail: &str) -> Result<Self, String> {
        let addr = (cfg.host.as_str(), cfg.port)
            .to_socket_addrs()
            .map_err(|e| format!("resolve {}: {e}", cfg.host))?
            .next()
            .ok_or("no address")?;
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .map_err(|e| format!("connect {addr}: {e}"))?;
        s.set_nodelay(true).ok();
        s.set_write_timeout(Some(Duration::from_secs(5))).ok();
        s.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let pkt = encode_connect(
            &format!("rvcsi-{node}"),
            cfg.keepalive_s,
            cfg.username.as_deref(),
            cfg.password.as_deref(),
            Some(Will {
                topic: avail,
                payload: "offline",
            }),
        );
        s.write_all(&pkt)
            .map_err(|e| format!("send CONNECT: {e}"))?;
        let mut ack = [0u8; 4];
        s.read_exact(&mut ack)
            .map_err(|e| format!("read CONNACK: {e}"))?;
        if ack[0] != 0x20 || ack[1] != 2 || ack[3] != 0 {
            return Err(format!("broker refused connection (CONNACK {:02x?})", ack));
        }
        s.set_read_timeout(Some(Duration::from_millis(1))).ok();
        Ok(Self { s })
    }

    fn send(&mut self, pkt: &[u8]) -> Result<(), String> {
        if pkt.len() > MAX_PACKET {
            return Err("packet too large".into());
        }
        self.s.write_all(pkt).map_err(|e| e.to_string())
    }

    /// Discard anything the broker sent (PINGRESP); report a closed socket.
    fn alive(&mut self) -> bool {
        let mut b = [0u8; 256];
        match self.s.read(&mut b) {
            Ok(0) => false,
            Ok(_) => true,
            Err(e) => matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ),
        }
    }
}

/// Start the exporter thread. Dropping the returned sender shuts it down cleanly.
pub fn spawn(
    cfg: MqttCfg,
    node: String,
    device_name: String,
) -> (SyncSender<Report>, Arc<MqttStatus>, JoinHandle<()>) {
    let (tx, rx) = sync_channel::<Report>(4);
    let status = Arc::new(MqttStatus::default());
    let st = Arc::clone(&status);
    let handle = std::thread::Builder::new()
        .name("mqtt".into())
        .spawn(move || {
            let avail = ha::availability_topic(&cfg, &node);
            let state = ha::state_topic(&cfg, &node);
            let disco = ha::discovery(&cfg, &node, &device_name);
            let ping_every = Duration::from_secs(u64::from(cfg.keepalive_s / 2).max(1));
            let mut backoff = Duration::from_secs(1);
            'outer: loop {
                let mut conn = match Conn::open(&cfg, &node, &avail) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("[mqtt] {e}; retry in {backoff:?}");
                        // While down: discard reports (state is only ever the latest), watch for shutdown.
                        let end = std::time::Instant::now() + backoff;
                        while std::time::Instant::now() < end {
                            match rx.recv_timeout(Duration::from_millis(200)) {
                                Err(RecvTimeoutError::Disconnected) => return,
                                Ok(_) => {
                                    st.dropped.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(RecvTimeoutError::Timeout) => {}
                            }
                        }
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                        continue;
                    }
                };
                backoff = Duration::from_secs(1);
                st.connects.fetch_add(1, Ordering::Relaxed);
                let mut ok = conn.send(&encode_publish(&avail, b"online", true)).is_ok();
                for (topic, payload) in &disco {
                    ok &= conn
                        .send(&encode_publish(topic, payload.as_bytes(), true))
                        .is_ok();
                }
                if !ok {
                    eprintln!("[mqtt] failed publishing discovery");
                    continue;
                }
                st.connected.store(true, Ordering::Relaxed);
                eprintln!(
                    "[mqtt] connected; {} discovery entities published",
                    disco.len()
                );
                loop {
                    match rx.recv_timeout(ping_every) {
                        Ok(rep) => {
                            let body = report_json(&rep, MAX_PACKET / 2);
                            if conn
                                .send(&encode_publish(&state, body.as_bytes(), false))
                                .is_err()
                            {
                                break;
                            }
                            st.published.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            if conn.send(&PINGREQ).is_err() {
                                break;
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => {
                            let _ = conn.send(&encode_publish(&avail, b"offline", true));
                            let _ = conn.send(&DISCONNECT);
                            st.connected.store(false, Ordering::Relaxed);
                            break 'outer;
                        }
                    }
                    if !conn.alive() {
                        break;
                    }
                }
                st.connected.store(false, Ordering::Relaxed);
                eprintln!("[mqtt] connection lost; reconnecting");
            }
        })
        .expect("spawn mqtt thread");
    (tx, status, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::DspConfig;
    use crate::engine::Engine;
    use std::net::TcpListener;
    use std::sync::mpsc;

    #[test]
    fn remaining_length_boundaries() {
        for (n, enc) in [
            (0usize, vec![0x00]),
            (127, vec![0x7F]),
            (128, vec![0x80, 0x01]),
            (16383, vec![0xFF, 0x7F]),
            (16384, vec![0x80, 0x80, 0x01]),
        ] {
            let mut b = Vec::new();
            put_remaining(&mut b, n);
            assert_eq!(b, enc, "n={n}");
        }
    }

    #[test]
    fn connect_packet_layout() {
        let p = encode_connect(
            "cid",
            30,
            Some("u"),
            Some("p"),
            Some(Will {
                topic: "t",
                payload: "offline",
            }),
        );
        assert_eq!(p[0], 0x10);
        assert_eq!(&p[2..9], &[0, 4, b'M', b'Q', b'T', b'T', 4]);
        assert_eq!(p[9], 0x02 | 0x04 | 0x20 | 0x80 | 0x40);
        assert_eq!(&p[10..12], &30u16.to_be_bytes());
        assert_eq!(p[1] as usize, p.len() - 2);
        let no_auth = encode_connect("cid", 30, None, Some("orphan-pass"), None);
        assert_eq!(
            no_auth[9], 0x02,
            "password without username is not representable in 3.1.1"
        );
    }

    #[test]
    fn publish_packet_layout_and_retain_bit() {
        let p = encode_publish("a/b", b"xyz", true);
        assert_eq!(p, vec![0x31, 8, 0, 3, b'a', b'/', b'b', b'x', b'y', b'z']);
        assert_eq!(encode_publish("a", b"", false)[0], 0x30);
    }

    #[derive(Debug)]
    enum Ev {
        Connect {
            will_topic: String,
            user: Option<String>,
        },
        Publish {
            topic: String,
            payload: String,
            retain: bool,
        },
        Disconnect,
    }

    fn read_pkt(s: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
        let mut h = [0u8; 1];
        s.read_exact(&mut h).ok()?;
        let (mut len, mut mult) = (0usize, 1usize);
        loop {
            let mut b = [0u8; 1];
            s.read_exact(&mut b).ok()?;
            len += (b[0] & 0x7F) as usize * mult;
            mult *= 128;
            if b[0] & 0x80 == 0 {
                break;
            }
        }
        let mut body = vec![0u8; len];
        s.read_exact(&mut body).ok()?;
        Some((h[0], body))
    }

    fn s16(b: &[u8]) -> (String, &[u8]) {
        let n = u16::from_be_bytes([b[0], b[1]]) as usize;
        (
            String::from_utf8_lossy(&b[2..2 + n]).into_owned(),
            &b[2 + n..],
        )
    }

    /// Mock broker: records events; closes the first connection after `drop_first_after` publishes.
    fn broker(drop_first_after: Option<usize>) -> (u16, mpsc::Receiver<Ev>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut n_conn = 0;
            for conn in l.incoming() {
                let mut s = conn.unwrap();
                n_conn += 1;
                let tx = tx.clone();
                let (t, _) = (read_pkt(&mut s).unwrap(), ());
                assert_eq!(t.0, 0x10);
                let body = &t.1[10..]; // after proto(6)+level+flags+keepalive
                let flags = t.1[7];
                let (_cid, rest) = s16(body);
                let (will_topic, rest) = if flags & 0x04 != 0 {
                    let (wt, r) = s16(rest);
                    let (_wp, r) = s16(r);
                    (wt, r)
                } else {
                    (String::new(), rest)
                };
                let user = (flags & 0x80 != 0).then(|| s16(rest).0);
                tx.send(Ev::Connect { will_topic, user }).unwrap();
                s.write_all(&[0x20, 2, 0, 0]).unwrap();
                let mut published = 0;
                while let Some((h, b)) = read_pkt(&mut s) {
                    match h >> 4 {
                        3 => {
                            let (topic, payload) = s16(&b);
                            tx.send(Ev::Publish {
                                topic,
                                payload: String::from_utf8_lossy(payload).into_owned(),
                                retain: h & 1 == 1,
                            })
                            .unwrap();
                            published += 1;
                            if n_conn == 1 && drop_first_after == Some(published) {
                                break;
                            }
                        }
                        14 => {
                            tx.send(Ev::Disconnect).unwrap();
                            break;
                        }
                        _ => {}
                    }
                }
            }
        });
        (port, rx)
    }

    fn cfg(port: u16) -> MqttCfg {
        MqttCfg {
            port,
            username: Some("ha".into()),
            password: Some("pw".into()),
            ..MqttCfg::default()
        }
    }

    #[test]
    fn full_session_discovery_state_and_clean_shutdown() {
        let (port, rx) = broker(None);
        let (tx, status, h) = spawn(cfg(port), "edge2025".into(), "Motorola Edge (2025)".into());
        let wait = Duration::from_secs(5);
        match rx.recv_timeout(wait).unwrap() {
            Ev::Connect { will_topic, user } => {
                assert_eq!(will_topic, "rvcsi/edge2025/status");
                assert_eq!(user.as_deref(), Some("ha"));
            }
            e => panic!("{e:?}"),
        }
        let mut seen = Vec::new();
        for _ in 0..10 {
            match rx.recv_timeout(wait).unwrap() {
                Ev::Publish {
                    topic,
                    payload,
                    retain,
                } => {
                    assert!(retain, "{topic} must be retained");
                    seen.push((topic, payload));
                }
                e => panic!("{e:?}"),
            }
        }
        assert_eq!(
            seen[0],
            ("rvcsi/edge2025/status".to_string(), "online".to_string())
        );
        assert_eq!(
            seen.iter().filter(|(t, _)| t.ends_with("/config")).count(),
            9
        );
        let rep = Engine::new(DspConfig::default(), "edge2025")
            .unwrap()
            .step(1.0);
        tx.send(rep).unwrap();
        match rx.recv_timeout(wait).unwrap() {
            Ev::Publish {
                topic,
                payload,
                retain,
            } => {
                assert_eq!(topic, "rvcsi/edge2025/state");
                assert!(!retain);
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&payload).unwrap()["node"],
                    "edge2025"
                );
            }
            e => panic!("{e:?}"),
        }
        drop(tx);
        match rx.recv_timeout(wait).unwrap() {
            Ev::Publish {
                topic,
                payload,
                retain,
            } => assert_eq!(
                (topic.as_str(), payload.as_str(), retain),
                ("rvcsi/edge2025/status", "offline", true)
            ),
            e => panic!("{e:?}"),
        }
        assert!(matches!(rx.recv_timeout(wait).unwrap(), Ev::Disconnect));
        h.join().unwrap();
        assert_eq!(status.published.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn reconnects_and_republishes_discovery_after_broker_drop() {
        let (port, rx) = broker(Some(10)); // drop the first connection right after online+9 discovery
        let (tx, status, h) = spawn(cfg(port), "n".into(), "d".into());
        let mut connects = 0;
        let mut configs = 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        let mut settle: Option<std::time::Instant> = None;
        while std::time::Instant::now() < deadline
            && settle.is_none_or(|t| std::time::Instant::now() < t)
        {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(200)) {
                match ev {
                    Ev::Connect { .. } => {
                        connects += 1;
                        if connects == 2 {
                            settle = Some(std::time::Instant::now() + Duration::from_millis(600));
                        }
                    }
                    Ev::Publish { topic, .. } if topic.ends_with("/config") => configs += 1,
                    _ => {}
                }
            }
            // keep feeding reports so the dead socket is noticed
            let _ = tx.try_send(Engine::new(DspConfig::default(), "n").unwrap().step(1.0));
        }
        assert_eq!(connects, 2, "exporter must reconnect");
        assert_eq!(
            configs, 18,
            "discovery must be re-announced on every connect"
        );
        drop(tx);
        h.join().unwrap();
        assert!(status.connects.load(Ordering::Relaxed) >= 2);
    }

    #[test]
    fn unreachable_broker_never_blocks_producer_and_shuts_down() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l); // nothing listening
        let (tx, status, h) = spawn(cfg(port), "n".into(), "d".into());
        let t = std::time::Instant::now();
        for _ in 0..50 {
            let _ = tx.try_send(Engine::new(DspConfig::default(), "n").unwrap().step(1.0));
        }
        assert!(t.elapsed() < Duration::from_millis(500));
        drop(tx);
        h.join().unwrap();
        assert!(!status.connected.load(Ordering::Relaxed));
    }
}
