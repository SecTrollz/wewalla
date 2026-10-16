use rvcsi::config::{Config, SourceCfg};
use rvcsi::engine::{Engine, Rng};
use rvcsi::sample::{Kind, Sample};
use rvcsi::validate::{self, Criteria, RangeParams, Step};
use rvcsi::{ingest, node, record};
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
  record --out F      record raw link samples to a JSONL file     [--secs N, default 120]
  replay F            run a recording through the detector offline (reproducible analysis)
  validate --mode M   field-test the detector on this device (verdict JSON, exit 2 on FAIL):
                        empty   empty-room control                [--secs 300]
                        paced   metronome-paced breathing         --bpm B [--secs 180] [--feet D]
                        range   distance x bearing coverage walk  [--target-ft 40 --bearings 8 --distances 5,10,20,30,40
                                                                    --cell-secs 100 --control-secs 100]
                      common: [--settle-secs 40] [--no-wait]
  selftest            measure the detector's real false-alarm / detection rates [--trials N, default 400] [--hz R, default 10]

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

fn num<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> Result<T, String> {
    arg(args, name).map_or(Ok(default), |v| {
        v.parse()
            .map_err(|_| format!("{name}: invalid value {v:?}"))
    })
}

fn replay_cmd(args: &[String]) -> Result<(), String> {
    let path = args
        .get(1)
        .filter(|a| !a.starts_with("--"))
        .ok_or("usage: replay FILE")?;
    let cfg = load_config(args)?;
    let f = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let rec = record::read(std::io::BufReader::new(f))?;
    eprintln!(
        "replaying {} samples ({} malformed lines skipped), recorded from {:?}",
        rec.samples.len(),
        rec.bad_lines,
        rec.header.sources
    );
    let reports = record::replay(&cfg.dsp, &cfg.node_id, &rec.samples, None)?;
    for (i, r) in reports.iter().enumerate() {
        println!(
            "{}",
            serde_json::json!({"i": i, "presence": r.presence, "score": r.presence_score, "breathing_detected": r.breathing_detected,
                "bpm": r.breathing_bpm, "conf": r.breathing_conf, "threshold": r.breathing_threshold, "links": r.links_active, "mode": r.mode})
        );
    }
    let crit = Criteria::from_cfg(&cfg.dsp);
    println!(
        "{}",
        serde_json::json!({"summary": validate::stats(&reports, &crit)})
    );
    Ok(())
}

fn validate_cmd(args: &[String]) -> Result<(), String> {
    let cfg = load_config(args)?;
    let mode = arg(args, "--mode").ok_or("--mode empty|paced|range is required")?;
    let mut crit = Criteria::from_cfg(&cfg.dsp);
    crit.settle_secs = num(args, "--settle-secs", crit.settle_secs)?;
    let min_secs = crit.settle_secs + crit.min_reports as f64 * crit.hop_secs;
    let live = node::Live::start(&cfg)?;
    let mut col = node::LiveCollector {
        live,
        wait_for_enter: !args.iter().any(|a| a == "--no-wait"),
    };
    use rvcsi::validate::Collector;
    let result: Result<(serde_json::Value, bool), String> = (|| match mode.as_str() {
        "empty" => {
            let secs = num(args, "--secs", 300.0f64)?.max(min_secs);
            col.instruct(&Step::Control)?;
            let r = validate::eval_empty(&col.collect(secs)?, &crit);
            let pass = r.pass;
            Ok((serde_json::to_value(r).map_err(|e| e.to_string())?, pass))
        }
        "paced" => {
            let bpm: f32 = arg(args, "--bpm")
                .ok_or("--bpm is required for paced mode")?
                .parse()
                .map_err(|_| "--bpm must be a number")?;
            if !(4.0..=40.0).contains(&bpm) {
                return Err("--bpm must be within 4..=40".into());
            }
            let secs = num(args, "--secs", 180.0f64)?.max(min_secs);
            col.instruct(&Step::Paced {
                bpm,
                feet: arg(args, "--feet").and_then(|f| f.parse().ok()),
            })?;
            let r = validate::eval_paced(&col.collect(secs)?, bpm, &crit);
            let pass = r.pass;
            Ok((serde_json::to_value(r).map_err(|e| e.to_string())?, pass))
        }
        "range" => {
            let distances: Vec<u32> = arg(args, "--distances")
                .unwrap_or_else(|| "5,10,20,30,40".into())
                .split(',')
                .map(|d| {
                    d.trim()
                        .parse()
                        .map_err(|_| format!("--distances: bad value {d:?}"))
                })
                .collect::<Result<_, _>>()?;
            let p = RangeParams {
                bearings: validate::bearing_labels(num(args, "--bearings", 8usize)?),
                distances_ft: distances,
                cell_secs: num(args, "--cell-secs", 100.0)?,
                control_secs: num(args, "--control-secs", 100.0)?,
                target_ft: num(args, "--target-ft", 40u32)?,
                criteria: crit.clone(),
            };
            let r = validate::run_range(&mut col, &p)?;
            let pass = r.valid && r.presence_target_met;
            Ok((serde_json::to_value(r).map_err(|e| e.to_string())?, pass))
        }
        other => Err(format!("unknown --mode {other:?}")),
    })();
    let node::LiveCollector { live, .. } = col;
    live.shutdown();
    let (value, pass) = result?;
    println!("{}", serde_json::to_string_pretty(&serde_json::json!({"mode": mode, "verdict": if pass { "PASS" } else { "FAIL" }, "result": value})).map_err(|e| e.to_string())?);
    if !pass {
        std::process::exit(2);
    }
    Ok(())
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
        "record" => {
            let cfg = load_config(&args)?;
            let out = arg(&args, "--out").ok_or("--out FILE is required")?;
            let n = node::record(
                &cfg,
                num(&args, "--secs", 120.0f64)?,
                std::path::Path::new(&out),
            )?;
            println!("recorded {n} samples to {out}");
            Ok(())
        }
        "replay" => replay_cmd(&args),
        "validate" => validate_cmd(&args),
        "selftest" => {
            let trials: usize = arg(&args, "--trials").map_or(Ok(400), |s| {
                s.parse()
                    .map_err(|_| "--trials must be an integer".to_string())
            })?;
            let hz: f64 = arg(&args, "--hz").map_or(Ok(10.0), |s| {
                s.parse().map_err(|_| "--hz must be a number".to_string())
            })?;
            if args.iter().any(|a| a == "--calibrate") {
                let cfg = rvcsi::dsp::DspConfig::default();
                let t = rvcsi::selftest::calibrate(&cfg, trials, 5_000_000, 8, &[10.0, 4.0, 2.0]);
                println!("// generated by `rvcsi-node selftest --calibrate --trials {trials}`");
                println!("pub const CALIBRATED_THRESHOLDS: [f32; 8] = {t:?};");
                return Ok(());
            }
            let t = rvcsi::selftest::run(trials, hz);
            println!(
                "{}",
                serde_json::to_string_pretty(&t).map_err(|e| e.to_string())?
            );
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
