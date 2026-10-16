//! rvcsi: self-contained Wi-Fi sensing node for Android/Termux.
//!
//! sources (threads) -> Engine (sliding-window DSP) -> { UDP, MQTT + HA discovery, local web UI }
//! Every endpoint is a local socket; there is no cloud dependency.
#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::panic,
        clippy::field_reassign_with_default
    )
)]

pub mod config;
pub mod dsp;
pub mod engine;
pub mod export;
pub mod ingest;
pub mod node;
pub mod sample;
pub mod web;

pub use config::Config;
pub use engine::{Engine, Report};
pub use sample::{Kind, Sample};
