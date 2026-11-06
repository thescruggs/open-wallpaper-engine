// SPDX-License-Identifier: GPL-3.0-or-later
//! Wire types for `kwe-led-worker` (contract: `docs/LED_PROTOCOL_V1.md`).
//!
//! * stdin (daemon → worker): NDJSON lines of at most [`MAX_LINE_BYTES`].
//!   The renderer `audio_bands` line from `kwe-input-protocol` is accepted
//!   unchanged; the LED-only control lines (`led_config`, `led_frame`,
//!   `led_status_request`, `led_reprobe`, `led_stop`) are defined here.
//! * stdout (worker → daemon): `led_status` lines of at most
//!   [`MAX_STATUS_LINE_BYTES`].
//!
//! This crate does no I/O and contains no network code, so the daemon can
//! depend on it without linking the OpenRGB client. It deliberately does not
//! widen `kwe-input-protocol`: renderers never see these lines.
//!
//! The wire format and implementation are original to this project.

use std::path::{Component, Path, PathBuf};

use kwe_input_protocol::AudioFrame;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub const VERSION: u32 = 1;
/// stdin line cap, newline included; the same cap as renderer input.
pub const MAX_LINE_BYTES: usize = kwe_input_protocol::MAX_MESSAGE_BYTES;
/// stdout status line cap, newline included.
pub const MAX_STATUS_LINE_BYTES: usize = 32 * 1024;
pub const RATE_MIN_HZ: u8 = 1;
pub const RATE_MAX_HZ: u8 = 20;
pub const RATE_DEFAULT_HZ: u8 = 10;
pub const BRIGHTNESS_MAX: u8 = 100;
pub const BRIGHTNESS_DEFAULT: u8 = 100;
pub const MAX_DISABLED_DEVICES: usize = 128;
pub const MAX_PATH_BYTES: usize = 4096;
/// Devices listed in one status line (the rest are counted, not listed).
pub const MAX_STATUS_DEVICES: usize = 32;
/// Zones listed per device in one status line.
pub const MAX_STATUS_ZONES: usize = 16;
/// Display strings (names, vendor, ...) are cut to this many characters.
pub const MAX_STATUS_TEXT: usize = 80;
/// Length of a [`device_id`]: 16 lowercase hex digits.
pub const DEVICE_ID_LEN: usize = 16;

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Mode {
    #[serde(rename = "off")]
    Off,
    #[default]
    #[serde(rename = "ambient")]
    Ambient,
    #[serde(rename = "spectrum")]
    Spectrum,
    #[serde(rename = "ambient+pulse")]
    AmbientPulse,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Ambient => "ambient",
            Mode::Spectrum => "spectrum",
            Mode::AmbientPulse => "ambient+pulse",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "off" => Some(Mode::Off),
            "ambient" => Some(Mode::Ambient),
            "spectrum" => Some(Mode::Spectrum),
            "ambient+pulse" => Some(Mode::AmbientPulse),
            _ => None,
        }
    }
}

/// Settings identifier of a physical device (`disabled_devices`, status
/// `devices[].id`): FNV-1a 64 over `name NUL vendor NUL serial NUL
/// location`, as 16 lowercase hex digits — **except that `location` is
/// left out (hashed as empty) when the device reports a non-empty serial**.
/// Locations such as `/dev/hidraw3` change on replug/reboot, which would
/// silently re-enable a device the user disabled; a serial identifies the
/// unit by itself. Devices without a serial keep the location (it is the
/// only thing telling two identical units apart), so their ids still change
/// when the location does. Two units with the same name, vendor and serial
/// share one id (disabling one disables both).
///
/// Deterministic across runs, processes, and controller re-ordering.
pub fn device_id(name: &str, vendor: &str, serial: &str, location: &str) -> String {
    let location = if serial.trim().is_empty() {
        location
    } else {
        ""
    };
    device_key_hash(name, vendor, serial, location)
}

/// FNV-1a 64 over all four key strings (`name NUL vendor NUL serial NUL
/// location`), 16 lowercase hex digits. The worker uses it (plus an
/// occurrence index) to tell individual controllers apart internally; it is
/// not a settings id — see [`device_id`].
pub fn device_key_hash(name: &str, vendor: &str, serial: &str, location: &str) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for (i, part) in [name, vendor, serial, location].iter().enumerate() {
        if i > 0 {
            // NUL separator byte: XOR with 0 is the identity.
            hash = hash.wrapping_mul(PRIME);
        }
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
    }
    format!("{hash:016x}")
}

pub fn is_device_id(text: &str) -> bool {
    text.len() == DEVICE_ID_LEN
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// ---------------------------------------------------------------------------
// stdin control lines
// ---------------------------------------------------------------------------

/// `led_config`: every field optional; a present field replaces the current
/// value, an absent one keeps it. `disabled_devices` replaces the whole set.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brightness: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_hz: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_devices: Option<Vec<String>>,
}

impl ConfigPatch {
    pub fn validate(&self) -> Result<(), ControlError> {
        if let Some(b) = self.brightness
            && b > BRIGHTNESS_MAX
        {
            return Err(ControlError::Invalid("brightness must be 0..=100"));
        }
        if let Some(r) = self.rate_hz
            && !(RATE_MIN_HZ..=RATE_MAX_HZ).contains(&r)
        {
            return Err(ControlError::Invalid("rate_hz must be 1..=20"));
        }
        if let Some(list) = &self.disabled_devices {
            if list.len() > MAX_DISABLED_DEVICES {
                return Err(ControlError::Invalid("too many disabled_devices"));
            }
            if !list.iter().all(|id| is_device_id(id)) {
                return Err(ControlError::Invalid(
                    "disabled_devices entries must be 16 lowercase hex digits",
                ));
            }
        }
        Ok(())
    }

    /// Field-wise merge: fields present in `newer` win.
    pub fn merge(&mut self, newer: ConfigPatch) {
        if newer.mode.is_some() {
            self.mode = newer.mode;
        }
        if newer.brightness.is_some() {
            self.brightness = newer.brightness;
        }
        if newer.rate_hz.is_some() {
            self.rate_hz = newer.rate_hz;
        }
        if newer.disabled_devices.is_some() {
            self.disabled_devices = newer.disabled_devices;
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == ConfigPatch::default()
    }
}

/// One decoded stdin line.
#[derive(Debug, Clone, PartialEq)]
pub enum Control {
    Config(ConfigPatch),
    /// `None` = no live renderer / no frame source.
    Frame(Option<PathBuf>),
    Audio(AudioFrame),
    StatusRequest,
    Reprobe,
    Stop,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ControlError {
    #[error("line exceeds {MAX_LINE_BYTES} bytes")]
    TooLarge,
    #[error("line is empty or contains CR/embedded newline")]
    Framing,
    #[error("invalid JSON or schema: {0}")]
    Json(String),
    #[error("unsupported version {0}")]
    Version(u64),
    /// A well-formed line of a type this worker does not handle (other
    /// renderer messages, future additions). Not an error; counted.
    #[error("ignored message type")]
    Ignored,
    #[error("invalid value: {0}")]
    Invalid(&'static str),
}

impl ControlError {
    pub fn is_ignored(&self) -> bool {
        matches!(self, ControlError::Ignored)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigLine {
    version: u32,
    #[serde(rename = "type")]
    _type: String,
    #[serde(default)]
    mode: Option<Mode>,
    #[serde(default)]
    brightness: Option<u8>,
    #[serde(default)]
    rate_hz: Option<u8>,
    #[serde(default)]
    disabled_devices: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FrameLine {
    version: u32,
    #[serde(rename = "type")]
    _type: String,
    path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BareLine {
    version: u32,
    #[serde(rename = "type")]
    _type: String,
}

fn framing(bytes: &[u8]) -> Result<&[u8], ControlError> {
    if bytes.len() > MAX_LINE_BYTES {
        return Err(ControlError::TooLarge);
    }
    let payload = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if payload.is_empty() || payload.contains(&b'\n') || payload.contains(&b'\r') {
        return Err(ControlError::Framing);
    }
    Ok(payload)
}

fn json_err(e: serde_json::Error) -> ControlError {
    let mut text = e.to_string();
    text.truncate(160);
    ControlError::Json(text)
}

/// Validates a frame path: absolute, UTF-8, at most [`MAX_PATH_BYTES`], no
/// NUL, no `.`/`..` components. Whether it exists is checked per read.
pub fn validate_frame_path(text: &str) -> Result<PathBuf, ControlError> {
    if text.is_empty() || text.len() > MAX_PATH_BYTES || text.contains('\0') {
        return Err(ControlError::Invalid("frame path length or NUL"));
    }
    let path = Path::new(text);
    if !path.is_absolute() {
        return Err(ControlError::Invalid("frame path must be absolute"));
    }
    if text.split('/').any(|part| part == "." || part == "..") {
        return Err(ControlError::Invalid("frame path must not contain . or .."));
    }
    if path
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(ControlError::Invalid("frame path must not contain . or .."));
    }
    Ok(path.to_path_buf())
}

/// Decodes one stdin line (with or without its trailing newline).
pub fn decode_control(bytes: &[u8]) -> Result<Control, ControlError> {
    let payload = framing(bytes)?;
    let value: Value = serde_json::from_slice(payload).map_err(json_err)?;
    let Some(obj) = value.as_object() else {
        return Err(ControlError::Json("line is not a JSON object".into()));
    };
    let Some(kind) = obj.get("type").and_then(Value::as_str).map(str::to_owned) else {
        return Err(ControlError::Json("missing string field `type`".into()));
    };
    let version = obj.get("version").and_then(Value::as_u64);
    let check_version = |v: u32| {
        if v == VERSION {
            Ok(())
        } else {
            Err(ControlError::Version(u64::from(v)))
        }
    };
    match kind.as_str() {
        "audio_bands" => kwe_input_protocol::decode_audio_frame(payload)
            .map(Control::Audio)
            .map_err(|e| {
                let mut text = e.to_string();
                text.truncate(160);
                ControlError::Json(text)
            }),
        "led_config" => {
            let line: ConfigLine = serde_json::from_value(value).map_err(json_err)?;
            check_version(line.version)?;
            let patch = ConfigPatch {
                mode: line.mode,
                brightness: line.brightness,
                rate_hz: line.rate_hz,
                disabled_devices: line.disabled_devices,
            };
            patch.validate()?;
            Ok(Control::Config(patch))
        }
        "led_frame" => {
            let line: FrameLine = serde_json::from_value(value).map_err(json_err)?;
            check_version(line.version)?;
            match line.path {
                None => Ok(Control::Frame(None)),
                Some(p) => Ok(Control::Frame(Some(validate_frame_path(&p)?))),
            }
        }
        "led_status_request" | "led_reprobe" | "led_stop" => {
            let line: BareLine = serde_json::from_value(value).map_err(json_err)?;
            check_version(line.version)?;
            Ok(match kind.as_str() {
                "led_status_request" => Control::StatusRequest,
                "led_reprobe" => Control::Reprobe,
                _ => Control::Stop,
            })
        }
        _ => {
            // Unknown types are ignored only when they are otherwise
            // well-formed versioned lines; anything else is rejected.
            if version.is_some() {
                Err(ControlError::Ignored)
            } else {
                Err(ControlError::Json("missing numeric field `version`".into()))
            }
        }
    }
}

fn line(value: Value) -> Vec<u8> {
    let mut bytes = value.to_string().into_bytes();
    bytes.push(b'\n');
    bytes
}

/// Encodes a `led_config` line (validated; ≤ [`MAX_LINE_BYTES`]).
pub fn encode_config(patch: &ConfigPatch) -> Result<Vec<u8>, ControlError> {
    patch.validate()?;
    let mut value = serde_json::to_value(patch).map_err(json_err)?;
    value["version"] = VERSION.into();
    value["type"] = "led_config".into();
    let bytes = line(value);
    if bytes.len() > MAX_LINE_BYTES {
        return Err(ControlError::TooLarge);
    }
    Ok(bytes)
}

/// Encodes a `led_frame` line; `None` clears the frame source.
pub fn encode_frame(path: Option<&Path>) -> Result<Vec<u8>, ControlError> {
    let path = match path {
        None => Value::Null,
        Some(p) => {
            let text = p
                .to_str()
                .ok_or(ControlError::Invalid("frame path must be UTF-8"))?;
            validate_frame_path(text)?;
            Value::String(text.to_string())
        }
    };
    let bytes = line(serde_json::json!({"version": VERSION, "type": "led_frame", "path": path}));
    if bytes.len() > MAX_LINE_BYTES {
        return Err(ControlError::TooLarge);
    }
    Ok(bytes)
}

pub fn encode_status_request() -> Vec<u8> {
    line(serde_json::json!({"version": VERSION, "type": "led_status_request"}))
}

pub fn encode_reprobe() -> Vec<u8> {
    line(serde_json::json!({"version": VERSION, "type": "led_reprobe"}))
}

pub fn encode_stop() -> Vec<u8> {
    line(serde_json::json!({"version": VERSION, "type": "led_stop"}))
}

// ---------------------------------------------------------------------------
// stdout status lines
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Mode `off`: no device is controlled (all restored).
    Off,
    /// No SDK server answers on loopback. Normal and quiet.
    ServerAbsent,
    /// Before the first probe completed.
    Connecting,
    /// Connected and enumerated. Driving the enabled devices when `reason`
    /// is `null`; with reason `waiting_for_source` no device is taken over
    /// (the mode has nothing real to show yet: no frame for `ambient` /
    /// `ambient+pulse`, no fresh audio for `spectrum`).
    Active,
    /// Connection dropped or refused for a reason other than absence; see
    /// `reason`. The worker keeps re-probing.
    Degraded,
    /// Final line only: the worker restored devices and is exiting.
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameState {
    /// No frame path configured.
    None,
    /// Last read produced a stable snapshot.
    Ok,
    /// Path configured but the file does not exist.
    Absent,
    /// File exists but is not a valid/complete frame file (bad header,
    /// truncated, symlink, not a regular file).
    Invalid,
    /// The writer was mid-publish on every bounded attempt this tick.
    Busy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZoneKind {
    Single,
    Linear,
    Matrix,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZoneInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: ZoneKind,
    pub leds: u32,
    /// `[height, width]` for matrix zones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matrix: Option<[u32; 2]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// [`device_id`] of the full (untruncated) key; use this in
    /// `led_config.disabled_devices`.
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub serial: String,
    pub location: String,
    /// OpenRGB device type number.
    #[serde(rename = "type")]
    pub kind: i32,
    pub leds: u32,
    pub enabled: bool,
    /// True while this worker holds the device in its per-LED mode.
    pub controlled: bool,
    pub zones: Vec<ZoneInfo>,
    #[serde(default)]
    pub zones_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub port: u16,
    pub reachable: bool,
    /// Negotiated protocol version while connected.
    pub protocol: Option<u32>,
    /// The server's own highest version while connected.
    pub server_protocol: Option<u32>,
    /// Milliseconds until the next probe while not connected.
    pub next_probe_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameInfo {
    pub state: FrameState,
    /// Producer frame sequence of the last stable snapshot.
    pub sequence: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioInfo {
    /// An `audio_bands` line arrived within the freshness window (500 ms).
    pub fresh: bool,
    pub bands: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Counters {
    pub ticks: u64,
    pub updates_sent: u64,
    pub update_errors: u64,
    pub frames_read: u64,
    pub connects: u64,
    pub disconnects: u64,
    pub stdin_lines: u64,
    pub stdin_rejected: u64,
    pub stdin_ignored: u64,
    pub audio_frames: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreSummary {
    pub attempted: u32,
    pub restored: u32,
    pub failed: u32,
    /// The bounded restore deadline expired before it finished.
    pub timed_out: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusLine {
    pub version: u32,
    #[serde(rename = "type")]
    pub message_type: String,
    /// Increments by one per emitted line.
    pub seq: u64,
    pub state: State,
    /// Machine-readable cause for `degraded`, `waiting_for_source` on
    /// `active` while no device is driven, the last stop cause on the final
    /// `stopped` line; `null` otherwise.
    pub reason: Option<String>,
    pub mode: Mode,
    pub brightness: u8,
    pub rate_hz: u8,
    pub server: ServerInfo,
    pub frame: FrameInfo,
    pub audio: AudioInfo,
    pub device_count: u32,
    /// Devices this worker is sending colours to right now (in its per-LED
    /// mode and driven this state). 0 while waiting for a source, in mode
    /// `off`, and while not connected. Additive in v1: absent on lines of
    /// older workers (decodes as 0).
    #[serde(default)]
    pub driven_count: u32,
    pub devices: Vec<DeviceInfo>,
    pub devices_truncated: bool,
    pub counters: Counters,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<RestoreSummary>,
}

pub fn cut(text: &str) -> String {
    text.chars().take(MAX_STATUS_TEXT).collect()
}

/// Serializes a status line within [`MAX_STATUS_LINE_BYTES`]: if it does
/// not fit, zone lists are dropped first (`zones_truncated`), then devices
/// from the end (`devices_truncated`). Always returns a line that fits.
pub fn encode_status(status: &StatusLine) -> Vec<u8> {
    let mut status = status.clone();
    if status.devices.len() > MAX_STATUS_DEVICES {
        status.devices.truncate(MAX_STATUS_DEVICES);
        status.devices_truncated = true;
    }
    let fits = |s: &StatusLine| -> Option<Vec<u8>> {
        let mut bytes = serde_json::to_vec(s).ok()?;
        bytes.push(b'\n');
        (bytes.len() <= MAX_STATUS_LINE_BYTES).then_some(bytes)
    };
    if let Some(b) = fits(&status) {
        return b;
    }
    for d in &mut status.devices {
        if !d.zones.is_empty() {
            d.zones.clear();
            d.zones_truncated = true;
        }
    }
    loop {
        if let Some(b) = fits(&status) {
            return b;
        }
        if status.devices.pop().is_none() {
            break;
        }
        status.devices_truncated = true;
    }
    // Unreachable with the documented field caps; keep the contract anyway.
    let mut b = serde_json::to_vec(&serde_json::json!({
        "version": VERSION, "type": "led_status", "seq": status.seq,
        "state": status.state, "reason": "status_too_large",
    }))
    .unwrap_or_default();
    b.push(b'\n');
    b
}

/// Parses one status line (daemon side).
pub fn decode_status(bytes: &[u8]) -> Result<StatusLine, ControlError> {
    if bytes.len() > MAX_STATUS_LINE_BYTES {
        return Err(ControlError::TooLarge);
    }
    let payload = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let status: StatusLine = serde_json::from_slice(payload).map_err(json_err)?;
    if status.version != VERSION {
        return Err(ControlError::Version(u64::from(status.version)));
    }
    if status.message_type != "led_status" {
        return Err(ControlError::Json("not a led_status line".into()));
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_stable_and_separates_fields() {
        let a = device_id("Kbd", "Vendor", "SN", "loc");
        assert_eq!(a, device_id("Kbd", "Vendor", "SN", "loc"));
        assert!(is_device_id(&a));
        assert_ne!(a, device_id("KbdV", "endor", "SN", "loc"));
        // A serial identifies the unit: the location (replug/reboot) does
        // not change the id ...
        assert_eq!(a, device_id("Kbd", "Vendor", "SN", "loc2"));
        // ... without a serial the location is all that tells units apart.
        assert_ne!(
            device_id("Kbd", "Vendor", "", "loc"),
            device_id("Kbd", "Vendor", "", "loc2")
        );
        assert_ne!(
            device_key_hash("Kbd", "Vendor", "SN", "loc"),
            device_key_hash("Kbd", "Vendor", "SN", "loc2")
        );
        // Pinned values: changing the hash would silently re-enable every
        // device a user disabled. Serial-less ids are the original
        // derivation; serial ids hash the location as empty.
        assert_eq!(device_id("", "", "", ""), "d94d12186c0f2fb7");
        assert_eq!(device_key_hash("", "", "", ""), "d94d12186c0f2fb7");
        assert_eq!(
            device_id("Kbd", "Vendor", "SN", "anything"),
            device_key_hash("Kbd", "Vendor", "SN", "")
        );
    }

    #[test]
    fn config_lines_round_trip_and_validate() {
        let patch = ConfigPatch {
            mode: Some(Mode::AmbientPulse),
            brightness: Some(40),
            rate_hz: Some(20),
            disabled_devices: Some(vec!["0123456789abcdef".into()]),
        };
        let bytes = encode_config(&patch).unwrap();
        assert_eq!(decode_control(&bytes).unwrap(), Control::Config(patch));
        let partial = br#"{"version":1,"type":"led_config","mode":"off"}"#;
        assert_eq!(
            decode_control(partial).unwrap(),
            Control::Config(ConfigPatch {
                mode: Some(Mode::Off),
                ..ConfigPatch::default()
            })
        );
        for bad in [
            &br#"{"version":1,"type":"led_config","brightness":101}"#[..],
            br#"{"version":1,"type":"led_config","rate_hz":0}"#,
            br#"{"version":1,"type":"led_config","rate_hz":21}"#,
            br#"{"version":1,"type":"led_config","mode":"disco"}"#,
            br#"{"version":1,"type":"led_config","extra":1}"#,
            br#"{"version":1,"type":"led_config","disabled_devices":["ABC"]}"#,
            br#"{"version":2,"type":"led_config"}"#,
        ] {
            assert!(
                decode_control(bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn frame_lines_validate_paths() {
        let line = encode_frame(Some(Path::new("/run/user/1000/kwe/f.bin"))).unwrap();
        assert_eq!(
            decode_control(&line).unwrap(),
            Control::Frame(Some(PathBuf::from("/run/user/1000/kwe/f.bin")))
        );
        assert_eq!(
            decode_control(&encode_frame(None).unwrap()).unwrap(),
            Control::Frame(None)
        );
        for bad in ["relative/f.bin", "/a/../b", "/a/./b", ""] {
            let l = format!(r#"{{"version":1,"type":"led_frame","path":"{bad}"}}"#);
            assert!(decode_control(l.as_bytes()).is_err(), "{bad}");
        }
        let nul = br#"{"version":1,"type":"led_frame","path":"/a\u0000b"}"#;
        assert!(decode_control(nul).is_err());
    }

    #[test]
    fn audio_line_is_the_renderer_format_unchanged() {
        let frame = AudioFrame::new(3, vec![0.5; 16], vec![0.25; 16]).unwrap();
        let bytes = kwe_input_protocol::encode_audio_frame(&frame).unwrap();
        assert_eq!(decode_control(&bytes).unwrap(), Control::Audio(frame));
    }

    #[test]
    fn framing_and_unknown_types() {
        assert_eq!(
            decode_control(&vec![b'x'; MAX_LINE_BYTES + 1]),
            Err(ControlError::TooLarge)
        );
        assert_eq!(decode_control(b"\n"), Err(ControlError::Framing));
        assert_eq!(decode_control(b"{}\r\n"), Err(ControlError::Framing));
        assert!(matches!(
            decode_control(b"garbage"),
            Err(ControlError::Json(_))
        ));
        assert!(matches!(
            decode_control(b"[1,2]"),
            Err(ControlError::Json(_))
        ));
        let other = br#"{"version":1,"type":"media_state","sequence":1,"playback":"paused"}"#;
        assert_eq!(decode_control(other), Err(ControlError::Ignored));
        assert!(matches!(
            decode_control(br#"{"type":"whatever"}"#),
            Err(ControlError::Json(_))
        ));
        assert_eq!(decode_control(&encode_stop()).unwrap(), Control::Stop);
        assert_eq!(decode_control(&encode_reprobe()).unwrap(), Control::Reprobe);
        assert_eq!(
            decode_control(&encode_status_request()).unwrap(),
            Control::StatusRequest
        );
    }

    fn device(i: usize, zones: usize) -> DeviceInfo {
        DeviceInfo {
            id: device_id(&format!("d{i}"), "", "", ""),
            name: "n".repeat(MAX_STATUS_TEXT),
            vendor: "v".repeat(MAX_STATUS_TEXT),
            serial: "s".repeat(MAX_STATUS_TEXT),
            location: "l".repeat(MAX_STATUS_TEXT),
            kind: 0,
            leds: 1,
            enabled: true,
            controlled: false,
            zones: (0..zones)
                .map(|_| ZoneInfo {
                    name: "z".repeat(MAX_STATUS_TEXT),
                    kind: ZoneKind::Matrix,
                    leds: 100,
                    matrix: Some([6, 22]),
                })
                .collect(),
            zones_truncated: false,
        }
    }

    pub(crate) fn sample_status(devices: Vec<DeviceInfo>) -> StatusLine {
        StatusLine {
            version: VERSION,
            message_type: "led_status".into(),
            seq: 1,
            state: State::Active,
            reason: None,
            mode: Mode::Ambient,
            brightness: 100,
            rate_hz: 10,
            server: ServerInfo {
                port: 6742,
                reachable: true,
                protocol: Some(5),
                server_protocol: Some(5),
                next_probe_ms: None,
            },
            frame: FrameInfo {
                state: FrameState::Ok,
                sequence: Some(1),
                width: Some(4),
                height: Some(4),
            },
            audio: AudioInfo {
                fresh: false,
                bands: None,
            },
            device_count: devices.len() as u32,
            driven_count: 0,
            devices,
            devices_truncated: false,
            counters: Counters::default(),
            restore: None,
        }
    }

    #[test]
    fn status_from_an_older_worker_without_driven_count_decodes() {
        let mut value = serde_json::to_value(sample_status(vec![])).unwrap();
        value.as_object_mut().unwrap().remove("driven_count");
        let back = decode_status(value.to_string().as_bytes()).unwrap();
        assert_eq!(back.driven_count, 0);
    }

    #[test]
    fn status_round_trips_and_is_bounded() {
        let small = sample_status(vec![device(0, 2)]);
        let bytes = encode_status(&small);
        assert_eq!(decode_status(&bytes).unwrap(), small);

        let huge = sample_status((0..200).map(|i| device(i, MAX_STATUS_ZONES)).collect());
        let bytes = encode_status(&huge);
        assert!(bytes.len() <= MAX_STATUS_LINE_BYTES);
        let back = decode_status(&bytes).unwrap();
        assert!(back.devices_truncated);
        assert!(back.devices.len() <= MAX_STATUS_DEVICES);
        assert_eq!(back.device_count, 200);
    }
}
