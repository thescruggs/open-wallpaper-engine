// SPDX-License-Identifier: GPL-3.0-or-later
//! Daemon-owned global wallpaper settings.
//!
//! One record for the whole daemon, in `settings-v1.json` beside
//! `permissions-v1.json` in the private state directory. Two knobs:
//! `audio_output` — whether wallpapers may PLAY sound (distinct from the
//! per-wallpaper `audio` grant, which gates delivery of CAPTURED system
//! audio to audio-reactive wallpapers); the supervisor re-reads the store at
//! every spawn and appends `--mute` to video and scene workers while audio
//! output is disabled (web workers stay always-muted by their own policy).
//! `pause_when_covered` (F3) — whether the live renderer is told to pause
//! while every output shows a fullscreen application; the supervisor runs
//! the occlusion detector only while this is on.
//! `pause_when_display_off` (F4) — whether the live renderer is told to
//! pause while every display is powered down (DPMS / screen energy saving),
//! holding the last frame until a display wakes; the supervisor runs the
//! display power helper only while this is on.
//!
//! `led` (OpenRGB L3) — the LED sync record `{enabled, mode, brightness,
//! rate_hz, disabled_devices}`; the supervisor runs `kwe-led-worker` while
//! `enabled` is on and forwards every other field to it live
//! (`docs/LED_PROTOCOL_V1.md`, `docs/SUPERVISOR_API_V1.md` §LED sync).
//!
//! Defaults: audio enabled (the behavior before this store existed), both
//! pause policies off, LED sync on in `ambient` at 100 % / 10 Hz with every
//! device enabled (maintainer decision 2026-11-05: OpenRGB is optional but
//! used when its SDK server runs). A file written before a field existed
//! loads with that field's default; an out-of-bounds LED value fails
//! validation and quarantines the file like any other schema violation. Persistence is atomic; a corrupt file is quarantined with the
//! rename-to-invalid pattern and the store starts fresh, mirroring
//! `grants::GrantStore`.

use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};
use kwe_led_protocol::{
    BRIGHTNESS_DEFAULT, BRIGHTNESS_MAX, MAX_DISABLED_DEVICES, Mode, RATE_DEFAULT_HZ, RATE_MAX_HZ,
    RATE_MIN_HZ, is_device_id,
};
use serde::{Deserialize, Serialize};

use crate::persist::{atomic_write, ensure_private_dir, quarantine_invalid_state};

const SETTINGS_FILE: &str = "settings-v1.json";
/// The record is one small object; anything bigger is not ours.
const MAX_SETTINGS_BYTES: u64 = 16 * 1024;

fn default_audio_output() -> bool {
    true
}

fn default_true() -> bool {
    true
}

fn default_led_brightness() -> u8 {
    BRIGHTNESS_DEFAULT
}

fn default_led_rate_hz() -> u8 {
    RATE_DEFAULT_HZ
}

/// OpenRGB LED sync settings (slice L3). Every field defaults individually,
/// so a partial record (or none at all) loads with the documented defaults.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LedSettings {
    /// Use OpenRGB while its SDK server answers on loopback (default on).
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub mode: Mode,
    /// Brightness cap in percent, 0..=100.
    #[serde(default = "default_led_brightness")]
    pub brightness: u8,
    /// LED update rate, 1..=20 Hz.
    #[serde(default = "default_led_rate_hz")]
    pub rate_hz: u8,
    /// Device ids (`kwe_led_protocol::device_id`, 16 lowercase hex digits)
    /// the worker must leave alone; sorted, unique, at most 128.
    #[serde(default)]
    pub disabled_devices: Vec<String>,
}

impl Default for LedSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: Mode::Ambient,
            brightness: BRIGHTNESS_DEFAULT,
            rate_hz: RATE_DEFAULT_HZ,
            disabled_devices: Vec::new(),
        }
    }
}

impl LedSettings {
    /// Daemon-side bounds (the worker re-checks every line it receives).
    pub fn validate(&self) -> Result<()> {
        validate_led_values(
            Some(self.brightness),
            Some(self.rate_hz),
            Some(&self.disabled_devices),
        )?;
        let mut sorted = self.disabled_devices.clone();
        sorted.sort();
        sorted.dedup();
        if sorted != self.disabled_devices {
            bail!("led.disabled_devices must be sorted and unique");
        }
        Ok(())
    }

    /// The record with `patch` applied; ids are normalized (sorted, unique).
    pub fn patched(&self, patch: &LedPatch) -> Result<Self> {
        patch.validate()?;
        let mut next = self.clone();
        if let Some(enabled) = patch.enabled {
            next.enabled = enabled;
        }
        if let Some(mode) = patch.mode {
            next.mode = mode;
        }
        if let Some(brightness) = patch.brightness {
            next.brightness = brightness;
        }
        if let Some(rate_hz) = patch.rate_hz {
            next.rate_hz = rate_hz;
        }
        if let Some(devices) = &patch.disabled_devices {
            let mut devices = devices.clone();
            devices.sort();
            devices.dedup();
            next.disabled_devices = devices;
        }
        next.validate()?;
        Ok(next)
    }
}

/// `settings.set {"led": {...}}`: every field optional; present fields
/// replace their value, `disabled_devices` replaces the whole list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedPatch {
    pub enabled: Option<bool>,
    pub mode: Option<Mode>,
    pub brightness: Option<u8>,
    pub rate_hz: Option<u8>,
    pub disabled_devices: Option<Vec<String>>,
}

impl LedPatch {
    /// Bounds that do not depend on the current record, so the RPC layer
    /// can reject a bad patch before any other setting in the same request
    /// is applied.
    pub fn validate(&self) -> Result<()> {
        validate_led_values(
            self.brightness,
            self.rate_hz,
            self.disabled_devices.as_deref(),
        )
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

fn validate_led_values(
    brightness: Option<u8>,
    rate_hz: Option<u8>,
    devices: Option<&[String]>,
) -> Result<()> {
    if brightness.is_some_and(|value| value > BRIGHTNESS_MAX) {
        bail!("led.brightness must be 0..={BRIGHTNESS_MAX}");
    }
    if rate_hz.is_some_and(|value| !(RATE_MIN_HZ..=RATE_MAX_HZ).contains(&value)) {
        bail!("led.rate_hz must be {RATE_MIN_HZ}..={RATE_MAX_HZ}");
    }
    if let Some(devices) = devices {
        if devices.len() > MAX_DISABLED_DEVICES {
            bail!("led.disabled_devices holds at most {MAX_DISABLED_DEVICES} ids");
        }
        if !devices.iter().all(|id| is_device_id(id)) {
            bail!("led.disabled_devices entries must be 16 lowercase hex digits");
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedSettings {
    schema_version: u32,
    #[serde(default = "default_audio_output")]
    audio_output: bool,
    /// Additive (F3): files written before the field existed load as `false`.
    #[serde(default)]
    pause_when_covered: bool,
    /// Additive (F4): files written before the field existed load as `false`.
    #[serde(default)]
    pause_when_display_off: bool,
    /// Additive (OpenRGB L3): files written before the record existed load
    /// with `LedSettings::default()` (sync on).
    #[serde(default)]
    led: LedSettings,
}

impl Default for PersistedSettings {
    fn default() -> Self {
        Self {
            schema_version: 1,
            audio_output: true,
            pause_when_covered: false,
            pause_when_display_off: false,
            led: LedSettings::default(),
        }
    }
}

/// The settings file on disk plus its in-memory state. Mutations persist
/// atomically and only commit once the write succeeded.
pub struct SettingsStore {
    path: PathBuf,
    state: PersistedSettings,
}

impl SettingsStore {
    /// Opens (or creates) the settings file in `directory`. A corrupt file is
    /// quarantined to `<file>.invalid-<unix_seconds>` and the store starts
    /// fresh with the defaults.
    pub fn open(directory: &Path) -> Result<Self> {
        ensure_private_dir(directory)?;
        let path = directory.join(SETTINGS_FILE);
        let state = Self::load(&path);
        Ok(Self { path, state })
    }

    pub fn audio_output(&self) -> bool {
        self.state.audio_output
    }

    /// Persists the new value atomically; the in-memory state only changes
    /// once the write succeeded. Returns whether the value changed.
    pub fn set_audio_output(&mut self, enabled: bool) -> Result<bool> {
        if self.state.audio_output == enabled {
            return Ok(false);
        }
        let mut next = self.state.clone();
        next.audio_output = enabled;
        self.save(&next)?;
        self.state = next;
        Ok(true)
    }

    pub fn pause_when_covered(&self) -> bool {
        self.state.pause_when_covered
    }

    /// F3: persists the pause-when-covered policy atomically. Returns whether
    /// the value changed.
    pub fn set_pause_when_covered(&mut self, enabled: bool) -> Result<bool> {
        if self.state.pause_when_covered == enabled {
            return Ok(false);
        }
        let mut next = self.state.clone();
        next.pause_when_covered = enabled;
        self.save(&next)?;
        self.state = next;
        Ok(true)
    }

    pub fn pause_when_display_off(&self) -> bool {
        self.state.pause_when_display_off
    }

    /// F4: persists the pause-when-display-off policy atomically. Returns
    /// whether the value changed.
    pub fn set_pause_when_display_off(&mut self, enabled: bool) -> Result<bool> {
        if self.state.pause_when_display_off == enabled {
            return Ok(false);
        }
        let mut next = self.state.clone();
        next.pause_when_display_off = enabled;
        self.save(&next)?;
        self.state = next;
        Ok(true)
    }

    pub fn led(&self) -> &LedSettings {
        &self.state.led
    }

    /// OpenRGB L3: persists a validated LED record atomically. Returns
    /// whether it changed.
    pub fn set_led(&mut self, led: LedSettings) -> Result<bool> {
        led.validate()?;
        if self.state.led == led {
            return Ok(false);
        }
        let mut next = self.state.clone();
        next.led = led;
        self.save(&next)?;
        self.state = next;
        Ok(true)
    }

    fn save(&self, state: &PersistedSettings) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(state)?;
        if bytes.len() as u64 > MAX_SETTINGS_BYTES {
            bail!("settings state exceeds {MAX_SETTINGS_BYTES} bytes");
        }
        atomic_write(&self.path, &bytes)
    }

    fn load(path: &Path) -> PersistedSettings {
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return PersistedSettings::default();
            }
            Err(error) => {
                Self::quarantine(path, &format!("open failed: {error}"));
                return PersistedSettings::default();
            }
        };
        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                Self::quarantine(path, &format!("metadata failed: {error}"));
                return PersistedSettings::default();
            }
        };
        if !metadata.is_file() || metadata.len() > MAX_SETTINGS_BYTES {
            Self::quarantine(path, "not a bounded regular file");
            return PersistedSettings::default();
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        if let Err(error) = file.read_to_end(&mut bytes) {
            Self::quarantine(path, &format!("read failed: {error}"));
            return PersistedSettings::default();
        }
        match serde_json::from_slice::<PersistedSettings>(&bytes) {
            Ok(state) if state.schema_version == 1 => match state.led.validate() {
                Ok(()) => state,
                Err(error) => {
                    Self::quarantine(path, &format!("invalid led record: {error}"));
                    PersistedSettings::default()
                }
            },
            Ok(_) => {
                Self::quarantine(path, "unsupported schema");
                PersistedSettings::default()
            }
            Err(error) => {
                Self::quarantine(path, &format!("parse failed: {error}"));
                PersistedSettings::default()
            }
        }
    }

    fn quarantine(path: &Path, reason: &str) {
        eprintln!("event=settings.store_invalid detail={reason}");
        quarantine_invalid_state(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_directory(label: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "kwe-settings-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn defaults_to_audio_output_enabled_and_round_trips() {
        let directory = temporary_directory("roundtrip");
        let mut store = SettingsStore::open(&directory).unwrap();
        assert!(store.audio_output(), "audio output must default to enabled");

        assert!(store.set_audio_output(false).unwrap());
        assert!(!store.audio_output());
        // A no-op set reports no change and does not rewrite the file.
        assert!(!store.set_audio_output(false).unwrap());

        let reloaded = SettingsStore::open(&directory).unwrap();
        assert!(!reloaded.audio_output(), "the value must persist");
    }

    #[test]
    fn pause_when_covered_defaults_off_round_trips_and_loads_legacy_files() {
        let directory = temporary_directory("pause");
        let mut store = SettingsStore::open(&directory).unwrap();
        assert!(!store.pause_when_covered(), "pause must default to off");
        assert!(store.set_pause_when_covered(true).unwrap());
        assert!(!store.set_pause_when_covered(true).unwrap());
        let reloaded = SettingsStore::open(&directory).unwrap();
        assert!(reloaded.pause_when_covered());
        assert!(reloaded.audio_output(), "the other knob is untouched");

        // A record written before F3 existed loads with the field defaulted.
        std::fs::write(
            directory.join(SETTINGS_FILE),
            br#"{"schema_version":1,"audio_output":false}"#,
        )
        .unwrap();
        let legacy = SettingsStore::open(&directory).unwrap();
        assert!(!legacy.audio_output());
        assert!(!legacy.pause_when_covered());
    }

    #[test]
    fn pause_when_display_off_defaults_off_round_trips_and_loads_legacy_files() {
        let directory = temporary_directory("display-off");
        let mut store = SettingsStore::open(&directory).unwrap();
        assert!(!store.pause_when_display_off(), "must default to off");
        assert!(store.set_pause_when_display_off(true).unwrap());
        assert!(!store.set_pause_when_display_off(true).unwrap());
        let reloaded = SettingsStore::open(&directory).unwrap();
        assert!(reloaded.pause_when_display_off());
        assert!(
            !reloaded.pause_when_covered(),
            "the other knob is untouched"
        );

        // A record written before F4 existed loads with the field defaulted.
        std::fs::write(
            directory.join(SETTINGS_FILE),
            br#"{"schema_version":1,"audio_output":false,"pause_when_covered":true}"#,
        )
        .unwrap();
        let legacy = SettingsStore::open(&directory).unwrap();
        assert!(legacy.pause_when_covered());
        assert!(!legacy.pause_when_display_off());
    }

    #[test]
    fn corrupt_or_oversize_store_is_quarantined_and_defaults_apply() {
        let directory = temporary_directory("corrupt");
        let path = directory.join(SETTINGS_FILE);

        std::fs::write(&path, b"not-json").unwrap();
        let store = SettingsStore::open(&directory).unwrap();
        assert!(
            store.audio_output(),
            "corruption must fall back to defaults"
        );
        assert!(!path.exists(), "the corrupt file must be quarantined");

        std::fs::write(&path, vec![b' '; (MAX_SETTINGS_BYTES + 1) as usize]).unwrap();
        let store = SettingsStore::open(&directory).unwrap();
        assert!(store.audio_output());
        assert!(!path.exists());

        // Unknown fields are a schema violation (deny_unknown_fields).
        std::fs::write(
            &path,
            br#"{"schema_version":1,"audio_output":false,"bogus":1}"#,
        )
        .unwrap();
        let store = SettingsStore::open(&directory).unwrap();
        assert!(store.audio_output());
    }

    #[test]
    fn led_defaults_on_and_loads_from_a_file_written_before_l3() {
        let directory = temporary_directory("led-legacy");
        std::fs::write(
            directory.join(SETTINGS_FILE),
            br#"{"schema_version":1,"audio_output":false,"pause_when_covered":true,"pause_when_display_off":true}"#,
        )
        .unwrap();
        let store = SettingsStore::open(&directory).unwrap();
        assert!(!store.audio_output(), "the old fields survive");
        assert!(store.pause_when_display_off());
        let led = store.led();
        assert!(led.enabled, "LED sync defaults on (maintainer 2026-11-05)");
        assert_eq!(led.mode, Mode::Ambient);
        assert_eq!(led.brightness, 100);
        assert_eq!(led.rate_hz, 10);
        assert!(led.disabled_devices.is_empty());
        assert!(directory.join(SETTINGS_FILE).exists(), "not quarantined");

        // A partial record fills the missing fields with their defaults.
        std::fs::write(
            directory.join(SETTINGS_FILE),
            br#"{"schema_version":1,"led":{"mode":"spectrum"}}"#,
        )
        .unwrap();
        let store = SettingsStore::open(&directory).unwrap();
        assert!(store.led().enabled);
        assert_eq!(store.led().mode, Mode::Spectrum);
        assert_eq!(store.led().rate_hz, 10);
    }

    #[test]
    fn led_round_trips_and_patches_normalize_device_ids() {
        let directory = temporary_directory("led-roundtrip");
        let mut store = SettingsStore::open(&directory).unwrap();
        let next = store
            .led()
            .patched(&LedPatch {
                enabled: Some(false),
                mode: Some(Mode::AmbientPulse),
                brightness: Some(0),
                rate_hz: Some(20),
                disabled_devices: Some(vec![
                    "ffffffffffffffff".into(),
                    "0123456789abcdef".into(),
                    "ffffffffffffffff".into(),
                ]),
            })
            .unwrap();
        assert_eq!(
            next.disabled_devices,
            vec![
                "0123456789abcdef".to_string(),
                "ffffffffffffffff".to_string()
            ]
        );
        assert!(store.set_led(next.clone()).unwrap());
        assert!(!store.set_led(next.clone()).unwrap(), "no-op set");
        let reloaded = SettingsStore::open(&directory).unwrap();
        assert_eq!(reloaded.led(), &next);
        assert!(reloaded.audio_output(), "other knobs untouched");
    }

    #[test]
    fn led_bounds_are_enforced_on_patch_and_quarantined_on_load() {
        let base = LedSettings::default();
        for patch in [
            LedPatch {
                brightness: Some(101),
                ..LedPatch::default()
            },
            LedPatch {
                rate_hz: Some(0),
                ..LedPatch::default()
            },
            LedPatch {
                rate_hz: Some(21),
                ..LedPatch::default()
            },
            LedPatch {
                disabled_devices: Some(vec!["0123456789ABCDEF".into()]),
                ..LedPatch::default()
            },
            LedPatch {
                disabled_devices: Some(vec!["0123".into()]),
                ..LedPatch::default()
            },
            LedPatch {
                disabled_devices: Some(
                    (0..=MAX_DISABLED_DEVICES)
                        .map(|i| format!("{i:016x}"))
                        .collect(),
                ),
                ..LedPatch::default()
            },
        ] {
            assert!(patch.validate().is_err(), "{patch:?}");
            assert!(base.patched(&patch).is_err(), "{patch:?}");
        }

        let directory = temporary_directory("led-bounds");
        let path = directory.join(SETTINGS_FILE);
        for bad in [
            &br#"{"schema_version":1,"led":{"rate_hz":50}}"#[..],
            br#"{"schema_version":1,"led":{"brightness":300}}"#,
            br#"{"schema_version":1,"led":{"mode":"disco"}}"#,
            br#"{"schema_version":1,"led":{"enabled":true,"extra":1}}"#,
            br#"{"schema_version":1,"led":{"disabled_devices":["nothex"]}}"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            let store = SettingsStore::open(&directory).unwrap();
            assert_eq!(store.led(), &LedSettings::default());
            assert!(
                !path.exists(),
                "invalid led record must quarantine the file"
            );
        }
    }
}
