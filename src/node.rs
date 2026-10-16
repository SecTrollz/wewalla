//! Wires sources -> engine -> exporters + web, and owns shutdown.

use crate::config::{Config, SourceCfg};
use crate::engine::{Baselines, Engine};
use crate::export::{mqtt, udp::UdpExporter};
use crate::ingest::{self, Health};
use crate::sample::{now, Sample};
use crate::web::{self, auth, AppState};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, TrySendError};
use std::sync::Arc;
use std::time::Duration;

const CALIBRATION_MARGIN: f32 = 1.15;

pub fn resolve_sources(cfg: &Config) -> Result<Vec<SourceCfg>, String> {
    if !cfg.sources.is_empty() {
        return Ok(cfg.sources.clone());
    }
    eprintln!("[node] no sources configured: probing this device");
    let probes = ingest::probe("wlan0");
    for p in &probes {
        eprintln!(
            "[probe] {:<14} {} {}",
            p.backend,
            if p.ok { "ok  " } else { "FAIL" },
            p.detail
        );
    }
    let chosen = ingest::auto_select("wlan0", &probes)?;
    eprintln!(
        "[node] auto-selected: {}",
        serde_json::to_string(&chosen).unwrap_or_default()
    );
    Ok(chosen)
}

pub fn write_private_atomic(path: &Path, data: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(&tmp)
        .and_then(|mut f| f.write_all(data).and_then(|()| f.sync_all()))
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn load_baselines(path: &Path) -> Result<Option<Baselines>, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => serde_json::from_str(&t)
            .map(Some)
            .map_err(|e| format!("{}: {e} (delete it and re-run `calibrate`)", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

type Spawned = (Vec<Arc<Health>>, Vec<std::thread::JoinHandle<()>>);

fn spawn_sources(
    srcs: &[SourceCfg],
    tx: &std::sync::mpsc::SyncSender<Sample>,
    stop: &Arc<AtomicBool>,
) -> Result<Spawned, String> {
    let (mut hs, mut js) = (Vec::new(), Vec::new());
    for c in srcs {
        let src = ingest::build(c)?;
        let (h, j) = ingest::spawn(src, tx.clone(), Arc::clone(stop));
        eprintln!("[node] source up: {}", h.name);
        hs.push(h);
        js.push(j);
    }
    Ok((hs, js))
}

/// Run the node until `stop` is set.
pub fn run(cfg: Config, stop: Arc<AtomicBool>) -> Result<(), String> {
    cfg.validate()?;
    let state_dir = Path::new(&cfg.state_dir).to_path_buf();
    let token = auth::load_or_create_token(&state_dir)?;
    let srcs = resolve_sources(&cfg)?;
    let mut engine = Engine::new(cfg.dsp.clone(), &cfg.node_id)?;
    if let Some(b) = load_baselines(&cfg.state_path("baseline.json"))? {
        eprintln!(
            "[node] loaded calibrated baseline for {} link(s)",
            b.links.len()
        );
        engine.load_baselines(&b);
    } else {
        eprintln!("[node] no calibration: baseline adapts online (run `rvcsi-node calibrate` in an empty room for best presence accuracy)");
    }

    let state = AppState::new(cfg.clone(), token)?;
    state.synthetic_only.store(
        srcs.iter()
            .all(|s| matches!(s, SourceCfg::Synthetic { .. })),
        Ordering::Relaxed,
    );

    let (tx, rx) = sync_channel::<Sample>(4096);
    let (healths, src_threads) = spawn_sources(&srcs, &tx, &stop)?;
    drop(tx);
    *state.healths.lock().map_err(|_| "poisoned")? = healths;

    let mut udp = if cfg.udp.enabled {
        Some(UdpExporter::new(&cfg.udp.targets)?)
    } else {
        None
    };
    let (mqtt_tx, mqtt_thread) = if cfg.mqtt.enabled {
        let (t, status, j) = mqtt::spawn(
            cfg.mqtt.clone(),
            cfg.node_id.clone(),
            cfg.device_name.clone(),
        );
        *state.mqtt.lock().map_err(|_| "poisoned")? = Some(status);
        (Some(t), Some(j))
    } else {
        (None, None)
    };
    let server = if cfg.http.enabled {
        let s = web::serve(Arc::clone(&state))?;
        eprintln!(
            "[web] listening on http://{}  (log in once with: rvcsi-node token --url)",
            s.addr
        );
        Some(s)
    } else {
        None
    };

    let hop = f64::from(cfg.dsp.hop_secs);
    let mut next = now() + hop;
    let mut last_log = 0.0f64;
    while !stop.load(Ordering::Relaxed) {
        let wait = (next - now()).clamp(0.0, 0.25);
        match rx.recv_timeout(Duration::from_secs_f64(wait)) {
            Ok(s) => {
                engine.push(s);
                for _ in 0..4096 {
                    match rx.try_recv() {
                        Ok(s) => engine.push(s),
                        Err(_) => break,
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let t = now();
        if t >= next {
            let rep = engine.step(t);
            state.publish(&rep);
            if let Some(u) = udp.as_mut() {
                u.send(&rep);
                state.udp_sent.store(u.sent, Ordering::Relaxed);
            }
            if t - last_log >= 15.0 {
                last_log = t;
                eprintln!(
                    "[node] mode={} presence={} score={:.2} breathing={} rate={:.1}Hz links={}",
                    rep.mode,
                    rep.presence,
                    rep.presence_score,
                    rep.breathing_bpm
                        .map_or("-".into(), |b| format!("{b:.1}bpm")),
                    rep.sample_rate_hz,
                    rep.links_active
                );
            }
            if let Some(m) = &mqtt_tx {
                if let Err(TrySendError::Disconnected(_)) = m.try_send(rep) {
                    eprintln!("[node] mqtt exporter died");
                }
            }
            next += hop;
            if next < t {
                next = t + hop;
            }
        }
    }

    eprintln!("[node] shutting down");
    stop.store(true, Ordering::Relaxed);
    state.stop.store(true, Ordering::Relaxed);
    drop(mqtt_tx);
    for j in src_threads {
        let _ = j.join();
    }
    if let Some(j) = mqtt_thread {
        let _ = j.join();
    }
    if let Some(s) = server {
        s.join();
    }
    Ok(())
}

/// Record an empty-room baseline for `secs` seconds and persist it.
pub fn calibrate(cfg: Config, secs: f64, stop: Arc<AtomicBool>) -> Result<Baselines, String> {
    cfg.validate()?;
    if secs < f64::from(cfg.dsp.presence_slow_window_secs) {
        return Err(format!(
            "calibration needs at least {} s (the slowest presence window)",
            cfg.dsp.presence_slow_window_secs
        ));
    }
    let srcs = resolve_sources(&cfg)?;
    let mut engine = Engine::new(cfg.dsp.clone(), &cfg.node_id)?;
    let (tx, rx) = sync_channel::<Sample>(4096);
    let (_h, threads) = spawn_sources(&srcs, &tx, &stop)?;
    drop(tx);
    eprintln!("[calibrate] keep the room EMPTY and the phone still for {secs:.0} s");
    let hop = f64::from(cfg.dsp.hop_secs);
    let (start, mut next) = (now(), now() + hop);
    while now() - start < secs && !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(s) => engine.push(s),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if now() >= next {
            engine.step(now());
            next += hop;
        }
    }
    stop.store(true, Ordering::Relaxed);
    for j in threads {
        let _ = j.join();
    }
    let mut b = engine.baselines();
    if b.links.is_empty() {
        return Err("no link produced enough samples to calibrate".into());
    }
    for v in b.links.values_mut() {
        *v *= CALIBRATION_MARGIN;
    }
    std::fs::create_dir_all(&cfg.state_dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_vec_pretty(&b).map_err(|e| e.to_string())?;
    write_private_atomic(&cfg.state_path("baseline.json"), &json)?;
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_cfg(tag: &str) -> Config {
        let mut c = Config::default();
        c.state_dir = std::env::temp_dir()
            .join(format!("rvcsi-node-{tag}-{}", std::process::id()))
            .display()
            .to_string();
        c.sources = vec![SourceCfg::Synthetic {
            hz: 20.0,
            links: 2,
            breathing_bpm: 15.0,
            amp: 1.0,
            noise: 0.5,
            present: true,
            seed: 3,
        }];
        c.dsp.hop_secs = 0.5;
        c.mqtt.enabled = false;
        c.udp.enabled = true;
        c.http.bind = "127.0.0.1:0".into();
        c.http.ui_dir = "/nonexistent".into();
        c
    }

    #[test]
    fn end_to_end_synthetic_node_emits_udp_and_serves_status() {
        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
        let mut cfg = synthetic_cfg("e2e");
        cfg.udp.targets = vec![rx.local_addr().unwrap().to_string()];
        let dir = cfg.state_dir.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&stop);
        let h = std::thread::spawn(move || run(cfg, s2));
        let mut buf = [0u8; 2048];
        let mut got = 0;
        let mut last: serde_json::Value = serde_json::Value::Null;
        while got < 3 {
            let n = rx.recv(&mut buf).expect("node must keep emitting reports");
            last = serde_json::from_slice(&buf[..n]).unwrap();
            got += 1;
        }
        assert_eq!(last["node"], "edge2025");
        assert_eq!(last["links_active"], 2);
        assert!(last["sample_rate_hz"].as_f64().unwrap() > 5.0);
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_config_is_refused_before_anything_starts() {
        let mut cfg = synthetic_cfg("bad");
        cfg.node_id = "bad id".into();
        assert!(run(cfg, Arc::new(AtomicBool::new(false))).is_err());
    }

    #[test]
    fn corrupt_baseline_is_a_hard_error() {
        let dir = std::env::temp_dir().join(format!("rvcsi-bl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("baseline.json"), "{").unwrap();
        assert!(load_baselines(&dir.join("baseline.json")).is_err());
        assert!(load_baselines(&dir.join("missing.json")).unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn calibration_refuses_windows_shorter_than_the_dsp_needs() {
        let cfg = synthetic_cfg("cal");
        assert!(calibrate(cfg, 5.0, Arc::new(AtomicBool::new(false))).is_err());
    }
}
