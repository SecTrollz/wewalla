//! Link ingest: one thread per source, bounded channel into the engine.

pub mod android;
pub mod exec;
pub mod synthetic;
pub mod udp_in;

use crate::config::SourceCfg;
use crate::sample::Sample;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub trait Source: Send {
    fn name(&self) -> String;
    /// Target spacing between polls. `ZERO` for sources whose `poll` blocks on I/O.
    fn period(&self) -> Duration;
    fn poll(&mut self, out: &mut Vec<Sample>) -> Result<(), String>;
}

/// Lock-free counters published to `/api/v1/status` and the CLI.
#[derive(Debug, Default)]
pub struct Health {
    pub name: String,
    pub polls_ok: AtomicU64,
    pub polls_err: AtomicU64,
    pub samples: AtomicU64,
    pub dropped: AtomicU64,
    pub last_ok_unix_ms: AtomicU64,
    pub last_err: Mutex<String>,
}

#[derive(Debug, Serialize)]
pub struct HealthSnapshot {
    pub name: String,
    pub polls_ok: u64,
    pub polls_err: u64,
    pub samples: u64,
    pub dropped: u64,
    pub last_ok_unix_ms: u64,
    pub last_err: String,
}

impl Health {
    pub fn new(name: String) -> Self {
        Self {
            name,
            ..Default::default()
        }
    }
    pub fn snapshot(&self) -> HealthSnapshot {
        HealthSnapshot {
            name: self.name.clone(),
            polls_ok: self.polls_ok.load(Ordering::Relaxed),
            polls_err: self.polls_err.load(Ordering::Relaxed),
            samples: self.samples.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            last_ok_unix_ms: self.last_ok_unix_ms.load(Ordering::Relaxed),
            last_err: self.last_err.lock().map(|g| g.clone()).unwrap_or_default(),
        }
    }
}

pub fn build(cfg: &SourceCfg) -> Result<Box<dyn Source>, String> {
    Ok(match cfg {
        SourceCfg::ProcWireless { iface, hz } => Box::new(android::ProcWireless {
            iface: iface.clone(),
            hz: *hz,
        }),
        SourceCfg::IwLink { iface, hz, su } => Box::new(android::IwLink {
            iface: iface.clone(),
            hz: *hz,
            su: *su,
        }),
        SourceCfg::TermuxConn { hz } => Box::new(android::TermuxConn { hz: *hz }),
        SourceCfg::TermuxScan { interval_s } => Box::new(android::TermuxScan::new(*interval_s)),
        SourceCfg::UdpIngest { bind } => Box::new(udp_in::UdpIngest::bind(bind)?),
        SourceCfg::Synthetic {
            hz,
            links,
            breathing_bpm,
            amp,
            noise,
            present,
            seed,
        } => Box::new(synthetic::Synthetic::new(
            *hz,
            *links,
            *breathing_bpm,
            *amp,
            *noise,
            *present,
            *seed,
        )),
    })
}

fn sleep_until_or_stop(dur: Duration, stop: &AtomicBool) {
    let end = Instant::now() + dur;
    while !stop.load(Ordering::Relaxed) {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
}

/// Run `src` until `stop`. A failing source backs off exponentially (cap 30 s) and keeps
/// retrying; it never takes the node down.
pub fn spawn(
    mut src: Box<dyn Source>,
    tx: SyncSender<Sample>,
    stop: Arc<AtomicBool>,
) -> (Arc<Health>, JoinHandle<()>) {
    let health = Arc::new(Health::new(src.name()));
    let h = Arc::clone(&health);
    let handle = std::thread::Builder::new()
        .name(format!("src-{}", h.name))
        .spawn(move || {
            let mut buf = Vec::with_capacity(64);
            let mut consecutive_err = 0u32;
            while !stop.load(Ordering::Relaxed) {
                let started = Instant::now();
                buf.clear();
                match src.poll(&mut buf) {
                    Ok(()) => {
                        consecutive_err = 0;
                        h.polls_ok.fetch_add(1, Ordering::Relaxed);
                        if !buf.is_empty() {
                            h.last_ok_unix_ms
                                .store(crate::sample::unix_ms(), Ordering::Relaxed);
                        }
                        for s in buf.drain(..) {
                            match tx.try_send(s) {
                                Ok(()) => {
                                    h.samples.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(TrySendError::Full(_)) => {
                                    h.dropped.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(TrySendError::Disconnected(_)) => return,
                            }
                        }
                    }
                    Err(e) => {
                        consecutive_err = consecutive_err.saturating_add(1);
                        h.polls_err.fetch_add(1, Ordering::Relaxed);
                        if let Ok(mut g) = h.last_err.lock() {
                            *g = e.chars().take(300).collect();
                        }
                    }
                }
                let base = src.period();
                let wait = if consecutive_err > 0 {
                    (base.max(Duration::from_millis(250))
                        * 2u32.saturating_pow(consecutive_err.min(8)))
                    .min(Duration::from_secs(30))
                } else {
                    base.saturating_sub(started.elapsed())
                };
                sleep_until_or_stop(wait, &stop);
            }
        })
        .expect("spawn source thread");
    (health, handle)
}

// ------------------------------------------------------------------ probing

#[derive(Debug, Clone, Serialize)]
pub struct ProbeResult {
    pub backend: &'static str,
    pub ok: bool,
    pub detail: String,
    pub sample: Option<f32>,
    pub achievable_hz: Option<f32>,
}

fn probe_one(backend: &'static str, mut src: Box<dyn Source>, polls: usize) -> ProbeResult {
    let t0 = Instant::now();
    let mut last = Err("not polled".to_string());
    let mut ok_n = 0usize;
    let mut sample = None;
    let mut buf = Vec::new();
    for _ in 0..polls {
        buf.clear();
        match src.poll(&mut buf) {
            Ok(()) => {
                ok_n += 1;
                last = Ok(());
                sample = buf.first().map(|s| s.value).or(sample);
            }
            Err(e) => last = Err(e),
        }
        if ok_n == 0 && last.is_err() {
            break; // no point hammering a backend that already failed
        }
    }
    let el = t0.elapsed().as_secs_f32();
    match last {
        Ok(()) if ok_n > 0 => ProbeResult {
            backend,
            ok: true,
            detail: format!("{ok_n}/{polls} polls ok"),
            sample,
            achievable_hz: Some(ok_n as f32 / el.max(1e-3)),
        },
        Err(e) => ProbeResult {
            backend,
            ok: false,
            detail: e,
            sample: None,
            achievable_hz: None,
        },
        _ => ProbeResult {
            backend,
            ok: false,
            detail: "no data".into(),
            sample: None,
            achievable_hz: None,
        },
    }
}

pub fn probe(iface: &str) -> Vec<ProbeResult> {
    let mut r = vec![
        probe_one(
            "proc_wireless",
            Box::new(android::ProcWireless {
                iface: iface.into(),
                hz: 10.0,
            }),
            5,
        ),
        probe_one(
            "iw_link",
            Box::new(android::IwLink {
                iface: iface.into(),
                hz: 5.0,
                su: false,
            }),
            3,
        ),
        probe_one("termux_conn", Box::new(android::TermuxConn { hz: 2.0 }), 4),
        probe_one("termux_scan", Box::new(android::TermuxScan::new(8.0)), 1),
    ];
    r.push(ProbeResult {
        backend: "wifi_rtt",
        ok: false,
        detail: "not reachable from Termux:API. Needs a companion app (WifiRttManager, FTM-capable AP) feeding udp_ingest".into(),
        sample: None,
        achievable_hz: None,
    });
    r.push(ProbeResult {
        backend: "csi",
        ok: false,
        detail: "stock Android drivers do not expose CSI or beamforming feedback; a rooted/custom-firmware bridge must feed udp_ingest".into(),
        sample: None,
        achievable_hz: None,
    });
    r
}

/// Choose the best fast link source plus scan for presence-grade neighbour data.
pub fn auto_select(iface: &str, probes: &[ProbeResult]) -> Result<Vec<SourceCfg>, String> {
    let get = |b: &str| probes.iter().find(|p| p.backend == b && p.ok);
    let mut out = Vec::new();
    if get("proc_wireless").is_some() {
        out.push(SourceCfg::ProcWireless {
            iface: iface.into(),
            hz: 10.0,
        });
    } else if get("iw_link").is_some() {
        out.push(SourceCfg::IwLink {
            iface: iface.into(),
            hz: 5.0,
            su: false,
        });
    } else if let Some(p) = get("termux_conn") {
        let hz = (p.achievable_hz.unwrap_or(1.0) * 0.8).clamp(0.5, 4.0);
        out.push(SourceCfg::TermuxConn { hz });
    }
    if get("termux_scan").is_some() {
        out.push(SourceCfg::TermuxScan { interval_s: 8.0 });
    }
    if out.is_empty() {
        let why: Vec<String> = probes
            .iter()
            .filter(|p| !p.ok)
            .map(|p| format!("{}: {}", p.backend, p.detail))
            .collect();
        return Err(format!(
            "no usable link source on this device:\n  {}",
            why.join("\n  ")
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(b: &'static str, ok: bool, hz: f32) -> ProbeResult {
        ProbeResult {
            backend: b,
            ok,
            detail: if ok { "ok".into() } else { "nope".into() },
            sample: None,
            achievable_hz: ok.then_some(hz),
        }
    }

    #[test]
    fn auto_select_prefers_fast_sources_and_adds_scan() {
        let p = [
            pr("proc_wireless", true, 50.0),
            pr("iw_link", true, 5.0),
            pr("termux_conn", true, 3.0),
            pr("termux_scan", true, 0.1),
        ];
        let s = auto_select("wlan0", &p).unwrap();
        assert_eq!(s.len(), 2);
        assert!(matches!(s[0], SourceCfg::ProcWireless { .. }));
        assert!(matches!(s[1], SourceCfg::TermuxScan { .. }));
    }

    #[test]
    fn auto_select_falls_back_and_clamps_rate() {
        let p = [
            pr("proc_wireless", false, 0.0),
            pr("iw_link", false, 0.0),
            pr("termux_conn", true, 9.0),
            pr("termux_scan", false, 0.0),
        ];
        let s = auto_select("wlan0", &p).unwrap();
        match s[..] {
            [SourceCfg::TermuxConn { hz }] => assert!((0.5..=4.0).contains(&hz)),
            _ => panic!("{s:?}"),
        }
    }

    #[test]
    fn auto_select_reports_why_when_nothing_works() {
        let p = [
            pr("proc_wireless", false, 0.0),
            pr("termux_conn", false, 0.0),
        ];
        let e = auto_select("wlan0", &p).unwrap_err();
        assert!(e.contains("proc_wireless") && e.contains("termux_conn"));
    }

    struct Flaky(u32);
    impl Source for Flaky {
        fn name(&self) -> String {
            "flaky".into()
        }
        fn period(&self) -> Duration {
            Duration::from_millis(5)
        }
        fn poll(&mut self, out: &mut Vec<Sample>) -> Result<(), String> {
            self.0 += 1;
            if self.0 <= 2 {
                return Err("boom".into());
            }
            out.push(Sample {
                t: crate::sample::now(),
                link: "x".into(),
                kind: crate::sample::Kind::Rssi,
                value: -50.0,
            });
            Ok(())
        }
    }

    #[test]
    fn runner_survives_errors_recovers_and_stops() {
        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        let stop = Arc::new(AtomicBool::new(false));
        let (health, h) = spawn(Box::new(Flaky(0)), tx, Arc::clone(&stop));
        let s = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("recovered after errors");
        assert_eq!(s.link, "x");
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
        let snap = health.snapshot();
        assert!(snap.polls_err >= 2 && snap.samples >= 1);
        assert_eq!(snap.last_err, "boom");
    }

    #[test]
    fn full_channel_drops_instead_of_blocking() {
        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let (health, h) = spawn(
            Box::new(synthetic::Synthetic::new(100.0, 1, 15.0, 0.5, 0.5, true, 1)),
            tx,
            Arc::clone(&stop),
        );
        std::thread::sleep(Duration::from_millis(300));
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
        assert!(health.snapshot().dropped > 0);
    }
}
