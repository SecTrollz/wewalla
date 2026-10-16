use rvcsi::config::{Config, SourceCfg};
use rvcsi::engine::{Engine, Rng};
use rvcsi::sample::{Kind, Sample};
use rvcsi::{ingest, node};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

const USAGE: &str = "\
rvcsi-node: self-contained Wi-Fi sensing node (Android/Termux)

USAGE: rvcsi-node <command> [--config FILE] [options]

COMMANDS
  run                 start the node (sources -> DSP -> UDP / MQTT / local web UI)
  probe               report which link sources this device actually exposes
  calibrate           record an empty-room baseline   [--secs N, default 90]
  token               print the node token            [--url] print a one-time login URL
  check-config        validate the configuration and exit
  bench               DSP throughput on synthetic data [--links N, default 8]

--config defaults to $RVCSI_CONFIG, then ./config/edge2025.json, then built-in defaults.
";

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn load_config(args: &[String]) -> Result<Config, String> {
    let path = arg(args, "--config")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("RVCSI_CONFIG").map(PathBuf::from))
        .or_else(|| {
            let p = PathBuf::from("config/edge2025.json");
            p.exists().then_some(p)
        });
    match path {
        Some(p) => Config::load(&p),
        None => {
            let c = Config::default();
            c.validate()?;
            Ok(c)
        }
    }
}

fn bench(links: usize) -> Result<(), String> {
    let mut e = Engine::new(rvcsi::dsp::DspConfig::default(), "bench")?;
    let mut rng = Rng::new(1);
    let hz = 10.0;
    let secs = 120.0;
    let mut step_us = Vec::new();
    let mut push_ns = 0u128;
    let mut pushed = 0u64;
    let mut next = 2.0;
    for i in 0..(secs * hz) as usize {
        let t = i as f64 / hz;
        let s = (std::f64::consts::TAU * 0.25 * t).sin() as f32;
        let t0 = Instant::now();
        for l in 0..links {
            e.push(Sample {
                t,
                link: format!("l{l}"),
                kind: Kind::Rssi,
                value: (-55.0 + 0.6 * s + rng.gauss()).round(),
            });
            pushed += 1;
        }
        push_ns += t0.elapsed().as_nanos();
        if t >= next {
            let t1 = Instant::now();
            let _ = e.step(t);
            step_us.push(t1.elapsed().as_secs_f64() * 1e6);
            next += 2.0;
        }
    }
    step_us.sort_by(f64::total_cmp);
    let pct = |p: f64| step_us[((step_us.len() as f64 - 1.0) * p) as usize];
    println!(
        "{}",
        serde_json::json!({
            "links": links, "sim_seconds": secs, "arch": std::env::consts::ARCH,
            "push_ns_per_sample": push_ns as f64 / pushed as f64,
            "step_us": {"p50": pct(0.5), "p99": pct(0.99), "max": pct(1.0)},
            "note": "host-measured; run on the phone for device numbers",
        })
    );
    Ok(())
}

fn real_main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().map(String::as_str) else {
        print!("{USAGE}");
        return Ok(());
    };
    rvcsi::sample::now(); // pin the monotonic epoch
    match cmd {
        "run" => {
            let cfg = load_config(&args)?;
            node::run(cfg, Arc::new(AtomicBool::new(false)))
        }
        "probe" => {
            let iface = arg(&args, "--iface").unwrap_or_else(|| "wlan0".into());
            let r = ingest::probe(&iface);
            println!(
                "{}",
                serde_json::to_string_pretty(&r).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        "calibrate" => {
            let cfg = load_config(&args)?;
            let secs: f64 = arg(&args, "--secs").map_or(Ok(90.0), |s| {
                s.parse().map_err(|_| "--secs must be a number".to_string())
            })?;
            let b = node::calibrate(cfg.clone(), secs, Arc::new(AtomicBool::new(false)))?;
            println!(
                "saved {} baseline(s) to {}",
                b.links.len(),
                cfg.state_path("baseline.json").display()
            );
            Ok(())
        }
        "token" => {
            let cfg = load_config(&args)?;
            let t = rvcsi::web::auth::load_or_create_token(std::path::Path::new(&cfg.state_dir))?;
            if args.iter().any(|a| a == "--url") {
                println!("http://{}/login?token={t}", cfg.http.bind);
            } else {
                println!("{t}");
            }
            Ok(())
        }
        "check-config" => {
            let cfg = load_config(&args)?;
            let n = if cfg.sources.is_empty() {
                "auto".to_string()
            } else {
                cfg.sources.len().to_string()
            };
            println!(
                "ok: node={} sources={} http={} mqtt={} udp={}",
                cfg.node_id, n, cfg.http.enabled, cfg.mqtt.enabled, cfg.udp.enabled
            );
            if cfg
                .sources
                .iter()
                .any(|s| matches!(s, SourceCfg::Synthetic { .. }))
            {
                println!("warning: synthetic source configured; reports are labelled simulated");
            }
            Ok(())
        }
        "bench" => bench(arg(&args, "--links").map_or(Ok(8), |s| {
            s.parse()
                .map_err(|_| "--links must be an integer".to_string())
        })?),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown command {other:?}\n\n{USAGE}")),
    }
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
