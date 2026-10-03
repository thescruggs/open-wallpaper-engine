// SPDX-License-Identifier: GPL-3.0-or-later
//! The Steam credential file. Same posture as `kwe-daemon`'s persist/settings
//! modules (reimplemented here on purpose): private 0700 directory, 0600
//! file, O_NOFOLLOW, size cap, atomic write, quarantine of anything unusable.

use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::util::{log, unix_nanos, unix_seconds};

pub const FILE_NAME: &str = "steam-auth-v1.json";
const MAX_FILE_BYTES: u64 = 16 * 1024;

/// A token or cookie. `Debug` never shows the value and there is no
/// `Display`, so a stray `{:?}`/`{}` cannot leak it.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub schema_version: u32,
    pub account_name: String,
    pub steam_id: String,
    pub refresh_token: Secret,
    #[serde(default)]
    pub access_token: Secret,
    #[serde(default)]
    pub community_access_token: Secret,
}

impl Credential {
    fn sane(&self) -> bool {
        self.schema_version == 1
            && !self.steam_id.is_empty()
            && self.steam_id.len() <= 20
            && self.steam_id.bytes().all(|b| b.is_ascii_digit())
            && !self.refresh_token.is_empty()
            && self.refresh_token.expose().len() <= 8 * 1024
            && self.access_token.expose().len() <= 8 * 1024
            && self.community_access_token.expose().len() <= 8 * 1024
            && self.account_name.chars().count() <= 128
    }
}

pub struct CredentialStore {
    dir: PathBuf,
}

impl CredentialStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(FILE_NAME)
    }

    fn quarantine(&self) {
        let path = self.path();
        let target = self
            .dir
            .join(format!("{FILE_NAME}.invalid-{}", unix_seconds()));
        match fs::rename(&path, &target) {
            Ok(()) => log("store.quarantined", ""),
            Err(_) => log("store.quarantine_error", ""),
        }
    }

    /// The stored credential, or None (signed out). Unreadable, oversize,
    /// symlinked or wrong-schema files are renamed aside.
    pub fn load(&self) -> Option<Credential> {
        let path = self.path();
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(_) => return None,
        };
        if !meta.file_type().is_file() {
            // A symlink or odd node: never read through it.
            self.quarantine();
            return None;
        }
        let credential = self.read_file(&path);
        match credential {
            Some(c) => Some(c),
            None => {
                self.quarantine();
                None
            }
        }
    }

    fn read_file(&self, path: &Path) -> Option<Credential> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
            .ok()?;
        if file.metadata().ok()?.len() > MAX_FILE_BYTES {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return None;
        }
        let credential: Credential = serde_json::from_slice(&bytes).ok()?;
        credential.sane().then_some(credential)
    }

    pub fn save(&self, credential: &Credential) -> std::io::Result<()> {
        ensure_private_dir(&self.dir)?;
        let bytes = serde_json::to_vec(credential).map_err(std::io::Error::other)?;
        let temporary = self.dir.join(format!(
            ".{FILE_NAME}.tmp-{}-{}",
            std::process::id(),
            unix_nanos()
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&temporary)?;
        let written = file.write_all(&bytes).and_then(|()| file.sync_all());
        if let Err(error) = written.and_then(|()| fs::rename(&temporary, self.path())) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        File::open(&self.dir)?.sync_all()
    }

    pub fn remove(&self) {
        match fs::remove_file(self.path()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => log("store.remove_error", ""),
        }
    }
}

/// Creates (or verifies) a real directory owned by us and sets it to 0700.
pub fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(std::io::Error::other(
            "private path must be a real directory",
        ));
    }
    // SAFETY: geteuid has no preconditions.
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::other("private path is not owned by us"));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kwe-ww-{label}-{}-{}",
            std::process::id(),
            unix_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub fn sample(refresh: &str) -> Credential {
        Credential {
            schema_version: 1,
            account_name: "fake_account".into(),
            steam_id: "76561190000000001".into(),
            refresh_token: Secret::new(refresh),
            access_token: Secret::default(),
            community_access_token: Secret::default(),
        }
    }

    fn invalid_siblings(dir: &Path) -> usize {
        fs::read_dir(dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("steam-auth-v1.json.invalid-")
            })
            .count()
    }

    #[test]
    fn save_load_round_trip_with_private_modes() {
        let dir = temp_dir("rt").join("state");
        let store = CredentialStore::new(&dir);
        assert!(store.load().is_none());
        store.save(&sample("r.e.f")).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(store.path()).unwrap().mode() & 0o777, 0o600);
        assert_eq!(store.load().unwrap().steam_id, "76561190000000001");
        store.remove();
        assert!(!store.path().exists());
    }

    #[test]
    fn garbage_wrong_schema_and_oversize_are_quarantined() {
        for (label, bytes) in [
            ("garbage", b"not json".to_vec()),
            (
                "schema",
                br#"{"schema_version":2,"account_name":"a","steam_id":"1","refresh_token":"x"}"#
                    .to_vec(),
            ),
            ("oversize", vec![b' '; 20 * 1024]),
        ] {
            let dir = temp_dir(label);
            let store = CredentialStore::new(&dir);
            fs::write(store.path(), bytes).unwrap();
            assert!(store.load().is_none(), "{label}");
            assert!(!store.path().exists(), "{label}");
            assert_eq!(invalid_siblings(&dir), 1, "{label}");
        }
    }

    #[test]
    fn symlink_is_refused_and_not_followed() {
        let dir = temp_dir("symlink");
        let store = CredentialStore::new(&dir);
        let target = dir.join("elsewhere.json");
        fs::write(&target, serde_json::to_vec(&sample("r.e.f")).unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, store.path()).unwrap();
        assert!(store.load().is_none());
        assert!(target.exists(), "the link target is left alone");
        assert_eq!(invalid_siblings(&dir), 1);
    }

    #[test]
    fn debug_output_hides_tokens() {
        let mut credential = sample("SENTINEL-REFRESH");
        credential.access_token = Secret::new("SENTINEL-ACCESS");
        let text = format!("{credential:?}");
        assert!(!text.contains("SENTINEL"), "{text}");
        assert!(text.contains("<redacted>"));
    }
}
