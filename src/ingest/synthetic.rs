//! Deterministic stand-in link for bring-up, soak tests and the benchmark. It is
//! labelled `synthetic` everywhere it surfaces; it is never auto-selected.

use super::Source;
use crate::engine::Rng;
use crate::sample::{now, Kind, Sample};
use std::time::Duration;

#[derive(Debug)]
pub struct Synthetic {
    pub hz: f32,
    pub links: usize,
    pub bpm: f32,
    pub amp: f32,
    pub noise: f32,
    pub present: bool,
    rng: Rng,
}

impl Synthetic {
    pub fn new(
        hz: f32,
        links: usize,
        bpm: f32,
        amp: f32,
        noise: f32,
        present: bool,
        seed: u64,
    ) -> Self {
        Self {
            hz,
            links,
            bpm,
            amp,
            noise,
            present,
            rng: Rng::new(seed),
        }
    }
}

impl Source for Synthetic {
    fn name(&self) -> String {
        "synthetic".into()
    }
    fn period(&self) -> Duration {
        Duration::from_secs_f32(1.0 / self.hz)
    }
    fn poll(&mut self, out: &mut Vec<Sample>) -> Result<(), String> {
        let t = now();
        let s = (std::f64::consts::TAU * f64::from(self.bpm) / 60.0 * t).sin() as f32;
        for l in 0..self.links {
            let body = if self.present { self.amp * s } else { 0.0 };
            let v = (-55.0 - l as f32 * 3.0 + body + self.noise * self.rng.gauss()).round();
            out.push(Sample {
                t,
                link: format!("syn{l}"),
                kind: Kind::Rssi,
                value: v,
            });
        }
        Ok(())
    }
}
