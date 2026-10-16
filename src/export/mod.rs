//! Local-only exporters. No exporter ever talks to anything that is not an address the
//! operator configured.

pub mod ha;
pub mod mqtt;
pub mod udp;

use crate::engine::Report;

/// Compact JSON for datagrams/MQTT state: the per-link array is dropped when it would
/// push the payload past a single safe UDP datagram.
pub fn report_json(r: &Report, max_len: usize) -> String {
    let full = serde_json::to_string(r).unwrap_or_else(|_| "{}".into());
    if full.len() <= max_len {
        return full;
    }
    let mut slim = serde_json::to_value(r).unwrap_or_default();
    if let Some(o) = slim.as_object_mut() {
        o.remove("links");
    }
    slim.to_string()
}
