//! Node configuration. Strict by design: unknown keys are errors, every value is
//! validated before any thread or socket is created.

use crate::dsp::DspConfig;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

fn d_iface() -> String {
    "wlan0".into()
}
fn d_hz10() -> f32 {
    10.0
}
fn d_hz5() -> f32 {
    5.0
}
fn d_hz2() -> f32 {
    2.0
}
fn d_scan() -> f32 {
    8.0
}
fn d_true() -> bool {
    true
}
fn d_bpm() -> f32 {
    15.0
}
fn d_amp() -> f32 {
    0.6
}
fn d_noise() -> f32 {
    0.8
}
fn d_links() -> usize {
    1
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceCfg {
    /// `/proc/net/wireless` link level of the connected AP. Fast (no process spawn) but
    /// SELinux on recent Android may hide it from Termux; `probe` tells you.
    ProcWireless {
        #[serde(default = "d_iface")]
        iface: String,
        #[serde(default = "d_hz10")]
        hz: f32,
    },
    /// `iw dev <iface> link` (needs the `iw` package; usually root via `su`).
    IwLink {
        #[serde(default = "d_iface")]
        iface: String,
        #[serde(default = "d_hz5")]
        hz: f32,
        #[serde(default)]
        su: bool,
    },
    /// `termux-wifi-connectioninfo` (Termux:API). ~2-4 Hz achievable.
    TermuxConn {
        #[serde(default = "d_hz2")]
        hz: f32,
    },
    /// `termux-wifi-scaninfo` (Termux:API): per-AP RSSI. Android throttles scans, so this
    /// is a presence-grade source only.
    TermuxScan {
        #[serde(default = "d_scan")]
        interval_s: f32,
    },
    /// Local UDP JSON bridge for anything the stock OS will not give Termux:
    /// Wi-Fi RTT from a companion app, CSI from a rooted/Nexmon path, ESP32 nodes.
    UdpIngest { bind: String },
    /// Deterministic generator for bring-up and tests. Never selected automatically.
    Synthetic {
        #[serde(default = "d_hz10")]
        hz: f32,
        #[serde(default = "d_links")]
        links: usize,
        #[serde(default = "d_bpm")]
        breathing_bpm: f32,
        #[serde(default = "d_amp")]
        amp: f32,
        #[serde(default = "d_noise")]
        noise: f32,
        #[serde(default = "d_true")]
        present: bool,
        #[serde(default)]
        seed: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct UdpCfg {
    pub enabled: bool,
    pub targets: Vec<String>,
}
impl Default for UdpCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            targets: vec!["127.0.0.1:5005".into()],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct MqttCfg {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub discovery_prefix: String,
    pub base_topic: String,
    pub keepalive_s: u16,
}
impl Default for MqttCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            host: "127.0.0.1".into(),
            port: 1883,
            username: None,
            password: None,
            discovery_prefix: "homeassistant".into(),
            base_topic: "rvcsi".into(),
            keepalive_s: 30,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct HttpCfg {
    pub enabled: bool,
    /// Loopback by default. A non-loopback bind requires `allow_lan = true`.
    pub bind: String,
    pub allow_lan: bool,
    /// Directory holding the web UI (`index.html`, `viz.html`, `model3d.html`, ...).
    pub ui_dir: String,
    pub max_connections: usize,
}
impl Default for HttpCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: "127.0.0.1:8765".into(),
            allow_lan: false,
            ui_dir: "ui".into(),
            max_connections: 24,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub node_id: String,
    pub device_name: String,
    /// Holds `token`, `baseline.json`, `map.json`. Created 0700.
    pub state_dir: String,
    /// Empty = auto-select from what `probe` finds on this device.
    pub sources: Vec<SourceCfg>,
    pub dsp: DspConfig,
    pub udp: UdpCfg,
    pub mqtt: MqttCfg,
    pub http: HttpCfg,
}

impl Default for Config {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        Self {
            node_id: "edge2025".into(),
            device_name: "Motorola Edge (2025)".into(),
            state_dir: format!("{home}/.rvcsi"),
            sources: vec![],
            dsp: DspConfig::default(),
            udp: UdpCfg::default(),
            mqtt: MqttCfg::default(),
            http: HttpCfg::default(),
        }
    }
}

/// Characters allowed in identifiers that end up in MQTT topics / HA unique_ids.
pub fn is_ident(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn rate_ok(hz: f32, lo: f32, hi: f32) -> bool {
    hz.is_finite() && hz >= lo && hz <= hi
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if text.len() > 1 << 20 {
            return Err("config file larger than 1 MiB".into());
        }
        let c: Config =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        c.validate()?;
        Ok(c)
    }

    pub fn state_path(&self, name: &str) -> PathBuf {
        Path::new(&self.state_dir).join(name)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !is_ident(&self.node_id, 32) {
            return Err("node_id must be 1-32 chars of [A-Za-z0-9_-]".into());
        }
        if self.device_name.is_empty()
            || self.device_name.len() > 64
            || self.device_name.chars().any(char::is_control)
        {
            return Err("device_name must be 1-64 printable chars".into());
        }
        self.dsp.validate()?;
        for s in &self.sources {
            match s {
                SourceCfg::ProcWireless { iface, hz } | SourceCfg::IwLink { iface, hz, .. } => {
                    if !is_ident(iface, 15) {
                        return Err(format!("invalid interface name {iface:?}"));
                    }
                    if !rate_ok(*hz, 0.1, 100.0) {
                        return Err("source hz must be within 0.1..=100".into());
                    }
                }
                SourceCfg::TermuxConn { hz } => {
                    if !rate_ok(*hz, 0.1, 20.0) {
                        return Err("termux_conn hz must be within 0.1..=20".into());
                    }
                }
                SourceCfg::TermuxScan { interval_s } => {
                    if !rate_ok(*interval_s, 1.0, 600.0) {
                        return Err("termux_scan interval_s must be within 1..=600".into());
                    }
                }
                SourceCfg::UdpIngest { bind } => {
                    bind.parse::<SocketAddr>()
                        .map_err(|e| format!("udp_ingest bind {bind:?}: {e}"))?;
                }
                SourceCfg::Synthetic {
                    hz,
                    links,
                    breathing_bpm,
                    amp,
                    noise,
                    ..
                } => {
                    if !rate_ok(*hz, 0.1, 200.0) || *links == 0 || *links > 64 {
                        return Err("synthetic: hz 0.1..=200, links 1..=64".into());
                    }
                    if !rate_ok(*breathing_bpm, 1.0, 60.0)
                        || !amp.is_finite()
                        || !noise.is_finite()
                        || *noise < 0.0
                    {
                        return Err("synthetic: invalid breathing_bpm/amp/noise".into());
                    }
                }
            }
        }
        if self.udp.enabled {
            for t in &self.udp.targets {
                t.parse::<SocketAddr>()
                    .map_err(|e| format!("udp target {t:?}: {e}"))?;
            }
        }
        if self.mqtt.enabled {
            let m = &self.mqtt;
            if m.host.is_empty()
                || m.host.len() > 253
                || m.host.chars().any(|c| c.is_control() || c.is_whitespace())
            {
                return Err("mqtt.host invalid".into());
            }
            if m.keepalive_s < 5 {
                return Err("mqtt.keepalive_s must be >= 5".into());
            }
            for (n, v) in [
                ("discovery_prefix", &m.discovery_prefix),
                ("base_topic", &m.base_topic),
            ] {
                let ok = !v.is_empty()
                    && v.len() <= 64
                    && !v.starts_with('/')
                    && !v.ends_with('/')
                    && v.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'/'));
                if !ok {
                    return Err(format!(
                        "mqtt.{n} must be [A-Za-z0-9_/-], no leading/trailing slash"
                    ));
                }
            }
            if m.username.as_ref().is_some_and(|u| u.len() > 255)
                || m.password.as_ref().is_some_and(|p| p.len() > 255)
            {
                return Err("mqtt credentials too long".into());
            }
        }
        if self.http.enabled {
            let a: SocketAddr = self
                .http
                .bind
                .parse()
                .map_err(|e| format!("http.bind: {e}"))?;
            if !a.ip().is_loopback() && !self.http.allow_lan {
                return Err("http.bind is not loopback: set http.allow_lan=true to accept this (token auth is still enforced, TLS is not provided)".into());
            }
            if self.http.max_connections == 0 || self.http.max_connections > 256 {
                return Err("http.max_connections must be 1..=256".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(serde_json::from_str::<Config>(r#"{"nodeid":"x"}"#).is_err());
        assert!(
            serde_json::from_str::<Config>(r#"{"sources":[{"type":"termux_conn","hzz":3}]}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<Config>(r#"{"sources":[{"type":"nope"}]}"#).is_err());
    }

    #[test]
    fn lan_bind_needs_explicit_opt_in() {
        let mut c = Config::default();
        c.http.bind = "0.0.0.0:8765".into();
        assert!(c.validate().is_err());
        c.http.allow_lan = true;
        c.validate().unwrap();
    }

    #[test]
    fn identifiers_cannot_inject_topics() {
        let mut c = Config::default();
        c.node_id = "a/b".into();
        assert!(c.validate().is_err());
        let mut c = Config::default();
        c.mqtt.base_topic = "rv#csi".into();
        assert!(c.validate().is_err());
        let mut c = Config::default();
        c.mqtt.discovery_prefix = "/homeassistant".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn source_parameters_are_bounded() {
        let mut c = Config::default();
        c.sources = vec![SourceCfg::TermuxConn { hz: 500.0 }];
        assert!(c.validate().is_err());
        c.sources = vec![SourceCfg::ProcWireless {
            iface: "wlan0; rm".into(),
            hz: 10.0,
        }];
        assert!(c.validate().is_err());
    }
}
