//! The sensing engine: per-link ring buffers -> presence + breathing report.

use crate::dsp::{detrended_std, median, DspConfig, Spectral};
use crate::sample::{unix_ms, Kind, Sample};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};

#[derive(Clone, Debug, Serialize)]
pub struct LinkInfo {
    pub id: String,
    pub kind: Kind,
    pub last_value: f32,
    pub rate_hz: f32,
    pub std: f32,
    pub baseline: f32,
    pub breathing_capable: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub ts_ms: u64,
    pub node: String,
    pub presence: bool,
    /// Max over links of (windowed detrended std / adaptive noise baseline).
    pub presence_score: f32,
    /// Largest windowed detrended std across links (units of the series, dB for RSSI).
    pub motion_std: f32,
    pub breathing_detected: bool,
    pub breathing_bpm: Option<f32>,
    /// Fused spectral peak / mean band power (dimensionless).
    pub breathing_conf: f32,
    /// Detection threshold for `breathing_conf` at the current link count and false-alarm rate.
    pub breathing_threshold: f32,
    pub dominant_freq_hz: Option<f32>,
    pub mean_value: f32,
    pub links_active: usize,
    pub links_breath_capable: usize,
    /// Highest raw sample rate among links.
    pub sample_rate_hz: f32,
    /// "breathing+presence" | "presence-only" | "no-data"
    pub mode: &'static str,
    pub links: Vec<LinkInfo>,
}

#[derive(Debug)]
struct Link {
    kind: Kind,
    id: String,
    ring: VecDeque<(f64, f32)>,
    baseline: Option<f32>,
    frozen: bool,
    last_t: f64,
    last_std: f32,
    rate: f32,
    capable: bool,
}

#[derive(Debug)]
pub struct Engine {
    cfg: DspConfig,
    node: String,
    links: HashMap<String, Link>,
    spec: Spectral,
    fused: Vec<f32>,
    scratch: Vec<f32>,
    presence: bool,
    last_active: f64,
    conf_hist: VecDeque<bool>,
    bpm_hist: VecDeque<f32>,
    preset: BTreeMap<String, f32>,
    thresholds: Vec<f32>,
}

/// Persisted calibration: link key -> baseline std.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Baselines {
    pub links: BTreeMap<String, f32>,
}

fn key(kind: Kind, id: &str) -> String {
    format!("{}:{}", kind.as_str(), id)
}

impl Engine {
    pub fn new(cfg: DspConfig, node: &str) -> Result<Self, String> {
        cfg.validate()?;
        let spec = Spectral::new(&cfg);
        let bins = spec.bins();
        Ok(Self {
            node: node.to_string(),
            links: HashMap::new(),
            spec,
            fused: vec![0.0; bins],
            scratch: vec![0.0; bins],
            presence: false,
            last_active: f64::NEG_INFINITY,
            conf_hist: VecDeque::with_capacity(4),
            bpm_hist: VecDeque::with_capacity(6),
            preset: BTreeMap::new(),
            thresholds: (1..=32).map(|l| cfg.threshold_for(l)).collect(),
            cfg,
        })
    }

    pub fn config(&self) -> &DspConfig {
        &self.cfg
    }

    /// Install a calibrated empty-room baseline. Frozen baselines never creep upward,
    /// so a person sitting still cannot be absorbed into "normal".
    pub fn load_baselines(&mut self, b: &Baselines) {
        for (k, v) in &b.links {
            if v.is_finite() && *v > 0.0 {
                self.preset.insert(k.clone(), *v);
            }
        }
        for (k, l) in &mut self.links {
            if let Some(v) = self.preset.get(k) {
                l.baseline = Some(*v);
                l.frozen = true;
            }
        }
    }

    pub fn baselines(&self) -> Baselines {
        Baselines {
            links: self
                .links
                .iter()
                .filter_map(|(k, l)| l.baseline.map(|b| (k.clone(), b)))
                .collect(),
        }
    }

    pub fn push(&mut self, s: Sample) {
        if !s.value.is_finite() || !s.t.is_finite() {
            return;
        }
        let k = key(s.kind, &s.link);
        let horizon = f64::from(self.cfg.horizon_secs());
        let preset = self.preset.get(&k).copied();
        let l = self.links.entry(k).or_insert_with(|| Link {
            kind: s.kind,
            id: s.link.clone(),
            ring: VecDeque::with_capacity(256),
            baseline: preset,
            frozen: preset.is_some(),
            last_t: s.t,
            last_std: 0.0,
            rate: 0.0,
            capable: false,
        });
        // Samples must be monotonic per link; a late sample is dropped, not reordered.
        if l.ring.back().is_some_and(|&(t, _)| s.t < t) {
            return;
        }
        l.ring.push_back((s.t, s.value));
        l.last_t = s.t;
        while l.ring.front().is_some_and(|&(t, _)| t < s.t - horizon) {
            l.ring.pop_front();
        }
    }

    pub fn step(&mut self, now: f64) -> Report {
        let cfg = &self.cfg;
        self.links
            .retain(|_, l| now - l.last_t <= f64::from(cfg.stale_secs));
        self.fused.fill(0.0);
        let (mut nfused, mut capable_n) = (0usize, 0usize);
        let (mut max_score, mut max_std, mut max_rate) = (0f32, 0f32, 0f32);
        let (mut mean_acc, mut mean_n) = (0f32, 0usize);

        for l in self.links.values_mut() {
            let horizon = f64::from(cfg.horizon_secs());
            while l.ring.front().is_some_and(|&(t, _)| t < now - horizon) {
                l.ring.pop_front();
            }
            let wcut = now - f64::from(cfg.window_secs);
            // True sample rate over the observed span (not span-of-window, which under-reports during warm-up).
            let (mut n_win, mut oldest) = (0usize, now);
            for &(t, _) in l.ring.iter().rev() {
                if t < wcut {
                    break;
                }
                n_win += 1;
                oldest = t;
            }
            let newest = l.ring.back().map_or(oldest, |&(t, _)| t);
            l.rate = if n_win >= 2 && newest > oldest {
                ((n_win - 1) as f64 / (newest - oldest)) as f32
            } else {
                0.0
            };
            max_rate = max_rate.max(l.rate);
            if let Some(&(_, v)) = l.ring.back() {
                mean_acc += v;
                mean_n += 1;
            }

            // --- presence: detrended std vs adaptive noise baseline
            let pw = f64::from(if l.rate >= 0.5 {
                cfg.presence_window_secs
            } else {
                cfg.presence_slow_window_secs
            });
            let pcut = now - pw;
            let (n, sd) = detrended_std(
                l.ring
                    .iter()
                    .rev()
                    .take_while(|&&(t, _)| t >= pcut)
                    .map(|&(t, v)| (t - now, v)),
            );
            if n >= cfg.min_presence_samples {
                let floor = l.kind.std_floor();
                let b = match l.baseline {
                    None => sd.max(floor),
                    Some(b) if sd < b => b + 0.2 * (sd - b),
                    Some(b) if l.frozen => b,
                    Some(b) => b * (1.0 + cfg.baseline_leak_per_hop),
                }
                .max(floor);
                l.baseline = Some(b);
                l.last_std = sd;
                max_std = max_std.max(sd);
                max_score = max_score.max(sd / b);
            }

            // --- breathing spectrum
            l.capable = false;
            if l.rate >= cfg.min_breath_rate_hz
                && self.spec.load(l.ring.iter(), now, cfg.min_coverage)
                && self.spec.compute_snr()
            {
                l.capable = true;
                capable_n += 1;
                nfused += 1;
                for (f, s) in self.fused.iter_mut().zip(&self.spec.snr) {
                    *f += s;
                }
            }
        }

        // --- fuse + peak pick
        let (mut bpm, mut conf, mut dom, mut thr) = (None, 0f32, None, self.thresholds[0]);
        let mut raw_hit = false;
        if nfused > 0 {
            thr = self.thresholds[nfused.clamp(1, 32) - 1];
            let inv = 1.0 / nfused as f32;
            self.fused.iter_mut().for_each(|f| *f *= inv);
            let (kmax, pmax) = self
                .fused
                .iter()
                .copied()
                .enumerate()
                .fold((0usize, 0f32), |a, (k, p)| if p > a.1 { (k, p) } else { a });
            let mean = self.fused.iter().sum::<f32>() / self.fused.len() as f32;
            conf = if mean > 0.0 { pmax / mean } else { 0.0 };
            // A peak on the band edge means energy is rising outside the band (drift/leakage), not a tone.
            let interior = kmax > 0 && kmax + 1 < self.fused.len();
            if interior {
                let (a, b, c) = (self.fused[kmax - 1], self.fused[kmax], self.fused[kmax + 1]);
                let den = a - 2.0 * b + c;
                let delta = if den.abs() > f32::EPSILON {
                    (0.5 * (a - c) / den).clamp(-0.5, 0.5)
                } else {
                    0.0
                };
                let f = self.spec.freqs[kmax] + delta * cfg.band_step_hz;
                dom = Some(f);
                raw_hit = conf >= thr;
                if raw_hit {
                    self.bpm_hist.push_back(f * 60.0);
                    if self.bpm_hist.len() > 5 {
                        self.bpm_hist.pop_front();
                    }
                }
            }
        }
        self.conf_hist.push_back(raw_hit);
        if self.conf_hist.len() > 3 {
            self.conf_hist.pop_front();
        }
        let detected =
            self.conf_hist.iter().filter(|&&h| h).count() >= 2 && !self.bpm_hist.is_empty();
        if detected {
            self.scratch.clear();
            self.scratch.extend(self.bpm_hist.iter().copied());
            bpm = Some(median(&mut self.scratch));
        } else if self.conf_hist.iter().all(|&h| !h) {
            self.bpm_hist.clear();
        }

        // --- presence state machine with hysteresis + hold
        let hot = if self.presence {
            max_score >= cfg.presence_off
        } else {
            max_score >= cfg.presence_on
        };
        if hot || detected {
            self.last_active = now;
            self.presence = true;
        } else if now - self.last_active > f64::from(cfg.presence_hold_secs) {
            self.presence = false;
        }

        let mut links: Vec<LinkInfo> = self
            .links
            .values()
            .map(|l| LinkInfo {
                id: l.id.clone(),
                kind: l.kind,
                last_value: l.ring.back().map_or(f32::NAN, |&(_, v)| v),
                rate_hz: l.rate,
                std: l.last_std,
                baseline: l.baseline.unwrap_or(0.0),
                breathing_capable: l.capable,
            })
            .collect();
        links.sort_by(|a, b| a.id.cmp(&b.id));
        for li in &mut links {
            if !li.last_value.is_finite() {
                li.last_value = 0.0;
            }
        }

        Report {
            ts_ms: unix_ms(),
            node: self.node.clone(),
            presence: self.presence,
            presence_score: max_score,
            motion_std: max_std,
            breathing_detected: detected,
            breathing_bpm: bpm,
            breathing_conf: conf,
            breathing_threshold: thr,
            dominant_freq_hz: dom,
            mean_value: if mean_n > 0 {
                mean_acc / mean_n as f32
            } else {
                0.0
            },
            links_active: self.links.len(),
            links_breath_capable: capable_n,
            sample_rate_hz: max_rate,
            mode: if self.links.is_empty() {
                "no-data"
            } else if capable_n > 0 {
                "breathing+presence"
            } else {
                "presence-only"
            },
            links,
        }
    }
}

/// Deterministic generator for tests, benchmarks and the `synthetic` source.
#[derive(Debug, Clone)]
pub struct Rng(u64);
impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn uniform(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
    /// Approximately N(0,1) (Irwin-Hall, n=12).
    pub fn gauss(&mut self) -> f32 {
        (0..12).map(|_| self.uniform()).sum::<f32>() - 6.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(
        e: &mut Engine,
        rng: &mut Rng,
        link: &str,
        t0: f64,
        secs: f64,
        hz: f64,
        f: impl Fn(f64, &mut Rng) -> f32,
    ) -> Report {
        let n = (secs * hz) as usize;
        let hop = f64::from(e.config().hop_secs);
        let mut next = t0 + hop;
        let mut last = None;
        for i in 0..n {
            let t = t0 + i as f64 / hz;
            let v = f(t, rng).round(); // integer dB like Android
            e.push(Sample {
                t,
                link: link.into(),
                kind: Kind::Rssi,
                value: v,
            });
            if t >= next {
                last = Some(e.step(t));
                next += hop;
            }
        }
        last.expect("at least one step")
    }

    fn breathing(bpm: f32, amp: f32, sigma: f32) -> impl Fn(f64, &mut Rng) -> f32 {
        move |t, r| {
            -55.0
                + amp * (std::f64::consts::TAU * f64::from(bpm) / 60.0 * t).sin() as f32
                + sigma * r.gauss()
        }
    }

    #[test]
    fn detects_breathing_rate_single_link() {
        let mut e = Engine::new(DspConfig::default(), "t").unwrap();
        let mut r = Rng::new(7);
        let rep = feed(
            &mut e,
            &mut r,
            "ap1",
            0.0,
            90.0,
            10.0,
            breathing(15.0, 0.8, 0.8),
        );
        assert!(rep.breathing_detected, "conf={}", rep.breathing_conf);
        assert!(
            (rep.breathing_bpm.unwrap() - 15.0).abs() <= 2.0,
            "bpm={:?}",
            rep.breathing_bpm
        );
        assert_eq!(rep.mode, "breathing+presence");
        assert!(rep.presence);
    }

    #[test]
    fn multi_link_fusion_beats_single_link_at_low_snr() {
        let mut hits_single = 0;
        let mut hits_fused = 0;
        for seed in 0..12u64 {
            let mut r = Rng::new(100 + seed);
            let mut e1 = Engine::new(DspConfig::default(), "t").unwrap();
            let rep1 = feed(
                &mut e1,
                &mut r,
                "a",
                0.0,
                80.0,
                10.0,
                breathing(18.0, 0.35, 1.0),
            );
            hits_single += usize::from(rep1.breathing_detected);

            let mut e4 = Engine::new(DspConfig::default(), "t").unwrap();
            let mut rep4 = None;
            let sine = |t: f64| (std::f64::consts::TAU * 18.0 / 60.0 * t).sin() as f32;
            let hop = 2.0;
            let mut next = hop;
            for i in 0..800 {
                let t = f64::from(i) / 10.0;
                for l in 0..4 {
                    let v = (-55.0 + 0.35 * sine(t) + r.gauss()).round();
                    e4.push(Sample {
                        t,
                        link: format!("l{l}"),
                        kind: Kind::Rssi,
                        value: v,
                    });
                }
                if t >= next {
                    rep4 = Some(e4.step(t));
                    next += hop;
                }
            }
            hits_fused += usize::from(rep4.unwrap().breathing_detected);
        }
        assert!(
            hits_fused >= hits_single,
            "fused {hits_fused} < single {hits_single}"
        );
        assert!(hits_fused >= 8, "fused detections {hits_fused}/12");
    }

    #[test]
    fn white_noise_false_positive_rate_is_low() {
        let mut fp = 0;
        let trials = 60;
        for seed in 0..trials {
            let mut e = Engine::new(DspConfig::default(), "t").unwrap();
            let mut r = Rng::new(9000 + seed);
            let rep = feed(&mut e, &mut r, "n", 0.0, 60.0, 10.0, |_, r| {
                -60.0 + 1.5 * r.gauss()
            });
            fp += usize::from(rep.breathing_detected);
        }
        assert!(
            fp * 100 <= trials as usize * 5,
            "false positives {fp}/{trials}"
        );
    }

    #[test]
    fn presence_rises_on_motion_and_clears_after_hold() {
        let mut e = Engine::new(DspConfig::default(), "t").unwrap();
        let mut r = Rng::new(3);
        let quiet = |_: f64, r: &mut Rng| -60.0 + 0.4 * r.gauss();
        let rep = feed(&mut e, &mut r, "ap", 0.0, 60.0, 10.0, quiet);
        assert!(!rep.presence, "score={}", rep.presence_score);
        // walking: large slow excursions
        let walk = |t: f64, r: &mut Rng| -60.0 + 6.0 * (t * 1.3).sin() as f32 + 0.4 * r.gauss();
        let rep = feed(&mut e, &mut r, "ap", 60.0, 30.0, 10.0, walk);
        assert!(rep.presence, "score={}", rep.presence_score);
        let rep = feed(&mut e, &mut r, "ap", 90.0, 90.0, 10.0, quiet);
        assert!(!rep.presence, "score={}", rep.presence_score);
    }

    #[test]
    fn slow_scan_links_give_presence_but_never_breathing() {
        let mut e = Engine::new(DspConfig::default(), "t").unwrap();
        let mut r = Rng::new(5);
        // 4 APs scanned every 8 s
        let mut run = |e: &mut Engine, t0: f64, secs: f64, sigma: f32| {
            let mut last = None;
            let mut t = t0;
            let mut next = t0 + 2.0;
            while t < t0 + secs {
                for a in 0..4 {
                    e.push(Sample {
                        t,
                        link: format!("ap{a}"),
                        kind: Kind::Rssi,
                        value: (-60.0 - a as f32 * 5.0 + sigma * r.gauss()).round(),
                    });
                }
                t += 8.0;
                while next <= t {
                    last = Some(e.step(next));
                    next += 2.0;
                }
            }
            last.unwrap()
        };
        let rep = run(&mut e, 0.0, 240.0, 0.3);
        assert!(!rep.presence, "score {}", rep.presence_score);
        assert_eq!(rep.links_breath_capable, 0);
        assert_eq!(rep.mode, "presence-only");
        let rep = run(&mut e, 240.0, 120.0, 5.0);
        assert!(rep.presence, "score {}", rep.presence_score);
        assert!(!rep.breathing_detected);
    }

    #[test]
    fn frozen_baseline_does_not_creep() {
        let mut e = Engine::new(DspConfig::default(), "t").unwrap();
        let mut b = Baselines::default();
        b.links.insert("rssi:ap".into(), 0.4);
        e.load_baselines(&b);
        let mut r = Rng::new(1);
        // sustained moderate variation for 20 min must keep presence, baseline stays 0.4
        let rep = feed(&mut e, &mut r, "ap", 0.0, 1200.0, 5.0, |t, r| {
            -60.0 + 1.5 * (t * 0.9).sin() as f32 + 0.3 * r.gauss()
        });
        assert!(rep.presence);
        assert!(
            (e.baselines().links["rssi:ap"] - 0.4).abs() < 0.3,
            "{:?}",
            e.baselines()
        );
    }

    #[test]
    fn rejects_non_finite_and_out_of_order() {
        let mut e = Engine::new(DspConfig::default(), "t").unwrap();
        e.push(Sample {
            t: 1.0,
            link: "x".into(),
            kind: Kind::Rssi,
            value: f32::NAN,
        });
        e.push(Sample {
            t: f64::INFINITY,
            link: "x".into(),
            kind: Kind::Rssi,
            value: -50.0,
        });
        assert_eq!(e.step(2.0).links_active, 0);
        e.push(Sample {
            t: 5.0,
            link: "x".into(),
            kind: Kind::Rssi,
            value: -50.0,
        });
        e.push(Sample {
            t: 4.0,
            link: "x".into(),
            kind: Kind::Rssi,
            value: -10.0,
        });
        let rep = e.step(6.0);
        assert_eq!(rep.links.len(), 1);
        assert_eq!(rep.links[0].last_value, -50.0);
    }

    #[test]
    fn stale_links_are_dropped() {
        let mut e = Engine::new(DspConfig::default(), "t").unwrap();
        e.push(Sample {
            t: 0.0,
            link: "x".into(),
            kind: Kind::Rssi,
            value: -50.0,
        });
        assert_eq!(e.step(10.0).links_active, 1);
        assert_eq!(e.step(100.0).links_active, 0);
        assert_eq!(e.step(100.0).mode, "no-data");
    }

    #[test]
    fn reported_rate_matches_true_source_rate_from_the_first_window() {
        for hz in [10.0f64, 2.0, 0.5, 0.125] {
            let mut e = Engine::new(DspConfig::default(), "t").unwrap();
            let mut last = None;
            let mut next = 2.0;
            for i in 0..(60.0 * hz) as usize {
                let t = i as f64 / hz;
                e.push(Sample {
                    t,
                    link: "a".into(),
                    kind: Kind::Rssi,
                    value: -50.0,
                });
                if t >= next {
                    last = Some(e.step(t));
                    next += 2.0;
                }
            }
            let r = last.unwrap().sample_rate_hz;
            assert!((f64::from(r) - hz).abs() < hz * 0.1, "hz={hz} measured={r}");
        }
    }
}
