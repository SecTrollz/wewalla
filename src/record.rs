//! Raw sample recordings: capture what a device really delivers, then replay it offline through the
//! exact same `Engine`. A recording is JSON lines: an optional header, then one sample per line.
//!
//!   {"rvcsi_record":1,"unix_ms":1760000000000,"node":"edge2025","sources":["proc_wireless:wlan0"]}
//!   {"t":12.5031,"link":"aa:bb:cc:dd:ee:ff","kind":"rssi","value":-53.0}
//!
//! Reading is strict: malformed lines are counted, and a file with more than 1% bad lines is an error
//! rather than a quietly truncated experiment.

use crate::config::is_ident;
use crate::dsp::DspConfig;
use crate::engine::{Baselines, Engine, Report};
use crate::sample::{Kind, Sample};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

const MAX_LINE: usize = 4096;

#[derive(Serialize, Deserialize)]
struct Line {
    t: f64,
    link: String,
    kind: Kind,
    value: f32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Header {
    pub rvcsi_record: u32,
    pub unix_ms: u64,
    pub node: String,
    pub sources: Vec<String>,
}

#[derive(Debug)]
pub struct Recorder<W: Write> {
    w: W,
    pub written: u64,
}

impl<W: Write> Recorder<W> {
    pub fn new(mut w: W, node: &str, sources: Vec<String>) -> Result<Self, String> {
        let h = Header {
            rvcsi_record: 1,
            unix_ms: crate::sample::unix_ms(),
            node: node.to_string(),
            sources,
        };
        writeln!(
            w,
            "{}",
            serde_json::to_string(&h).map_err(|e| e.to_string())?
        )
        .map_err(|e| e.to_string())?;
        Ok(Self { w, written: 0 })
    }

    pub fn write(&mut self, s: &Sample) -> Result<(), String> {
        let l = Line {
            t: s.t,
            link: s.link.clone(),
            kind: s.kind,
            value: s.value,
        };
        writeln!(
            self.w,
            "{}",
            serde_json::to_string(&l).map_err(|e| e.to_string())?
        )
        .map_err(|e| e.to_string())?;
        self.written += 1;
        Ok(())
    }

    pub fn finish(mut self) -> Result<W, String> {
        self.w.flush().map_err(|e| e.to_string())?;
        Ok(self.w)
    }
}

#[derive(Debug, Default)]
pub struct Recording {
    pub header: Header,
    pub samples: Vec<Sample>,
    pub bad_lines: usize,
}

pub fn read<R: BufRead>(r: R) -> Result<Recording, String> {
    let mut rec = Recording::default();
    let mut total = 0usize;
    for (i, line) in r.lines().enumerate() {
        let line = line.map_err(|e| format!("line {}: {e}", i + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        total += 1;
        if line.len() > MAX_LINE {
            rec.bad_lines += 1;
            continue;
        }
        if i == 0 && line.contains("\"rvcsi_record\"") {
            rec.header = serde_json::from_str(&line).map_err(|e| format!("header: {e}"))?;
            total -= 1;
            continue;
        }
        match serde_json::from_str::<Line>(&line) {
            Ok(l) if l.t.is_finite() && l.value.is_finite() && l.t >= 0.0 && is_link(&l.link) => {
                rec.samples.push(Sample {
                    t: l.t,
                    link: l.link,
                    kind: l.kind,
                    value: l.value,
                });
            }
            _ => rec.bad_lines += 1,
        }
    }
    if total > 0 && rec.bad_lines * 100 > total {
        return Err(format!(
            "{} of {total} lines are malformed (limit 1%)",
            rec.bad_lines
        ));
    }
    if rec.samples.is_empty() {
        return Err("recording contains no valid samples".into());
    }
    Ok(rec)
}

fn is_link(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'.' | b'_' | b'-'))
}

/// Feed `samples` through a fresh engine in recorded time, stepping every hop. Returns every report.
pub fn replay(
    cfg: &DspConfig,
    node: &str,
    samples: &[Sample],
    baselines: Option<&Baselines>,
) -> Result<Vec<Report>, String> {
    if !is_ident(node, 32) {
        return Err("invalid node id".into());
    }
    let mut e = Engine::new(cfg.clone(), node)?;
    if let Some(b) = baselines {
        e.load_baselines(b);
    }
    let mut order: Vec<&Sample> = samples.iter().collect();
    order.sort_by(|a, b| a.t.total_cmp(&b.t));
    let hop = f64::from(cfg.hop_secs);
    let mut out = Vec::new();
    let Some(first) = order.first() else {
        return Ok(out);
    };
    let mut next = first.t + hop;
    let mut last_t = first.t;
    for s in order {
        while s.t >= next {
            out.push(e.step(next));
            next += hop;
        }
        last_t = s.t;
        e.push(s.clone());
    }
    out.push(e.step(last_t.max(next - hop)));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Rng;
    use std::io::Cursor;

    fn synth(secs: f64, hz: f64, bpm: f32, amp: f32, links: usize, seed: u64) -> Vec<Sample> {
        let mut rng = Rng::new(seed);
        let mut v = Vec::new();
        for i in 0..(secs * hz) as usize {
            let t = i as f64 / hz;
            for l in 0..links {
                let s = (std::f64::consts::TAU * f64::from(bpm) / 60.0 * t).sin() as f32;
                v.push(Sample {
                    t,
                    link: format!("l{l}"),
                    kind: Kind::Rssi,
                    value: (-55.0 + amp * s + rng.gauss()).round(),
                });
            }
        }
        v
    }

    #[test]
    fn roundtrip_preserves_samples_and_header() {
        let src = synth(5.0, 10.0, 15.0, 1.0, 2, 1);
        let mut r = Recorder::new(Vec::new(), "edge2025", vec!["synthetic".into()]).unwrap();
        for s in &src {
            r.write(s).unwrap();
        }
        let bytes = r.finish().unwrap();
        let rec = read(Cursor::new(bytes)).unwrap();
        assert_eq!(rec.header.node, "edge2025");
        assert_eq!(rec.header.sources, ["synthetic"]);
        assert_eq!(rec.samples.len(), src.len());
        assert_eq!(rec.bad_lines, 0);
        assert!(
            (rec.samples[7].t - src[7].t).abs() < 1e-12 && rec.samples[7].value == src[7].value
        );
    }

    #[test]
    fn replay_detects_the_breathing_that_was_recorded() {
        let reports = replay(
            &DspConfig::default(),
            "t",
            &synth(90.0, 10.0, 15.0, 1.0, 4, 3),
            None,
        )
        .unwrap();
        let last = reports.last().unwrap();
        assert!(
            last.breathing_detected,
            "conf {} thr {}",
            last.breathing_conf, last.breathing_threshold
        );
        assert!((last.breathing_bpm.unwrap() - 15.0).abs() < 1.5);
        assert!(reports.len() > 30);
    }

    #[test]
    fn replay_is_deterministic_and_order_independent() {
        let mut s = synth(60.0, 10.0, 18.0, 0.8, 2, 9);
        let a = replay(&DspConfig::default(), "t", &s, None).unwrap();
        s.reverse();
        let b = replay(&DspConfig::default(), "t", &s, None).unwrap();
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(
                (x.presence, x.breathing_detected, x.breathing_bpm),
                (y.presence, y.breathing_detected, y.breathing_bpm)
            );
        }
    }

    #[test]
    fn strict_reader_counts_bad_lines_and_rejects_corrupt_files() {
        let good = r#"{"t":1.0,"link":"a","kind":"rssi","value":-50.0}"#;
        let mut ok = String::new();
        for _ in 0..200 {
            ok.push_str(good);
            ok.push('\n');
        }
        ok.push_str("garbage\n");
        let rec = read(Cursor::new(ok)).unwrap();
        assert_eq!((rec.samples.len(), rec.bad_lines), (200, 1));
        let mut bad = String::from(good);
        bad.push_str("\nnot json\nalso not\n");
        assert!(
            read(Cursor::new(bad)).is_err(),
            "more than 1% bad lines must fail"
        );
        assert!(read(Cursor::new("")).is_err());
        for hostile in [
            r#"{"t":"x","link":"a","kind":"rssi","value":1}"#,
            r#"{"t":1,"link":"a b","kind":"rssi","value":1}"#,
            r#"{"t":-1,"link":"a","kind":"rssi","value":1}"#,
            r#"{"t":1,"link":"a","kind":"nope","value":1}"#,
        ] {
            let doc = format!("{good}\n{hostile}\n").repeat(1) + &format!("{good}\n").repeat(150);
            assert_eq!(read(Cursor::new(doc)).unwrap().bad_lines, 1, "{hostile}");
        }
    }
}
