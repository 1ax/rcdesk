//! rcdesk host agent library: capture, encode, pipeline wiring, WebRTC
//! transport and the signaling client. `main.rs` is a thin CLI shell over
//! this crate so the video/transport path is unit- and integration-testable
//! without the binary (see `tests/loopback.rs`).

/// Default `tracing` filter for both binaries when `RUST_LOG` is unset
/// (slice 3.5d, debt D12). The WebRTC stack (`rtc*` crates) logs routine
/// events at WARN/ERROR on every session -- Chrome's ClientHello extensions
/// it doesn't know, STUN it discards, the mDNS lookup of Chrome's hidden
/// host candidate that always times out (the direct pair is found through a
/// peer-reflexive candidate instead) -- so only its errors are kept, minus
/// that mDNS one. Session state changes are logged by our own
/// `rcdesk_host::signaling`. `enigo=warn`: `agent::permissions` builds an
/// `Enigo` every 10s and enigo logs an `info!` line each time.
pub const DEFAULT_LOG_FILTER: &str =
    "info,rtc=error,rtc_dtls=error,rtc_ice=error,rtc_ice::agent::agent_proto=off,enigo=warn";

pub mod access;
pub mod adapt;
pub mod agent;
pub mod app;
pub mod capture;
pub mod clipboard;
pub mod cursor;
pub mod device;
pub mod encode;
mod fsutil;
pub mod input;
pub mod pipeline;
pub mod platform;
pub mod signaling;
pub mod transport;
