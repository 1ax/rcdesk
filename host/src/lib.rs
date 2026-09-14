//! rcdesk host agent library: capture, encode, pipeline wiring, WebRTC
//! transport and the signaling client. `main.rs` is a thin CLI shell over
//! this crate so the video/transport path is unit- and integration-testable
//! without the binary (see `tests/loopback.rs`).

pub mod adapt;
pub mod capture;
pub mod cursor;
pub mod encode;
pub mod input;
pub mod pipeline;
pub mod platform;
pub mod signaling;
pub mod transport;
