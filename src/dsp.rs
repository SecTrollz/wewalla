//! Sliding-window DSP primitives.
//!
//! Everything here is `f32`/`f64` scalar code written so LLVM auto-vectorises it
//! (NEON on aarch64): fixed 8-lane accumulators, contiguous tables, no
//! allocation after construction.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DspConfig {
    /// Uniform analysis rate for the breathing spectrum (Hz).
    pub fs_hz: f32,
    /// Breathing analysis window (s). Frequency resolution is 1/window.
    pub window_secs: f32,
    /// Report cadence (s).
    pub hop_secs: f32,
    pub band_lo_hz: f32,
    pub band_hi_hz: f32,
    pub band_step_hz: f32,
    /// Raw link sample rate required before a link may join the breathing spectrum.
    pub min_breath_rate_hz: f32,
    /// Fraction of analysis bins that must contain real samples.
    pub min_coverage: f32,
    /// Per-window probability that pure noise produces a breathing peak. The
    /// detection threshold is derived from this and the number of fused links.
    pub breath_false_alarm: f32,
    /// Presence window for links sampled >= 0.5 Hz (s).
    pub presence_window_secs: f32,
    /// Presence window for slow links such as Wi-Fi scans (s).
    pub presence_slow_window_secs: f32,
    pub presence_on: f32,
    pub presence_off: f32,
    pub presence_hold_secs: f32,
    /// A link with no sample for this long is dropped.
    pub stale_secs: f32,
    pub min_presence_samples: usize,
    /// Upward leak of the adaptive noise baseline per hop (fraction).
    pub baseline_leak_per_hop: f32,
}

impl Default for DspConfig {
    fn default() -> Self {
        Self {
            fs_hz: 2.0,
            window_secs: 30.0,
            hop_secs: 2.0,
            band_lo_hz: 0.10,
            band_hi_hz: 0.60,
            band_step_hz: 0.01,
            min_breath_rate_hz: 2.0,
            min_coverage: 0.7,
            breath_false_alarm: 0.005,
            presence_window_secs: 20.0,
            presence_slow_window_secs: 60.0,
            presence_on: 2.0,
            presence_off: 1.5,
            presence_hold_secs: 20.0,
            stale_secs: 45.0,
            min_presence_samples: 6,
            baseline_leak_per_hop: 0.0005,
        }
    }
}

impl DspConfig {
    /// Reject configurations that would make the DSP meaningless or unsafe to run.
    pub fn validate(&self) -> Result<(), String> {
        let f = |x: f32| x.is_finite() && x > 0.0;
        if !(f(self.fs_hz) && f(self.window_secs) && f(self.hop_secs) && f(self.band_step_hz)) {
            return Err(
                "fs_hz, window_secs, hop_secs, band_step_hz must be positive and finite".into(),
            );
        }
        if !(self.band_lo_hz > 0.0 && self.band_hi_hz > self.band_lo_hz) {
            return Err("band_lo_hz must be >0 and < band_hi_hz".into());
        }
        if self.band_hi_hz >= self.fs_hz / 2.0 {
            return Err("band_hi_hz must be below the Nyquist rate fs_hz/2".into());
        }
        let n = (self.window_secs * self.fs_hz).round();
        if !(16.0..=8192.0).contains(&n) {
            return Err("window_secs*fs_hz must be within 16..=8192 samples".into());
        }
        if self.band_step_hz > 1.0 / self.window_secs {
            return Err("band_step_hz coarser than the window's frequency resolution".into());
        }
        if self.presence_off > self.presence_on {
            return Err("presence_off must be <= presence_on".into());
        }
        if !(self.breath_false_alarm > 1e-9 && self.breath_false_alarm < 0.5) {
            return Err("breath_false_alarm must be within (1e-9, 0.5)".into());
        }
        if !(0.0..=1.0).contains(&self.min_coverage) {
            return Err("min_coverage must be within 0..=1".into());
        }
        if self.min_presence_samples < 3 {
            return Err("min_presence_samples must be >= 3".into());
        }
        if self.presence_slow_window_secs < self.presence_window_secs {
            return Err("presence_slow_window_secs must be >= presence_window_secs".into());
        }
        Ok(())
    }

    /// Longest history any stage needs (s).
    pub fn horizon_secs(&self) -> f32 {
        self.window_secs.max(self.presence_slow_window_secs)
    }
}

/// Dot product with 8 independent accumulators so the loop vectorises
/// without needing fast-math reassociation.
#[inline(always)]
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let ca = a.chunks_exact(8);
    let cb = b.chunks_exact(8);
    let (ra, rb) = (ca.remainder(), cb.remainder());
    let mut acc = [0f32; 8];
    for (x, y) in ca.zip(cb) {
        for i in 0..8 {
            acc[i] += x[i] * y[i];
        }
    }
    let mut s: f32 = acc.iter().sum();
    for (x, y) in ra.iter().zip(rb) {
        s += x * y;
    }
    s
}

/// Std-dev of the residual after a least-squares linear fit, over irregularly
/// spaced `(t, v)` points. Returns `(count, std)`. `t` should be shifted near zero.
pub fn detrended_std<I: Iterator<Item = (f64, f32)>>(it: I) -> (usize, f32) {
    let (mut n, mut st, mut sv, mut stt, mut stv, mut svv) = (0usize, 0f64, 0f64, 0f64, 0f64, 0f64);
    for (t, v) in it {
        let v = f64::from(v);
        n += 1;
        st += t;
        sv += v;
        stt += t * t;
        stv += t * v;
        svv += v * v;
    }
    if n < 2 {
        return (n, 0.0);
    }
    let nf = n as f64;
    let cvv = svv - sv * sv / nf;
    let ctt = stt - st * st / nf;
    let ctv = stv - st * sv / nf;
    let ss = if ctt > 1e-9 {
        cvv - ctv * ctv / ctt
    } else {
        cvv
    };
    ((n), (ss.max(0.0) / nf).sqrt() as f32)
}

/// Upper tail of Gamma(shape = l, scale = 1/l) at x: P(mean of l unit exponentials > x).
fn fused_tail(l: u32, x: f64) -> f64 {
    let y = f64::from(l) * x;
    let (mut term, mut sum) = (1.0f64, 1.0f64);
    for k in 1..l {
        term *= y / f64::from(k);
        sum += term;
    }
    (-y).exp() * sum
}

/// Peak/mean threshold such that, under noise only, P(any of `n_eff` independent bins
/// exceeds it) = `alpha`, for a spectrum averaged over `l` links.
pub fn fused_threshold(l: u32, n_eff: f64, alpha: f64) -> f32 {
    let l = l.clamp(1, 32);
    let (mut lo, mut hi) = (1.0f64, 200.0f64);
    for _ in 0..80 {
        let mid = 0.5 * (lo + hi);
        if n_eff * fused_tail(l, mid) > alpha {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    hi as f32
}

/// Monte-Carlo thresholds for 1..=8 fused links at the default configuration (30 s window, 2 Hz grid,
/// 0.10-0.60 Hz band at 0.01 Hz, false-alarm 0.005), worst case over 10/4/2 Hz source rates, from
/// `rvcsi-node selftest --calibrate --trials 10000` (white noise, sigma 1 dB, integer RSSI).
/// They replace the analytic Gamma-null value, which was optimistic from 4 links up
/// (measured false alarm ~1.5% against the 0.5% target).
pub const CALIBRATED_THRESHOLDS: [f32; 8] = [
    6.03277, 4.7639112, 4.034816, 3.653425, 3.3709118, 3.1489646, 2.9150064, 2.8115187,
];

impl DspConfig {
    /// True when `CALIBRATED_THRESHOLDS` describe this configuration.
    pub fn matches_calibration(&self) -> bool {
        let d = DspConfig::default();
        self.fs_hz == d.fs_hz
            && self.window_secs == d.window_secs
            && self.band_lo_hz == d.band_lo_hz
            && self.band_hi_hz == d.band_hi_hz
            && self.band_step_hz == d.band_step_hz
            && self.breath_false_alarm == d.breath_false_alarm
    }

    /// Detection threshold for `l` fused links: calibrated where a table applies, else analytic.
    /// Beyond 8 links the analytic curve is scaled by the 8-link calibrated/analytic ratio (extrapolated, untested).
    pub fn threshold_for(&self, l: usize) -> f32 {
        let n_eff = (f64::from(self.band_hi_hz - self.band_lo_hz) * f64::from(self.window_secs)
            / 1.5)
            .max(1.0);
        let analytic =
            |k: usize| fused_threshold(k as u32, n_eff, f64::from(self.breath_false_alarm));
        let l = l.clamp(1, 32);
        if !self.matches_calibration() {
            return analytic(l);
        }
        if l <= CALIBRATED_THRESHOLDS.len() {
            return CALIBRATED_THRESHOLDS[l - 1];
        }
        let n = CALIBRATED_THRESHOLDS.len();
        analytic(l) * (CALIBRATED_THRESHOLDS[n - 1] / analytic(n)).max(1.0)
    }
}

pub fn median(scratch: &mut [f32]) -> f32 {
    if scratch.is_empty() {
        return 0.0;
    }
    let mid = scratch.len() / 2;
    let (_, m, _) = scratch.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
    *m
}

/// Band-limited spectral analyser: resample -> detrend -> Hann -> direct DFT over
/// the breathing band only (51 bins vs. a full FFT), via precomputed tables.
#[derive(Debug)]
pub struct Spectral {
    n: usize,
    fs: f32,
    pub freqs: Vec<f32>,
    wcos: Vec<f32>,
    wsin: Vec<f32>,
    x: Vec<f32>,
    sum: Vec<f32>,
    cnt: Vec<u32>,
    power: Vec<f32>,
    pub snr: Vec<f32>,
    scratch: Vec<f32>,
}

impl Spectral {
    pub fn new(cfg: &DspConfig) -> Self {
        let n = (cfg.window_secs * cfg.fs_hz).round() as usize;
        let bins = ((cfg.band_hi_hz - cfg.band_lo_hz) / cfg.band_step_hz).round() as usize + 1;
        let freqs: Vec<f32> = (0..bins)
            .map(|k| cfg.band_lo_hz + k as f32 * cfg.band_step_hz)
            .collect();
        let mut wcos = vec![0f32; bins * n];
        let mut wsin = vec![0f32; bins * n];
        let tau = std::f64::consts::TAU;
        for (k, &f) in freqs.iter().enumerate() {
            for i in 0..n {
                let w = 0.5 - 0.5 * (tau * i as f64 / (n as f64 - 1.0)).cos();
                let ph = tau * f64::from(f) * i as f64 / f64::from(cfg.fs_hz);
                wcos[k * n + i] = (w * ph.cos()) as f32;
                wsin[k * n + i] = (w * ph.sin()) as f32;
            }
        }
        Self {
            n,
            fs: cfg.fs_hz,
            freqs,
            wcos,
            wsin,
            x: vec![0.0; n],
            sum: vec![0.0; n],
            cnt: vec![0; n],
            power: vec![0.0; bins],
            snr: vec![0.0; bins],
            scratch: vec![0.0; bins],
        }
    }

    pub fn bins(&self) -> usize {
        self.freqs.len()
    }

    /// Box-average the irregular samples into `n` uniform bins ending at `t_end`,
    /// linearly bridging gaps. Box averaging (not point sampling) is the
    /// anti-alias filter when the source runs faster than `fs`.
    /// Returns false when too few bins hold real data.
    pub fn load<'a, I: Iterator<Item = &'a (f64, f32)>>(
        &mut self,
        it: I,
        t_end: f64,
        min_cov: f32,
    ) -> bool {
        let n = self.n;
        let fs = f64::from(self.fs);
        let t0 = t_end - n as f64 / fs;
        self.sum.fill(0.0);
        self.cnt.fill(0);
        for &(t, v) in it {
            if t < t0 || t >= t_end || !v.is_finite() {
                continue;
            }
            let i = (((t - t0) * fs) as usize).min(n - 1);
            self.sum[i] += v;
            self.cnt[i] += 1;
        }
        let filled = self.cnt.iter().filter(|&&c| c > 0).count();
        if (filled as f32) < min_cov * n as f32 || filled < 2 {
            return false;
        }
        let mut prev: Option<usize> = None;
        for i in 0..n {
            if self.cnt[i] == 0 {
                continue;
            }
            let v = self.sum[i] / self.cnt[i] as f32;
            self.x[i] = v;
            match prev {
                Some(p) if i - p > 1 => {
                    let vp = self.x[p];
                    let span = (i - p) as f32;
                    for j in p + 1..i {
                        self.x[j] = vp + (v - vp) * ((j - p) as f32 / span);
                    }
                }
                None => self.x[..i].fill(v),
                _ => {}
            }
            prev = Some(i);
        }
        if let Some(p) = prev {
            let vp = self.x[p];
            self.x[p + 1..].fill(vp);
        }
        true
    }

    /// Detrend the loaded series and compute per-bin power normalised by the
    /// median bin (a noise-floor proxy). Returns false for a flat series.
    pub fn compute_snr(&mut self) -> bool {
        let n = self.n;
        let nf = n as f32;
        let mean = self.x.iter().sum::<f32>() / nf;
        let (mut sxy, mut sxx) = (0f32, 0f32);
        let c = (nf - 1.0) / 2.0;
        for (i, v) in self.x.iter().enumerate() {
            let d = i as f32 - c;
            sxy += d * (v - mean);
            sxx += d * d;
        }
        let slope = if sxx > 0.0 { sxy / sxx } else { 0.0 };
        let mut energy = 0f32;
        for (i, v) in self.x.iter_mut().enumerate() {
            *v -= mean + slope * (i as f32 - c);
            energy += *v * *v;
        }
        if energy / nf < 1e-10 {
            return false;
        }
        for k in 0..self.power.len() {
            let re = dot(&self.x, &self.wcos[k * n..(k + 1) * n]);
            let im = dot(&self.x, &self.wsin[k * n..(k + 1) * n]);
            self.power[k] = re * re + im * im;
        }
        self.scratch.copy_from_slice(&self.power);
        let med = median(&mut self.scratch).max(f32::MIN_POSITIVE * 1e6);
        for (s, p) in self.snr.iter_mut().zip(&self.power) {
            *s = p / med;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        DspConfig::default().validate().unwrap();
    }

    #[test]
    fn invalid_configs_rejected() {
        let mut c = DspConfig::default();
        c.band_hi_hz = 2.5;
        assert!(c.validate().is_err());
        let mut c = DspConfig::default();
        c.fs_hz = f32::NAN;
        assert!(c.validate().is_err());
        let mut c = DspConfig::default();
        c.presence_off = 3.0;
        assert!(c.validate().is_err());
        let mut c = DspConfig::default();
        c.band_step_hz = 0.5;
        assert!(c.validate().is_err());
    }

    #[test]
    fn fused_threshold_falls_with_more_links_and_matches_single_link_closed_form() {
        let t1 = fused_threshold(1, 10.0, 0.005);
        assert!(
            (f64::from(t1) - (10.0f64 / 0.005).ln()).abs() < 1e-3,
            "{t1}"
        );
        let t4 = fused_threshold(4, 10.0, 0.005);
        let t16 = fused_threshold(16, 10.0, 0.005);
        assert!(t1 > t4 && t4 > t16 && t16 > 1.0, "{t1} {t4} {t16}");
    }

    #[test]
    fn dot_matches_naive() {
        let a: Vec<f32> = (0..37).map(|i| i as f32 * 0.5).collect();
        let b: Vec<f32> = (0..37).map(|i| 1.0 - i as f32 * 0.01).collect();
        let naive: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert!((dot(&a, &b) - naive).abs() < 1e-2);
    }

    #[test]
    fn detrended_std_ignores_linear_drift_and_sees_noise() {
        let drift = (0..100).map(|i| (i as f64 * 0.1, -50.0 + i as f32 * 0.05));
        assert!(detrended_std(drift).1 < 1e-3);
        let sq = (0..100).map(|i| (i as f64 * 0.1, if i % 2 == 0 { 1.0 } else { -1.0 }));
        let (n, s) = detrended_std(sq);
        assert_eq!(n, 100);
        assert!((s - 1.0).abs() < 0.05);
    }

    #[test]
    fn spectrum_peaks_at_tone() {
        let cfg = DspConfig::default();
        let mut sp = Spectral::new(&cfg);
        let f0 = 0.27f32;
        let t_end = 100.0;
        let pts: Vec<(f64, f32)> = (0..400)
            .map(|i| {
                let t = t_end - 40.0 + i as f64 * 0.1;
                (
                    t,
                    -55.0 + (std::f64::consts::TAU * f64::from(f0) * t).sin() as f32,
                )
            })
            .collect();
        assert!(sp.load(pts.iter(), t_end, 0.7));
        assert!(sp.compute_snr());
        let k = sp
            .snr
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(k, _)| k)
            .unwrap();
        assert!((sp.freqs[k] - f0).abs() <= 0.02, "peak at {}", sp.freqs[k]);
    }

    #[test]
    fn load_rejects_sparse_data() {
        let cfg = DspConfig::default();
        let mut sp = Spectral::new(&cfg);
        let pts: Vec<(f64, f32)> = (0..5).map(|i| (90.0 + i as f64 * 2.0, -50.0)).collect();
        assert!(!sp.load(pts.iter(), 100.0, 0.7));
    }

    #[test]
    fn flat_series_is_rejected_not_nan() {
        let cfg = DspConfig::default();
        let mut sp = Spectral::new(&cfg);
        let pts: Vec<(f64, f32)> = (0..300).map(|i| (70.0 + i as f64 * 0.1, -50.0)).collect();
        assert!(sp.load(pts.iter(), 100.0, 0.7));
        assert!(!sp.compute_snr());
    }
}
