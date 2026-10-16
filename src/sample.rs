use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static T0: OnceLock<Instant> = OnceLock::new();

/// Monotonic seconds since first call. All pipeline timestamps use this clock
/// (immune to NTP steps / suspend-resume wall-clock jumps).
pub fn now() -> f64 {
    T0.get_or_init(Instant::now).elapsed().as_secs_f64()
}

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What a scalar series physically is. Baselines and noise floors are per kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// dBm from Android / driver (integer-quantised: noise floor ~0.29 dB std).
    Rssi,
    /// Wi-Fi RTT (802.11mc FTM) distance, millimetres.
    RttMm,
    /// Scalar derived from CSI (e.g. mean subcarrier amplitude) from an external bridge.
    Csi,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Rssi => "rssi",
            Kind::RttMm => "rtt_mm",
            Kind::Csi => "csi",
        }
    }
    /// Smallest std the series can show from quantisation alone.
    pub fn std_floor(self) -> f32 {
        match self {
            Kind::Rssi => 0.29, // sqrt(1/12): integer-dB rounding
            Kind::RttMm => 5.0,
            Kind::Csi => 1e-3,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Sample {
    /// Seconds on the `now()` clock, stamped at receipt.
    pub t: f64,
    /// Stable link identity (BSSID, interface, or external node name).
    pub link: String,
    pub kind: Kind,
    pub value: f32,
}
