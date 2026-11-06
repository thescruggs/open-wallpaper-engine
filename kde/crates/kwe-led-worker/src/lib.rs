// SPDX-License-Identifier: GPL-3.0-or-later
//! `kwe-led-worker`: isolated OpenRGB LED sync worker (plan:
//! `docs/backlog/OPENRGB_LED.md`, contract: `docs/LED_PROTOCOL_V1.md`).
//!
//! Sources (the wallpaper's frame file, audio bands teed by the daemon) →
//! pure mapping/smoothing → the loopback-only `kwe-openrgb` client. The
//! library exists so the math and the plumbing are unit-testable; the
//! binary in `main.rs` is the only consumer.
//!
//! Original implementation; no OpenRGB or third-party client code copied.

#![forbid(unsafe_code)]

pub mod color;
pub mod engine;
pub mod frame;
pub mod log;
pub mod mapping;
pub mod runtime;
