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
//! while every output is covered by a maximized or fullscreen window; the
//! supervisor runs the occlusion detector only while this is on.
//!
//! Defaults: audio enabled (the behavior before this store existed), pause
//! off. Persistence is atomic; a corrupt file is quarantined with the
//! rename-to-invalid pattern and the store starts fresh, mirroring
//! `grants::GrantStore`.

use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::persist::{atomic_write, ensure_private_dir, quarantine_invalid_state};

const SETTINGS_FILE: &str = "settings-v1.json";
/// The record is one small object; anything bigger is not ours.
const MAX_SETTINGS_BYTES: u64 = 16 * 1024;

fn default_audio_output() -> bool {
    true
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
}

impl Default for PersistedSettings {
    fn default() -> Self {
        Self {
            schema_version: 1,
            audio_output: true,
            pause_when_covered: false,
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
            Ok(state) if state.schema_version == 1 => state,
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
}
