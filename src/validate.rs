//! Field validation: turns reports from a live (or replayed) run into PASS/FAIL verdicts, so claims about
//! detection are tested on the actual device instead of asserted.
//!
//! Measurement rules (all deliberate):
//!  * The first `settle_secs` of every collection is discarded: the engine's windows (20-30 s) still hold
//!    the previous situation.
//!  * Range cells use the engine's *raw* per-window statistics (score over the presence threshold, spectral
//!    peak over the breathing threshold), not the debounced `presence` / `breathing_detected` flags, which
//!    deliberately linger after a person leaves and would leak one cell into the next.
//!  * The empty-room control uses the debounced flags, because that is what a user would see as a false alarm.
//!  * Results from a failed control are marked invalid, never "passed with caveats".

use crate::dsp::{median, DspConfig};
use crate::engine::Report;
use serde::Serialize;

#[derive(Clone, Debug)]
pub struct Criteria {
    pub settle_secs: f64,
    pub hop_secs: f64,
    pub presence_on: f32,
    pub min_reports: usize,
    pub max_presence_fa: f64,
    pub max_breath_fa: f64,
    pub min_presence_frac: f64,
    pub min_breath_frac: f64,
    pub bpm_tolerance: f32,
}

impl Criteria {
    pub fn from_cfg(c: &DspConfig) -> Self {
        Self {
            settle_secs: 40.0,
            hop_secs: f64::from(c.hop_secs),
            presence_on: c.presence_on,
            min_reports: 10,
            max_presence_fa: 0.05,
            max_breath_fa: 0.02,
            min_presence_frac: 0.8,
            min_breath_frac: 0.6,
            bpm_tolerance: 1.5,
        }
    }
    fn settle_n(&self) -> usize {
        (self.settle_secs / self.hop_secs).ceil() as usize
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Stats {
    pub reports: usize,
    /// Debounced flags (what a user sees).
    pub presence_flag_frac: f64,
    pub breath_flag_frac: f64,
    /// Raw per-window statistics (used for range cells).
    pub presence_raw_frac: f64,
    pub breath_raw_frac: f64,
    /// Median spectral-peak rate over raw breathing hits, breaths/min.
    pub peak_bpm_median: Option<f32>,
    pub mean_links: f32,
    pub mean_sample_rate_hz: f32,
}

fn frac(n: usize, d: usize) -> f64 {
    if d == 0 {
        0.0
    } else {
        n as f64 / d as f64
    }
}

fn raw_breath(r: &Report) -> bool {
    r.dominant_freq_hz.is_some() && r.breathing_conf >= r.breathing_threshold
}

pub fn stats(reports: &[Report], c: &Criteria) -> Stats {
    let n = reports.len();
    let mut bpms: Vec<f32> = reports
        .iter()
        .filter(|r| raw_breath(r))
        .filter_map(|r| r.dominant_freq_hz)
        .map(|f| f * 60.0)
        .collect();
    Stats {
        reports: n,
        presence_flag_frac: frac(reports.iter().filter(|r| r.presence).count(), n),
        breath_flag_frac: frac(reports.iter().filter(|r| r.breathing_detected).count(), n),
        presence_raw_frac: frac(
            reports
                .iter()
                .filter(|r| r.presence_score >= c.presence_on)
                .count(),
            n,
        ),
        breath_raw_frac: frac(reports.iter().filter(|r| raw_breath(r)).count(), n),
        peak_bpm_median: if bpms.is_empty() {
            None
        } else {
            Some(median(&mut bpms))
        },
        mean_links: if n == 0 {
            0.0
        } else {
            reports.iter().map(|r| r.links_active as f32).sum::<f32>() / n as f32
        },
        mean_sample_rate_hz: if n == 0 {
            0.0
        } else {
            reports.iter().map(|r| r.sample_rate_hz).sum::<f32>() / n as f32
        },
    }
}

fn steady<'a>(reports: &'a [Report], c: &Criteria) -> &'a [Report] {
    reports.get(c.settle_n()..).unwrap_or(&[])
}

#[derive(Debug, Clone, Serialize)]
pub struct EmptyResult {
    pub stats: Stats,
    pub pass: bool,
    pub reasons: Vec<String>,
}

pub fn eval_empty(reports: &[Report], c: &Criteria) -> EmptyResult {
    let s = stats(steady(reports, c), c);
    let mut reasons = Vec::new();
    if s.reports < c.min_reports {
        reasons.push(format!(
            "insufficient data: {} steady reports (< {})",
            s.reports, c.min_reports
        ));
    }
    if s.presence_flag_frac > c.max_presence_fa {
        reasons.push(format!(
            "presence false alarms {:.1}% > {:.1}%",
            s.presence_flag_frac * 100.0,
            c.max_presence_fa * 100.0
        ));
    }
    if s.breath_flag_frac > c.max_breath_fa {
        reasons.push(format!(
            "breathing false alarms {:.1}% > {:.1}%",
            s.breath_flag_frac * 100.0,
            c.max_breath_fa * 100.0
        ));
    }
    EmptyResult {
        pass: reasons.is_empty(),
        stats: s,
        reasons,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PacedResult {
    pub stats: Stats,
    pub target_bpm: f32,
    pub bpm_error: Option<f32>,
    pub pass: bool,
    pub reasons: Vec<String>,
}

pub fn eval_paced(reports: &[Report], target_bpm: f32, c: &Criteria) -> PacedResult {
    let s = stats(steady(reports, c), c);
    let err = s.peak_bpm_median.map(|b| (b - target_bpm).abs());
    let mut reasons = Vec::new();
    if s.reports < c.min_reports {
        reasons.push(format!(
            "insufficient data: {} steady reports (< {})",
            s.reports, c.min_reports
        ));
    }
    if s.breath_raw_frac < c.min_breath_frac {
        reasons.push(format!(
            "breathing seen in {:.0}% of windows < {:.0}%",
            s.breath_raw_frac * 100.0,
            c.min_breath_frac * 100.0
        ));
    }
    match err {
        Some(e) if e > c.bpm_tolerance => reasons.push(format!(
            "median rate off by {e:.2} bpm > {:.1}",
            c.bpm_tolerance
        )),
        None => reasons.push("no breathing peak found".into()),
        _ => {}
    }
    PacedResult {
        pass: reasons.is_empty(),
        stats: s,
        target_bpm,
        bpm_error: err,
        reasons,
    }
}

// ---------------------------------------------------------------------------------------- range

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Control,
    Cell { bearing: String, feet: u32 },
    Paced { bpm: f32, feet: Option<u32> },
}

impl Step {
    pub fn instruction(&self) -> String {
        match self {
            Step::Control => "EMPTY-ROOM CONTROL: everyone leaves the sensing area (the room, and 40 ft around the phone). Do not touch the phone.".into(),
            Step::Cell { bearing, feet } => format!("Stand {feet} ft from the phone toward {bearing}. Stay in place, move naturally (shift weight, gesture, breathe normally)."),
            Step::Paced { bpm, feet } => format!(
                "Sit still{} and breathe in time with a {bpm} breaths/min metronome (one full breath every {:.1} s).",
                feet.map_or(String::new(), |f| format!(" {f} ft from the phone")),
                60.0 / bpm
            ),
        }
    }
}

/// Supplies reports for a step. The live implementation prompts the operator and runs the real sources;
/// tests drive a simulated engine.
pub trait Collector {
    fn instruct(&mut self, step: &Step) -> Result<(), String>;
    fn collect(&mut self, secs: f64) -> Result<Vec<Report>, String>;
}

#[derive(Debug, Clone)]
pub struct RangeParams {
    pub bearings: Vec<String>,
    pub distances_ft: Vec<u32>,
    pub cell_secs: f64,
    pub control_secs: f64,
    pub target_ft: u32,
    pub criteria: Criteria,
}

impl RangeParams {
    pub fn validate(&self) -> Result<(), String> {
        if self.bearings.is_empty() || self.bearings.len() > 16 {
            return Err("bearings: 1..=16".into());
        }
        if self.distances_ft.is_empty()
            || self.distances_ft.len() > 12
            || self.distances_ft.iter().any(|d| !(1..=300).contains(d))
        {
            return Err("distances: 1..=12 values, each 1..=300 ft".into());
        }
        let need =
            self.criteria.settle_secs + self.criteria.min_reports as f64 * self.criteria.hop_secs;
        if self.cell_secs < need || self.control_secs < need {
            return Err(format!(
                "cell/control seconds must be >= {need:.0} (settle + {} reports)",
                self.criteria.min_reports
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CellResult {
    pub bearing: String,
    pub feet: u32,
    pub stats: Stats,
    pub enough_data: bool,
    pub presence_pass: bool,
    pub breathing_pass: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Coverage {
    pub bearing: String,
    /// Farthest distance reached with every nearer cell also passing; None if even the nearest failed.
    pub presence_reach_ft: Option<u32>,
    pub breathing_reach_ft: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RangeResult {
    pub control: EmptyResult,
    /// False when the empty-room control failed: nothing below can be trusted.
    pub valid: bool,
    pub target_ft: u32,
    pub cells: Vec<CellResult>,
    pub coverage: Vec<Coverage>,
    pub presence_target_met: bool,
    pub breathing_target_met: bool,
}

fn reach(cells: &[&CellResult], pass: impl Fn(&CellResult) -> bool) -> Option<u32> {
    let mut best = None;
    for c in cells {
        if pass(c) {
            best = Some(c.feet);
        } else {
            break;
        }
    }
    best
}

pub fn run_range(col: &mut dyn Collector, p: &RangeParams) -> Result<RangeResult, String> {
    p.validate()?;
    let c = &p.criteria;
    col.instruct(&Step::Control)?;
    let control = eval_empty(&col.collect(p.control_secs)?, c);
    let mut distances = p.distances_ft.clone();
    distances.sort_unstable();
    distances.dedup();
    let mut cells = Vec::new();
    for bearing in &p.bearings {
        for &feet in &distances {
            col.instruct(&Step::Cell {
                bearing: bearing.clone(),
                feet,
            })?;
            let reports = col.collect(p.cell_secs)?;
            let s = stats(steady(&reports, c), c);
            let enough = s.reports >= c.min_reports;
            cells.push(CellResult {
                bearing: bearing.clone(),
                feet,
                presence_pass: enough && s.presence_raw_frac >= c.min_presence_frac,
                breathing_pass: enough && s.breath_raw_frac >= c.min_breath_frac,
                enough_data: enough,
                stats: s,
            });
        }
    }
    let coverage: Vec<Coverage> = p
        .bearings
        .iter()
        .map(|b| {
            let mine: Vec<&CellResult> = cells.iter().filter(|c| &c.bearing == b).collect();
            Coverage {
                bearing: b.clone(),
                presence_reach_ft: reach(&mine, |c| c.presence_pass),
                breathing_reach_ft: reach(&mine, |c| c.breathing_pass),
            }
        })
        .collect();
    let valid = control.pass;
    let met = |f: fn(&Coverage) -> Option<u32>| {
        valid
            && coverage
                .iter()
                .all(|c| f(c).is_some_and(|r| r >= p.target_ft))
    };
    Ok(RangeResult {
        valid,
        target_ft: p.target_ft,
        presence_target_met: met(|c| c.presence_reach_ft),
        breathing_target_met: met(|c| c.breathing_reach_ft),
        coverage,
        cells,
        control,
    })
}

pub fn bearing_labels(n: usize) -> Vec<String> {
    match n {
        4 => ["N", "E", "S", "W"]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        8 => ["N", "NE", "E", "SE", "S", "SW", "W", "NW"]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        n => (0..n)
            .map(|i| format!("{}deg", i * 360 / n.max(1)))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, Rng};
    use crate::sample::{Kind, Sample};

    /// Simulated world: `plan` decides, per step, the breathing amplitude and movement amplitude (dB).
    struct Sim<F: Fn(&Step) -> (f32, f32)> {
        engine: Engine,
        rng: Rng,
        t: f64,
        step: Step,
        plan: F,
        bpm: f32,
        links: usize,
        /// Movement is broadband (AR(1), ~1 s correlation), never a pure tone: a tone inside 6-36 cycles/min
        /// is genuinely indistinguishable from breathing for this detector.
        mv: f32,
    }

    impl<F: Fn(&Step) -> (f32, f32)> Sim<F> {
        fn new(plan: F, bpm: f32, links: usize) -> Self {
            Self {
                engine: Engine::new(DspConfig::default(), "sim").unwrap(),
                rng: Rng::new(77),
                t: 0.0,
                step: Step::Control,
                plan,
                bpm,
                links,
                mv: 0.0,
            }
        }
    }

    impl<F: Fn(&Step) -> (f32, f32)> Collector for Sim<F> {
        fn instruct(&mut self, step: &Step) -> Result<(), String> {
            self.step = step.clone();
            Ok(())
        }
        fn collect(&mut self, secs: f64) -> Result<Vec<Report>, String> {
            let (breath, motion) = (self.plan)(&self.step);
            let (hz, hop) = (10.0, 2.0);
            let end = self.t + secs;
            let mut next = self.t + hop;
            let mut out = Vec::new();
            while self.t < end {
                let b = (std::f64::consts::TAU * f64::from(self.bpm) / 60.0 * self.t).sin() as f32;
                self.mv = 0.9 * self.mv + 0.436 * self.rng.gauss();
                let m = self.mv;
                for l in 0..self.links {
                    let v = -55.0 + breath * b + motion * m + self.rng.gauss();
                    self.engine.push(Sample {
                        t: self.t,
                        link: format!("l{l}"),
                        kind: Kind::Rssi,
                        value: v.round(),
                    });
                }
                self.t += 1.0 / hz;
                if self.t >= next {
                    out.push(self.engine.step(self.t));
                    next += hop;
                }
            }
            Ok(out)
        }
    }

    fn crit() -> Criteria {
        Criteria::from_cfg(&DspConfig::default())
    }

    fn params(bearings: &[&str], distances: &[u32]) -> RangeParams {
        RangeParams {
            bearings: bearings.iter().map(|s| (*s).to_string()).collect(),
            distances_ft: distances.to_vec(),
            cell_secs: 100.0,
            control_secs: 100.0,
            target_ft: 40,
            criteria: crit(),
        }
    }

    #[test]
    fn empty_room_passes_and_a_busy_room_fails() {
        let mut quiet = Sim::new(|_| (0.0, 0.0), 15.0, 3);
        let r = eval_empty(&quiet.collect(120.0).unwrap(), &crit());
        assert!(r.pass, "{:?}", r.reasons);
        let mut busy = Sim::new(|_| (0.0, 5.0), 15.0, 3);
        let r = eval_empty(&busy.collect(120.0).unwrap(), &crit());
        assert!(
            !r.pass && r.reasons.iter().any(|m| m.contains("presence")),
            "{:?}",
            r.reasons
        );
    }

    #[test]
    fn too_little_data_is_a_failure_not_a_pass() {
        let mut quiet = Sim::new(|_| (0.0, 0.0), 15.0, 3);
        let r = eval_empty(&quiet.collect(40.0).unwrap(), &crit());
        assert!(
            !r.pass && r.reasons[0].contains("insufficient"),
            "{:?}",
            r.reasons
        );
    }

    #[test]
    fn paced_breathing_passes_at_the_right_rate_and_fails_at_the_wrong_one() {
        let mut s = Sim::new(|_| (1.0, 0.0), 12.0, 4);
        let reports = s.collect(150.0).unwrap();
        let ok = eval_paced(&reports, 12.0, &crit());
        assert!(ok.pass, "{:?} {:?}", ok.reasons, ok.stats);
        assert!(ok.bpm_error.unwrap() < 1.0);
        let wrong = eval_paced(&reports, 20.0, &crit());
        assert!(
            !wrong.pass && wrong.reasons.iter().any(|m| m.contains("off by")),
            "{:?}",
            wrong.reasons
        );
        let mut none = Sim::new(|_| (0.0, 0.0), 12.0, 4);
        assert!(!eval_paced(&none.collect(150.0).unwrap(), 12.0, &crit()).pass);
    }

    #[test]
    fn range_coverage_is_contiguous_and_per_bearing() {
        // N: detected everywhere. E: misses at 20 ft but is "detected" again at 40 ft (a hole). W: dies past 20 ft.
        let plan = |s: &Step| match s {
            Step::Cell { bearing, feet } => match (bearing.as_str(), *feet) {
                ("N", _) => (0.0, 6.0),
                ("E", 20) => (0.0, 0.0),
                ("E", _) => (0.0, 6.0),
                ("W", f) if f <= 10 => (0.0, 6.0),
                _ => (0.0, 0.0),
            },
            _ => (0.0, 0.0),
        };
        let mut sim = Sim::new(plan, 15.0, 3);
        let res = run_range(&mut sim, &params(&["N", "E", "W"], &[5, 10, 20, 40])).unwrap();
        assert!(res.valid, "control failed: {:?}", res.control.reasons);
        let cov = |b: &str| {
            res.coverage
                .iter()
                .find(|c| c.bearing == b)
                .unwrap()
                .presence_reach_ft
        };
        assert_eq!(cov("N"), Some(40));
        assert_eq!(
            cov("E"),
            Some(10),
            "a hole at 20 ft ends contiguous reach even though 40 ft passes"
        );
        assert_eq!(cov("W"), Some(10));
        assert!(!res.presence_target_met, "W and E do not reach 40 ft");
        assert_eq!(res.cells.len(), 12);
    }

    #[test]
    fn range_target_met_only_when_every_bearing_reaches_it() {
        let mut sim = Sim::new(
            |s| {
                if matches!(s, Step::Cell { .. }) {
                    (0.0, 6.0)
                } else {
                    (0.0, 0.0)
                }
            },
            15.0,
            3,
        );
        let res = run_range(&mut sim, &params(&["N", "S"], &[10, 40])).unwrap();
        assert!(res.valid && res.presence_target_met);
        assert!(
            !res.breathing_target_met,
            "no breathing was simulated, so the breathing target must not be reported met"
        );
    }

    #[test]
    fn failed_control_invalidates_everything() {
        let mut sim = Sim::new(|_| (0.0, 6.0), 15.0, 3); // someone is moving during the "empty" control
        let res = run_range(&mut sim, &params(&["N"], &[5, 40])).unwrap();
        assert!(!res.valid && !res.control.pass);
        assert!(
            !res.presence_target_met && !res.breathing_target_met,
            "an invalid run can never meet the target"
        );
    }

    #[test]
    fn parameters_are_validated() {
        let mut p = params(&["N"], &[5]);
        p.cell_secs = 20.0;
        assert!(p.validate().is_err());
        let mut p = params(&["N"], &[0]);
        assert!(p.validate().is_err());
        p = params(&[], &[5]);
        assert!(p.validate().is_err());
        assert_eq!(bearing_labels(8).len(), 8);
        assert_eq!(bearing_labels(3), ["0deg", "120deg", "240deg"]);
    }

    #[test]
    fn instructions_are_actionable() {
        assert!(Step::Cell {
            bearing: "NE".into(),
            feet: 30
        }
        .instruction()
        .contains("30 ft"));
        assert!(Step::Paced {
            bpm: 12.0,
            feet: None
        }
        .instruction()
        .contains("5.0 s"));
    }
}
