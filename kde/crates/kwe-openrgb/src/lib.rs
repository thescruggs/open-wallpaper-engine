// SPDX-License-Identifier: GPL-3.0-or-later
//! Minimal, bounded, synchronous client for the OpenRGB SDK network protocol.
//!
//! Independent implementation written from the published protocol
//! description (OpenRGB `Documentation/OpenRGBSDK.md`, see `THIRD_PARTY.yml`
//! entry `openrgb-sdk-protocol`). No OpenRGB or third-party client code was
//! copied or adapted. Contract: `docs/OPENRGB_CLIENT.md`.
//!
//! Scope is only what LED sync needs: protocol negotiation, client name,
//! controller enumeration, `SetCustomMode`, `UpdateLEDs`, `UpdateZoneLEDs`,
//! `UpdateMode` (used to restore the previous mode), and the server's
//! device-list-updated notification.
//!
//! Safety posture:
//! - The client can only dial `127.0.0.1` — [`ClientConfig`] has a port and no
//!   address, because the SDK protocol has no authentication.
//! - Every read/write has a deadline; packet size and every count/string
//!   length are capped ([`limits`]); a hostile server yields a typed
//!   [`Error`], never a panic, hang, or unbounded allocation.
//! - "Server not running" is the distinct, cheap [`Error::ServerAbsent`].

#![forbid(unsafe_code)]

mod client;
mod error;
mod model;
pub mod wire;

#[cfg(any(test, feature = "stub"))]
pub mod stub;

#[cfg(test)]
mod tests;

pub use client::{Client, ClientConfig, DEFAULT_PORT, PollOutcome, RestoreReport};
pub use error::Error;
pub use model::{
    Controller, ControllerId, DeviceKey, Led, MatrixMap, Mode, Rgb, Segment, Zone, zone_type,
};

pub type Result<T> = std::result::Result<T, Error>;

/// Lowest server protocol version this client accepts (OpenRGB 0.5).
pub const PROTOCOL_MIN: u32 = 1;
/// Highest protocol version this client speaks (OpenRGB 1.0rc1 and later
/// releases). Newer servers negotiate down to this version.
pub const PROTOCOL_MAX: u32 = 5;

/// Hard caps applied to everything read from the server.
pub mod limits {
    /// Largest accepted packet payload.
    pub const MAX_PACKET_BYTES: u32 = 4 * 1024 * 1024;
    /// Largest controller count accepted from `REQUEST_CONTROLLER_COUNT`.
    pub const MAX_CONTROLLERS: u32 = 128;
    pub const MAX_ZONES: u16 = 64;
    /// Per controller; also bounds colours and LED display names.
    pub const MAX_LEDS: u16 = 8192;
    pub const MAX_MODES: u16 = 128;
    pub const MAX_MODE_COLORS: u16 = 256;
    pub const MAX_SEGMENTS: u16 = 64;
    pub const MAX_MATRIX_CELLS: u32 = 16 * 1024;
    /// Any length-prefixed string, NUL included.
    pub const MAX_STRING_BYTES: u16 = 4096;
    /// Unrelated server packets skipped while waiting for one reply, and
    /// packets drained per `poll_events` call.
    pub const MAX_UNSOLICITED: usize = 64;
    pub const MAX_CLIENT_NAME_BYTES: usize = 64;
    /// Upper bound for every configurable timeout.
    pub const MAX_TIMEOUT_MS: u64 = 30_000;
}
