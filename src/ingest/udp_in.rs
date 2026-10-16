//! Local UDP bridge for data the stock OS will not hand to Termux (Wi-Fi RTT from a
//! companion app, CSI-derived scalars from a rooted path, ESP32 nodes).
//!
//! Datagram = one UTF-8 JSON object:
//!   {"link":"esp32-a","rssi":-52}            -> Kind::Rssi (dBm)
//!   {"link":"ap1","rtt_mm":3120}             -> Kind::RttMm
//!   {"link":"n1","csi_amp":[12.1,11.8,...]}  -> Kind::Csi (mean amplitude)
//!   {"link":"n1","csi":12.4}                 -> Kind::Csi
//! Timestamps are taken at receipt; sender clocks are never trusted.

use super::Source;
use crate::sample::{now, Kind, Sample};
use serde_json::Value;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

const MAX_DGRAM: usize = 2048;
const MAX_CSI: usize = 1024;
/// Samples accepted per second across all senders (token bucket).
const RATE_LIMIT: f64 = 2000.0;

pub fn parse_datagram(buf: &[u8]) -> Result<(String, Kind, f32), String> {
    let v: Value = serde_json::from_slice(buf).map_err(|e| format!("json: {e}"))?;
    let link = v
        .get("link")
        .and_then(Value::as_str)
        .ok_or("missing link")?;
    if link.is_empty()
        || link.len() > 64
        || !link
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'.' | b'_' | b'-'))
    {
        return Err("invalid link id".into());
    }
    let num = |k: &str| v.get(k).and_then(Value::as_f64).filter(|x| x.is_finite());
    let (kind, val) = if let Some(r) = num("rssi") {
        if !(-126.0..=-1.0).contains(&r) {
            return Err("rssi out of range".into());
        }
        (Kind::Rssi, r)
    } else if let Some(d) = num("rtt_mm") {
        if !(0.0..=1.0e6).contains(&d) {
            return Err("rtt_mm out of range".into());
        }
        (Kind::RttMm, d)
    } else if let Some(c) = num("csi") {
        (Kind::Csi, c)
    } else if let Some(arr) = v.get("csi_amp").and_then(Value::as_array) {
        if arr.is_empty() || arr.len() > MAX_CSI {
            return Err("csi_amp length".into());
        }
        let mut s = 0f64;
        for x in arr {
            s += x
                .as_f64()
                .filter(|x| x.is_finite())
                .ok_or("csi_amp non-numeric")?;
        }
        (Kind::Csi, s / arr.len() as f64)
    } else {
        return Err("no rssi/rtt_mm/csi/csi_amp".into());
    };
    Ok((link.to_string(), kind, val as f32))
}

#[derive(Debug)]
pub struct UdpIngest {
    sock: UdpSocket,
    buf: Vec<u8>,
    tokens: f64,
    refill: Instant,
    pub rejected: u64,
}

impl UdpIngest {
    pub fn bind(addr: &str) -> Result<Self, String> {
        let sock = UdpSocket::bind(addr).map_err(|e| format!("udp_ingest bind {addr}: {e}"))?;
        sock.set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            sock,
            buf: vec![0; MAX_DGRAM + 1],
            tokens: RATE_LIMIT,
            refill: Instant::now(),
            rejected: 0,
        })
    }
    pub fn local_addr(&self) -> Result<std::net::SocketAddr, String> {
        self.sock.local_addr().map_err(|e| e.to_string())
    }
}

impl Source for UdpIngest {
    fn name(&self) -> String {
        format!(
            "udp_ingest:{}",
            self.local_addr().map(|a| a.to_string()).unwrap_or_default()
        )
    }
    fn period(&self) -> Duration {
        Duration::ZERO // poll() blocks on the socket
    }
    fn poll(&mut self, out: &mut Vec<Sample>) -> Result<(), String> {
        match self.sock.recv(&mut self.buf) {
            Ok(n) if n > MAX_DGRAM => {
                self.rejected += 1;
                Ok(())
            }
            Ok(n) => {
                let el = self.refill.elapsed().as_secs_f64();
                self.refill = Instant::now();
                self.tokens = (self.tokens + el * RATE_LIMIT).min(RATE_LIMIT);
                if self.tokens < 1.0 {
                    self.rejected += 1;
                    return Ok(());
                }
                self.tokens -= 1.0;
                match parse_datagram(&self.buf[..n]) {
                    Ok((link, kind, value)) => out.push(Sample {
                        t: now(),
                        link,
                        kind,
                        value,
                    }),
                    Err(_) => self.rejected += 1,
                }
                Ok(())
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(())
            }
            Err(e) => Err(format!("udp recv: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_kind() {
        assert_eq!(
            parse_datagram(br#"{"link":"a","rssi":-50}"#).unwrap(),
            ("a".into(), Kind::Rssi, -50.0)
        );
        assert_eq!(
            parse_datagram(br#"{"link":"a","rtt_mm":2500}"#).unwrap().1,
            Kind::RttMm
        );
        let (_, k, v) = parse_datagram(br#"{"link":"a","csi_amp":[1,2,3]}"#).unwrap();
        assert_eq!((k, v), (Kind::Csi, 2.0));
    }

    #[test]
    fn rejects_malformed_and_hostile_input() {
        for bad in [
            &br#"{"rssi":-50}"#[..],
            br#"{"link":"","rssi":-50}"#,
            br#"{"link":"a b","rssi":-50}"#,
            br#"{"link":"a/../b","rssi":-50}"#,
            br#"{"link":"a","rssi":10}"#,
            br#"{"link":"a","rssi":"-50"}"#,
            br#"{"link":"a","csi_amp":[]}"#,
            br#"{"link":"a","csi_amp":["x"]}"#,
            br#"{"link":"a","rtt_mm":-1}"#,
            b"\xff\xfe",
            b"[]",
            b"",
        ] {
            assert!(
                parse_datagram(bad).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        let long = format!(r#"{{"link":"{}","rssi":-50}}"#, "x".repeat(65));
        assert!(parse_datagram(long.as_bytes()).is_err());
    }

    #[test]
    fn socket_roundtrip_and_oversize_drop() {
        let mut u = UdpIngest::bind("127.0.0.1:0").unwrap();
        let addr = u.local_addr().unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        tx.send_to(br#"{"link":"esp32-a","rssi":-61}"#, addr)
            .unwrap();
        tx.send_to(&vec![b'x'; 3000], addr).unwrap();
        tx.send_to(b"garbage", addr).unwrap();
        let mut out = Vec::new();
        for _ in 0..5 {
            u.poll(&mut out).unwrap();
        }
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].link, "esp32-a");
        assert_eq!(u.rejected, 2);
    }
}
