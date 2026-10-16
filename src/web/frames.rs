//! Wire shapes for the retained web UI. Field names follow what the UI reads; values
//! are only ever measured quantities. Nothing is fabricated: no node positions, no
//! synthetic signal field, no pose.

use crate::engine::{LinkInfo, Report};
use crate::sample::Kind;
use serde_json::{json, Value};

fn motion_level(r: &Report, presence_on: f32) -> &'static str {
    if !r.presence {
        "absent"
    } else if r.presence_score >= presence_on * 2.0 {
        "active"
    } else {
        "present_still"
    }
}

fn node(i: usize, l: &LinkInfo) -> Value {
    let mut n = json!({
        "node_id": i + 1,
        "link": l.id,
        "kind": l.kind,
        "value": l.last_value,
        "rate_hz": l.rate_hz,
        "amplitude": [],
        "subcarrier_count": 0,
    });
    if l.kind == Kind::Rssi {
        n["rssi_dbm"] = json!(l.last_value);
    }
    n
}

/// The `/ws/sensing` message.
pub fn sensing_update(r: &Report, presence_on: f32, simulated: bool) -> String {
    let confidence = (r.presence_score / (r.presence_score + presence_on)).clamp(0.0, 1.0);
    json!({
        "type": "sensing_update",
        "timestamp": r.ts_ms as f64 / 1000.0,
        "source": if simulated { "simulated" } else { "wifi" },
        "_simulated": simulated,
        "nodes": r.links.iter().enumerate().map(|(i, l)| node(i, l)).collect::<Vec<_>>(),
        "features": {
            "mean_rssi": r.mean_value,
            "std": r.motion_std,
            "variance": r.motion_std * r.motion_std,
            // ratio to the adaptive empty-room baseline (1.0 = nothing happening)
            "motion_band_power": r.presence_score,
            // fused spectral peak / mean band power (dimensionless)
            "breathing_band_power": r.breathing_conf,
            "dominant_freq_hz": r.dominant_freq_hz,
        },
        "classification": {
            "motion_level": motion_level(r, presence_on),
            "presence": r.presence,
            "confidence": confidence,
        },
        "vital_signs": {
            "breathing_detected": r.breathing_detected,
            "breathing_bpm": r.breathing_bpm,
            "breathing_confidence": r.breathing_conf,
            "breathing_threshold": r.breathing_threshold,
        },
        "mode": r.mode,
        "sample_rate_hz": r.sample_rate_hz,
    })
    .to_string()
}

/// The `/events` (SSE) message consumed by the 3D page.
pub fn event(r: &Report, presence_on: f32) -> String {
    json!({
        "presence": r.presence,
        // 0..1 for the shader/volume animation: how far above baseline, saturating at 4x the on-threshold
        "motion": (r.presence_score / (presence_on * 4.0)).clamp(0.0, 1.0),
        "breathing_bpm": r.breathing_bpm,
        "map": { "ar_live": false, "phone_estimate": null },
        "bed": { "heart": null },
        "mode": r.mode,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::DspConfig;
    use crate::engine::Engine;
    use crate::sample::Sample;

    fn report() -> Report {
        let mut e = Engine::new(DspConfig::default(), "n").unwrap();
        for i in 0..40 {
            e.push(Sample {
                t: f64::from(i) * 0.5,
                link: "aa:bb".into(),
                kind: Kind::Rssi,
                value: -50.0,
            });
        }
        e.step(20.0)
    }

    #[test]
    fn sensing_update_has_the_fields_the_ui_reads_and_no_invented_ones() {
        let v: Value = serde_json::from_str(&sensing_update(&report(), 2.0, false)).unwrap();
        assert_eq!(v["type"], "sensing_update");
        assert_eq!(v["source"], "wifi");
        assert_eq!(v["_simulated"], false);
        assert_eq!(v["nodes"][0]["rssi_dbm"], -50.0);
        assert!(
            v["nodes"][0].get("position").is_none(),
            "no fabricated geometry"
        );
        assert!(
            v.get("signal_field").is_none(),
            "no fabricated signal field"
        );
        assert_eq!(v["classification"]["motion_level"], "absent");
        assert!(v["features"]["mean_rssi"].is_number());
    }

    #[test]
    fn simulated_frames_are_labelled() {
        let v: Value = serde_json::from_str(&sensing_update(&report(), 2.0, true)).unwrap();
        assert_eq!(
            (v["source"].as_str(), v["_simulated"].as_bool()),
            (Some("simulated"), Some(true))
        );
    }

    #[test]
    fn event_motion_is_clamped_and_numeric() {
        let mut r = report();
        r.presence_score = 1e9;
        let v: Value = serde_json::from_str(&event(&r, 2.0)).unwrap();
        assert_eq!(v["motion"], 1.0);
        assert!(v["map"]["phone_estimate"].is_null());
    }
}
