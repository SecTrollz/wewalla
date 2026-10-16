use super::report_json;
use crate::engine::Report;
use std::net::{SocketAddr, UdpSocket};

/// One JSON datagram per report to each configured target. Fire-and-forget: a dead
/// listener must never slow the DSP loop, so send errors are counted, not propagated.
#[derive(Debug)]
pub struct UdpExporter {
    sock: UdpSocket,
    targets: Vec<SocketAddr>,
    pub sent: u64,
    pub errors: u64,
}

/// Stays under a 1500-byte MTU minus IP/UDP headers.
const MAX_DGRAM: usize = 1400;

impl UdpExporter {
    pub fn new(targets: &[String]) -> Result<Self, String> {
        let targets = targets
            .iter()
            .map(|t| {
                t.parse::<SocketAddr>()
                    .map_err(|e| format!("udp target {t:?}: {e}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("udp export bind: {e}"))?;
        sock.set_broadcast(true).map_err(|e| e.to_string())?;
        sock.set_nonblocking(true).map_err(|e| e.to_string())?;
        Ok(Self {
            sock,
            targets,
            sent: 0,
            errors: 0,
        })
    }

    pub fn send(&mut self, r: &Report) {
        let body = report_json(r, MAX_DGRAM);
        for t in &self.targets {
            match self.sock.send_to(body.as_bytes(), t) {
                Ok(_) => self.sent += 1,
                Err(_) => self.errors += 1,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::DspConfig;
    use crate::engine::Engine;

    #[test]
    fn delivers_valid_json_and_survives_dead_target() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let live = rx.local_addr().unwrap().to_string();
        let mut ex = UdpExporter::new(&[live, "127.0.0.1:9".into()]).unwrap();
        let rep = Engine::new(DspConfig::default(), "n1").unwrap().step(1.0);
        ex.send(&rep);
        let mut buf = [0u8; 2048];
        let n = rx.recv(&mut buf).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf[..n]).unwrap();
        assert_eq!(v["node"], "n1");
        assert_eq!(v["mode"], "no-data");
        assert_eq!(ex.sent, 2);
    }

    #[test]
    fn rejects_bad_targets() {
        assert!(UdpExporter::new(&["not-an-addr".into()]).is_err());
    }

    #[test]
    fn oversized_reports_drop_link_detail_not_the_report() {
        let mut e = Engine::new(DspConfig::default(), "n1").unwrap();
        for i in 0..60 {
            e.push(crate::Sample {
                t: 0.5,
                link: format!("aa:bb:cc:dd:ee:{i:02x}"),
                kind: crate::Kind::Rssi,
                value: -60.0,
            });
        }
        let rep = e.step(1.0);
        let s = report_json(&rep, MAX_DGRAM);
        assert!(s.len() <= MAX_DGRAM);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(v.get("links").is_none());
        assert_eq!(v["links_active"], 60);
    }
}
