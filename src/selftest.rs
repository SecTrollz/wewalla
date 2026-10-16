//! Statistical self-test: measures the detector's real false-alarm and detection rates on
//! simulated links, so the numbers quoted in the docs can be re-derived by anyone (including on
//! the phone itself) instead of taken on trust.

use crate::dsp::DspConfig;
use crate::engine::{Engine, Report, Rng};
use crate::sample::{Kind, Sample};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Cell {
    pub links: usize,
    pub amp_db: f32,
    pub sigma_db: f32,
    pub trials: usize,
    /// Fraction of trials whose final window exceeded the per-window threshold.
    pub raw_rate: f64,
    /// Fraction of trials that reported `breathing_detected` (2-of-3 hops + edge rule).
    pub reported_rate: f64,
    /// Median reported bpm error among detected trials (only when amp > 0).
    pub median_bpm_err: Option<f32>,
}

#[derive(Debug, Serialize)]
pub struct Table {
    pub target_false_alarm: f32,
    pub hz: f64,
    pub rows: Vec<Cell>,
}

fn last_report(
    cfg: &DspConfig,
    links: usize,
    bpm: f32,
    amp: f32,
    sigma: f32,
    hz: f64,
    seed: u64,
) -> Option<Report> {
    let mut e = Engine::new(cfg.clone(), "selftest").ok()?;
    let mut rng = Rng::new(seed);
    let hop = f64::from(cfg.hop_secs);
    let total = f64::from(cfg.window_secs) + 3.0 * hop + 4.0;
    let mut next = hop;
    let phase = rng.uniform() * std::f32::consts::TAU;
    let mut last = None;
    let n = (total * hz) as usize;
    for i in 0..n {
        let t = i as f64 / hz;
        let s = (std::f64::consts::TAU * f64::from(bpm) / 60.0 * t) as f32 + phase;
        for l in 0..links {
            let v = (-55.0 + amp * s.sin() + sigma * rng.gauss()).round();
            e.push(Sample {
                t,
                link: format!("l{l}"),
                kind: Kind::Rssi,
                value: v,
            });
        }
        if t >= next {
            last = Some(e.step(t));
            next += hop;
        }
    }
    last
}

/// (raw window hit, reported detection, reported bpm). A raw hit needs an interior peak, as in the engine.
fn trial(
    cfg: &DspConfig,
    links: usize,
    bpm: f32,
    amp: f32,
    sigma: f32,
    hz: f64,
    seed: u64,
) -> (bool, bool, Option<f32>) {
    match last_report(cfg, links, bpm, amp, sigma, hz, seed) {
        Some(r) => (
            r.dominant_freq_hz.is_some() && r.breathing_conf >= r.breathing_threshold,
            r.breathing_detected,
            r.breathing_bpm,
        ),
        None => (false, false, None),
    }
}

/// Fraction of pure-noise windows that exceed the engine's own threshold.
pub fn null_raw_rate(cfg: &DspConfig, links: usize, hz: f64, trials: usize, seed_base: u64) -> f64 {
    let hits = (0..trials)
        .filter(|&k| {
            trial(
                cfg,
                links,
                15.0,
                0.0,
                1.0,
                hz,
                seed_base + k as u64 * 7 + links as u64,
            )
            .0
        })
        .count();
    hits as f64 / trials.max(1) as f64
}

/// Monte-Carlo threshold for each link count 1..=`max_links`: the (1 - alpha) quantile of the null
/// statistic (peak/mean, zero when the peak sits on the band edge), taking the worst case over the
/// source rates a phone realistically delivers. `thresholds` must be computed with the analytic
/// table turned off (they use the raw statistic, not the thresholded decision).
pub fn calibrate(
    cfg: &DspConfig,
    trials: usize,
    seed_base: u64,
    max_links: usize,
    rates: &[f64],
) -> Vec<f32> {
    (1..=max_links)
        .map(|links| {
            rates
                .iter()
                .map(|&hz| {
                    let mut stats: Vec<f32> = (0..trials)
                        .map(|k| {
                            last_report(
                                cfg,
                                links,
                                15.0,
                                0.0,
                                1.0,
                                hz,
                                seed_base + k as u64 * 7 + links as u64,
                            )
                            .map_or(0.0, |r| {
                                if r.dominant_freq_hz.is_some() {
                                    r.breathing_conf
                                } else {
                                    0.0
                                }
                            })
                        })
                        .collect();
                    stats.sort_by(f32::total_cmp);
                    let idx = (((1.0 - f64::from(cfg.breath_false_alarm)) * trials as f64).ceil()
                        as usize)
                        .clamp(1, trials)
                        - 1;
                    stats[idx]
                })
                .fold(0.0f32, f32::max)
        })
        .collect()
}

pub fn run(trials: usize, hz: f64) -> Table {
    let cfg = DspConfig::default();
    let mut rows = Vec::new();
    let sigma = 1.0;
    for links in [1usize, 2, 4] {
        for amp in [0.0f32, 0.2, 0.4, 0.8] {
            let (mut raw, mut rep) = (0usize, 0usize);
            let mut errs = Vec::new();
            for k in 0..trials {
                let bpm = 12.0 + (k % 13) as f32; // 12..24 bpm, spread over the band
                let (r, d, b) = trial(
                    &cfg,
                    links,
                    bpm,
                    amp,
                    sigma,
                    hz,
                    1_000 + k as u64 * 7 + links as u64,
                );
                raw += usize::from(r);
                rep += usize::from(d);
                if amp > 0.0 && d {
                    if let Some(b) = b {
                        errs.push((b - bpm).abs());
                    }
                }
            }
            errs.sort_by(f32::total_cmp);
            rows.push(Cell {
                links,
                amp_db: amp,
                sigma_db: sigma,
                trials,
                raw_rate: raw as f64 / trials as f64,
                reported_rate: rep as f64 / trials as f64,
                median_bpm_err: errs.get(errs.len() / 2).copied(),
            });
        }
    }
    Table {
        target_false_alarm: cfg.breath_false_alarm,
        hz,
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Out-of-sample: seeds 9_000_000+ were never used for calibration (5_000_000+). Bound is 2x the target
    /// false-alarm rate; with 3000 trials that is >4 binomial sigmas above the expected count, so a pass is
    /// not luck and a failure is a real regression. Slow in debug builds, so run with `--release` (CI does).
    #[test]
    #[cfg_attr(debug_assertions, ignore = "statistical; run with --release")]
    fn calibrated_false_alarm_holds_out_of_sample() {
        let cfg = DspConfig::default();
        let limit = 2.0 * f64::from(cfg.breath_false_alarm);
        for links in [1usize, 2, 4, 8] {
            for hz in [10.0, 4.0, 2.0] {
                let rate = null_raw_rate(&cfg, links, hz, 3000, 9_000_000);
                eprintln!(
                    "links={links} hz={hz} false-alarm={rate:.4} (target {}, limit {limit})",
                    cfg.breath_false_alarm
                );
                assert!(rate <= limit, "links={links} hz={hz}: {rate} > {limit}");
            }
        }
    }

    #[test]
    fn calibrated_table_is_monotone_and_only_applies_to_its_configuration() {
        let cfg = DspConfig::default();
        assert!(cfg.matches_calibration());
        let t: Vec<f32> = (1..=12).map(|l| cfg.threshold_for(l)).collect();
        assert!(
            t.windows(2).all(|w| w[0] > w[1] - 1e-3),
            "thresholds must not rise with more links: {t:?}"
        );
        let mut other = cfg.clone();
        other.band_hi_hz = 0.55;
        assert!(!other.matches_calibration());
        let n_eff =
            f64::from(other.band_hi_hz - other.band_lo_hz) * f64::from(other.window_secs) / 1.5;
        let analytic = crate::dsp::fused_threshold(1, n_eff, f64::from(other.breath_false_alarm));
        assert_eq!(
            other.threshold_for(1),
            analytic,
            "an uncalibrated configuration must use the analytic threshold"
        );
        assert_ne!(
            cfg.threshold_for(1),
            crate::dsp::fused_threshold(1, 10.0, f64::from(cfg.breath_false_alarm)),
            "the default configuration must use the calibrated table"
        );
    }

    #[test]
    fn detection_improves_with_links_and_amplitude() {
        let cfg = DspConfig::default();
        let det = |links, amp| {
            (0..120)
                .filter(|k| {
                    trial(
                        &cfg,
                        links,
                        12.0 + (k % 13) as f32,
                        amp,
                        1.0,
                        10.0,
                        20_000 + *k as u64,
                    )
                    .1
                })
                .count()
        };
        assert!(det(4, 0.8) >= det(1, 0.8));
        assert!(det(4, 0.8) > det(4, 0.2));
    }
}
