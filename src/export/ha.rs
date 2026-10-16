//! Home Assistant MQTT discovery payloads.

use crate::config::MqttCfg;
use serde_json::{json, Value};

pub fn state_topic(c: &MqttCfg, node: &str) -> String {
    format!("{}/{node}/state", c.base_topic)
}
pub fn availability_topic(c: &MqttCfg, node: &str) -> String {
    format!("{}/{node}/status", c.base_topic)
}

/// (topic, retained JSON payload) for every entity this node exposes.
pub fn discovery(c: &MqttCfg, node: &str, device_name: &str) -> Vec<(String, String)> {
    let device = json!({
        "identifiers": [format!("rvcsi_{node}")],
        "name": device_name,
        "manufacturer": "Motorola",
        "model": "Edge (2025)",
        "sw_version": env!("CARGO_PKG_VERSION"),
    });
    let base = |object: &str, name: &str| -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        m.insert("name".into(), name.into());
        m.insert("unique_id".into(), format!("{node}_{object}").into());
        m.insert("object_id".into(), format!("{node}_{object}").into());
        m.insert("state_topic".into(), state_topic(c, node).into());
        m.insert(
            "availability_topic".into(),
            availability_topic(c, node).into(),
        );
        m.insert("payload_available".into(), "online".into());
        m.insert("payload_not_available".into(), "offline".into());
        m.insert("device".into(), device.clone());
        m
    };
    let mut out = Vec::new();
    let mut add = |component: &str, object: &str, m: serde_json::Map<String, Value>| {
        out.push((
            format!("{}/{component}/{node}/{object}/config", c.discovery_prefix),
            Value::Object(m).to_string(),
        ));
    };

    let mut m = base("presence", "Presence");
    m.insert("device_class".into(), "occupancy".into());
    m.insert(
        "value_template".into(),
        "{{ 'ON' if value_json.presence else 'OFF' }}".into(),
    );
    add("binary_sensor", "presence", m);

    let mut m = base("breathing_detected", "Breathing detected");
    m.insert(
        "value_template".into(),
        "{{ 'ON' if value_json.breathing_detected else 'OFF' }}".into(),
    );
    m.insert("icon".into(), "mdi:lungs".into());
    add("binary_sensor", "breathing_detected", m);

    let sensor =
        |object: &str, name: &str, field: &str, unit: Option<&str>, icon: &str, diag: bool| {
            let mut m = base(object, name);
            m.insert(
                "value_template".into(),
                format!("{{{{ value_json.{field} }}}}").into(),
            );
            if let Some(u) = unit {
                m.insert("unit_of_measurement".into(), u.into());
                m.insert("state_class".into(), "measurement".into());
            }
            m.insert("icon".into(), icon.into());
            // Entities go `unavailable` if the node stops reporting even with the broker up.
            m.insert("expire_after".into(), 60.into());
            if diag {
                m.insert("entity_category".into(), "diagnostic".into());
            }
            m
        };
    add(
        "sensor",
        "breathing_rate",
        sensor(
            "breathing_rate",
            "Breathing rate",
            "breathing_bpm",
            Some("bpm"),
            "mdi:lungs",
            false,
        ),
    );
    add(
        "sensor",
        "breathing_confidence",
        sensor(
            "breathing_confidence",
            "Breathing confidence",
            "breathing_conf",
            Some("x"),
            "mdi:signal",
            false,
        ),
    );
    add(
        "sensor",
        "presence_score",
        sensor(
            "presence_score",
            "Presence score",
            "presence_score",
            Some("x"),
            "mdi:account-search",
            false,
        ),
    );
    add(
        "sensor",
        "motion_std",
        sensor(
            "motion_std",
            "Link motion",
            "motion_std",
            Some("dB"),
            "mdi:motion-sensor",
            false,
        ),
    );
    add(
        "sensor",
        "sample_rate",
        sensor(
            "sample_rate",
            "Link sample rate",
            "sample_rate_hz",
            Some("Hz"),
            "mdi:sine-wave",
            true,
        ),
    );
    add(
        "sensor",
        "links_active",
        sensor(
            "links_active",
            "Active links",
            "links_active",
            None,
            "mdi:access-point",
            true,
        ),
    );
    add(
        "sensor",
        "mode",
        sensor(
            "mode",
            "Sensing mode",
            "mode",
            None,
            "mdi:information-outline",
            true,
        ),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entity_is_well_formed_and_uniquely_identified() {
        let c = MqttCfg::default();
        let d = discovery(&c, "edge2025", "Motorola Edge (2025)");
        assert_eq!(d.len(), 9);
        let mut ids = std::collections::HashSet::new();
        for (topic, payload) in &d {
            assert!(
                topic.starts_with("homeassistant/") && topic.ends_with("/config"),
                "{topic}"
            );
            assert!(!topic.contains('#') && !topic.contains('+'));
            let v: Value = serde_json::from_str(payload).unwrap();
            assert_eq!(v["state_topic"], "rvcsi/edge2025/state");
            assert_eq!(v["availability_topic"], "rvcsi/edge2025/status");
            assert_eq!(v["device"]["identifiers"][0], "rvcsi_edge2025");
            assert!(
                ids.insert(v["unique_id"].as_str().unwrap().to_string()),
                "duplicate unique_id"
            );
        }
    }
}
