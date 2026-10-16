//! Stock-Android link observation backends. Everything stock Android lets a
//! non-root Termux process see about the Wi-Fi link, and nothing it does not.
//!
//! Honest capability map (Motorola Edge 2025, unrooted):
//!   * RSSI of the connected AP: yes (`/proc/net/wireless` if SELinux allows,
//!     else Termux:API connection info).
//!   * Per-AP RSSI of neighbours: yes, but scan-throttled -> presence-grade.
//!   * Wi-Fi RTT (802.11mc): the platform API exists, Termux:API does not expose it.
//!     Feed it through `udp_ingest` from a companion app.
//!   * CSI / beamforming feedback: not exposed by the stock driver. Needs a rooted
//!     or custom-firmware path feeding `udp_ingest`.

use super::exec;
use super::Source;
use crate::sample::{now, Kind, Sample};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

const CMD_TIMEOUT: Duration = Duration::from_millis(2500);
/// `WifiInfo.INVALID_RSSI` and anything below a physically meaningful floor.
const RSSI_MIN: f32 = -126.0;
const RSSI_MAX: f32 = -1.0;

fn rssi_ok(v: f32) -> bool {
    v.is_finite() && (RSSI_MIN..=RSSI_MAX).contains(&v)
}

fn period(hz: f32) -> Duration {
    Duration::from_secs_f32(1.0 / hz.clamp(0.1, 100.0))
}

// ---------------------------------------------------------------- /proc/net/wireless

/// Parse the `level` column (dBm) for `iface` out of `/proc/net/wireless`.
pub fn parse_proc_wireless(text: &str, iface: &str) -> Option<f32> {
    for line in text.lines().skip(2) {
        let (name, rest) = line.split_once(':')?;
        if name.trim() != iface {
            continue;
        }
        // status, link quality, signal level, noise level, ...
        let level = rest.split_whitespace().nth(2)?;
        let mut v: f32 = level.trim_end_matches('.').parse().ok()?;
        if v > 0.0 {
            v -= 256.0; // 8-bit unsigned encoding used by older drivers
        }
        return rssi_ok(v).then_some(v);
    }
    None
}

#[derive(Debug)]
pub struct ProcWireless {
    pub iface: String,
    pub hz: f32,
}
impl Source for ProcWireless {
    fn name(&self) -> String {
        format!("proc_wireless:{}", self.iface)
    }
    fn period(&self) -> Duration {
        period(self.hz)
    }
    fn poll(&mut self, out: &mut Vec<Sample>) -> Result<(), String> {
        let text = std::fs::read_to_string("/proc/net/wireless")
            .map_err(|e| format!("/proc/net/wireless: {e}"))?;
        let v = parse_proc_wireless(&text, &self.iface)
            .ok_or_else(|| format!("no valid level for {}", self.iface))?;
        out.push(Sample {
            t: now(),
            link: self.iface.clone(),
            kind: Kind::Rssi,
            value: v,
        });
        Ok(())
    }
}

// ---------------------------------------------------------------- iw

pub fn parse_iw_signal(text: &str) -> Option<f32> {
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("signal:") {
            let v: f32 = rest.split_whitespace().next()?.parse().ok()?;
            return rssi_ok(v).then_some(v);
        }
    }
    None
}

#[derive(Debug)]
pub struct IwLink {
    pub iface: String,
    pub hz: f32,
    pub su: bool,
}
impl Source for IwLink {
    fn name(&self) -> String {
        format!("iw_link:{}{}", self.iface, if self.su { ":su" } else { "" })
    }
    fn period(&self) -> Duration {
        period(self.hz)
    }
    fn poll(&mut self, out: &mut Vec<Sample>) -> Result<(), String> {
        let argv: Vec<String> = if self.su {
            vec![
                "su".into(),
                "-c".into(),
                format!("iw dev {} link", self.iface),
            ]
        } else {
            vec!["iw".into(), "dev".into(), self.iface.clone(), "link".into()]
        };
        let text = exec::run(&argv, CMD_TIMEOUT)?;
        let v = parse_iw_signal(&text).ok_or("iw: no signal line (not associated?)")?;
        out.push(Sample {
            t: now(),
            link: self.iface.clone(),
            kind: Kind::Rssi,
            value: v,
        });
        Ok(())
    }
}

// ---------------------------------------------------------------- Termux:API

fn json_err(v: &Value) -> Option<String> {
    v.get("API_ERROR")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn sane_bssid(b: &str) -> bool {
    b.len() == 17 && b.bytes().all(|c| c.is_ascii_hexdigit() || c == b':')
}

/// `termux-wifi-connectioninfo` -> (link id, rssi).
pub fn parse_conn(text: &str) -> Result<(String, f32), String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("connectioninfo json: {e}"))?;
    if let Some(e) = json_err(&v) {
        return Err(format!("termux-api: {e}"));
    }
    let rssi = v
        .get("rssi")
        .and_then(Value::as_f64)
        .ok_or("connectioninfo: no rssi")? as f32;
    if !rssi_ok(rssi) {
        return Err(format!("connectioninfo: rssi {rssi} invalid (Wi-Fi off?)"));
    }
    // Without location permission Android reports the placeholder 02:00:00:00:00:00.
    let link = match v.get("bssid").and_then(Value::as_str) {
        Some(b) if sane_bssid(b) && b != "02:00:00:00:00:00" => b.to_ascii_lowercase(),
        _ => "connected".to_string(),
    };
    Ok((link, rssi))
}

#[derive(Debug)]
pub struct TermuxConn {
    pub hz: f32,
}
impl Source for TermuxConn {
    fn name(&self) -> String {
        "termux_conn".into()
    }
    fn period(&self) -> Duration {
        period(self.hz)
    }
    fn poll(&mut self, out: &mut Vec<Sample>) -> Result<(), String> {
        let text = exec::run(&["termux-wifi-connectioninfo".to_string()], CMD_TIMEOUT)?;
        let (link, v) = parse_conn(&text)?;
        out.push(Sample {
            t: now(),
            link,
            kind: Kind::Rssi,
            value: v,
        });
        Ok(())
    }
}

/// `termux-wifi-scaninfo` -> fresh (bssid, rssi) observations. `seen` maps bssid to the
/// last scan timestamp already emitted: Android hands back its cached scan list on
/// every call, so without this a stale entry would be re-counted as a new measurement.
pub fn parse_scan(
    text: &str,
    seen: &mut HashMap<String, u64>,
) -> Result<Vec<(String, f32)>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("scaninfo json: {e}"))?;
    if let Some(e) = json_err(&v) {
        return Err(format!("termux-api: {e}"));
    }
    let arr = v.as_array().ok_or("scaninfo: expected array")?;
    let mut out = Vec::new();
    for ap in arr.iter().take(256) {
        let Some(b) = ap
            .get("bssid")
            .and_then(Value::as_str)
            .filter(|b| sane_bssid(b))
        else {
            continue;
        };
        let Some(r) = ap
            .get("rssi")
            .and_then(Value::as_f64)
            .map(|r| r as f32)
            .filter(|r| rssi_ok(*r))
        else {
            continue;
        };
        let b = b.to_ascii_lowercase();
        if let Some(ts) = ap.get("timestamp").and_then(Value::as_u64) {
            if seen.get(&b) == Some(&ts) {
                continue;
            }
            seen.insert(b.clone(), ts);
        }
        out.push((b, r));
    }
    if seen.len() > 1024 {
        seen.clear();
    }
    Ok(out)
}

#[derive(Debug)]
pub struct TermuxScan {
    pub interval_s: f32,
    seen: HashMap<String, u64>,
}
impl TermuxScan {
    pub fn new(interval_s: f32) -> Self {
        Self {
            interval_s,
            seen: HashMap::new(),
        }
    }
}
impl Source for TermuxScan {
    fn name(&self) -> String {
        "termux_scan".into()
    }
    fn period(&self) -> Duration {
        Duration::from_secs_f32(self.interval_s.clamp(1.0, 600.0))
    }
    fn poll(&mut self, out: &mut Vec<Sample>) -> Result<(), String> {
        let text = exec::run(
            &["termux-wifi-scaninfo".to_string()],
            Duration::from_secs(10),
        )?;
        let t = now();
        for (link, value) in parse_scan(&text, &mut self.seen)? {
            out.push(Sample {
                t,
                link,
                kind: Kind::Rssi,
                value,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROC: &str = "Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE\n face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22\n wlan0: 0000   58.  -52.  -256        0      0      0      0      0        0\n";

    #[test]
    fn proc_wireless_parses_dbm_and_rejects_garbage() {
        assert_eq!(parse_proc_wireless(PROC, "wlan0"), Some(-52.0));
        assert_eq!(parse_proc_wireless(PROC, "wlan1"), None);
        assert_eq!(parse_proc_wireless("", "wlan0"), None);
        let bad = PROC.replace("-52.", "-300.");
        assert_eq!(parse_proc_wireless(&bad, "wlan0"), None);
        let unsigned = PROC.replace("-52.", "200.");
        assert_eq!(parse_proc_wireless(&unsigned, "wlan0"), Some(-56.0));
        let zero = PROC.replace("-52.", "0.");
        assert_eq!(parse_proc_wireless(&zero, "wlan0"), None);
    }

    #[test]
    fn iw_signal_parsing() {
        let link = "Connected to aa:bb:cc:dd:ee:ff (on wlan0)\n\tSSID: x\n\tfreq: 5180\n\tsignal: -47 dBm\n\ttx bitrate: 866.7 MBit/s\n";
        assert_eq!(parse_iw_signal(link), Some(-47.0));
        assert_eq!(parse_iw_signal("Not connected.\n"), None);
        assert_eq!(parse_iw_signal("signal avg: -40 dBm\n"), None);
        assert_eq!(
            parse_iw_signal("\tsignal:  \t-40 [-42, -41] dBm\n"),
            Some(-40.0)
        );
    }

    #[test]
    fn conn_info_parsing() {
        let ok = r#"{"bssid":"AA:BB:CC:DD:EE:FF","rssi":-55,"ssid":"x","link_speed_mbps":866}"#;
        assert_eq!(
            parse_conn(ok).unwrap(),
            ("aa:bb:cc:dd:ee:ff".to_string(), -55.0)
        );
        let placeholder = r#"{"bssid":"02:00:00:00:00:00","rssi":-60}"#;
        assert_eq!(parse_conn(placeholder).unwrap().0, "connected");
        assert!(parse_conn(r#"{"bssid":"aa:bb:cc:dd:ee:ff","rssi":-127}"#).is_err());
        assert!(parse_conn(r#"{"API_ERROR":"permission denied"}"#)
            .unwrap_err()
            .contains("permission"));
        assert!(parse_conn("not json").is_err());
        assert!(parse_conn("{}").is_err());
    }

    #[test]
    fn scan_dedupes_cached_entries_and_drops_junk() {
        let scan = r#"[{"bssid":"aa:bb:cc:dd:ee:01","rssi":-60,"timestamp":100},
                       {"bssid":"aa:bb:cc:dd:ee:02","rssi":-70,"timestamp":100},
                       {"bssid":"bogus","rssi":-50,"timestamp":1},
                       {"bssid":"aa:bb:cc:dd:ee:03","rssi":5,"timestamp":1}]"#;
        let mut seen = HashMap::new();
        let first = parse_scan(scan, &mut seen).unwrap();
        assert_eq!(first.len(), 2);
        assert!(
            parse_scan(scan, &mut seen).unwrap().is_empty(),
            "cached results must not be re-emitted"
        );
        let fresh = scan.replace(
            "\"timestamp\":100},\n                       {\"bssid\":\"aa:bb:cc:dd:ee:02\"",
            "\"timestamp\":200},\n                       {\"bssid\":\"aa:bb:cc:dd:ee:02\"",
        );
        assert_eq!(parse_scan(&fresh, &mut seen).unwrap().len(), 1);
        assert!(parse_scan(r#"{"API_ERROR":"location off"}"#, &mut seen).is_err());
        assert!(parse_scan("{}", &mut seen).is_err());
    }
}
