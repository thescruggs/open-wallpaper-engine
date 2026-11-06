// SPDX-License-Identifier: GPL-3.0-or-later
//! Remembered pre-control modes persisted across worker processes
//! (`--saved-modes <ABS_PATH>`; contract: `docs/LED_PROTOCOL_V1.md`
//! "Remembered modes file").
//!
//! The file is written *before* the worker switches a device to its
//! per-LED mode, so after an unclean death (SIGKILL, OOM, panic) the next
//! worker adopts the user's original modes instead of saving the per-LED
//! mode it finds as the "previous" one. It is removed once every remembered
//! mode was restored.
//!
//! Writes are atomic (temp file + `rename`), mode 0600, bounded in size.
//! A missing file is the normal "nothing to restore" state; a corrupt,
//! oversized, foreign-version, or non-regular file is ignored with one log
//! line and removed (it is replaced by the next write) — never a crash,
//! never an exit.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use kwe_openrgb::{ControlMode, DeviceKey, Mode, Rgb, SavedMode, limits};
use serde_json::{Map, Value, json};

/// File format version (`"version"`); any other value is ignored.
pub const FILE_VERSION: u64 = 1;
const FILE_TYPE: &str = "kwe_led_saved_modes";
/// Largest file read or written. Entries are bounded by the client's caps
/// (≤ 128 entries, strings ≤ 4096 bytes, ≤ 256 mode colours, ≤ 8192 Direct
/// colours each); if the encoding would still be larger, Direct colours are
/// dropped first (the mode itself is still restored), then entries.
pub const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;

/// Outcome of [`load`]: the entries, and a note to log when the file was
/// ignored.
#[derive(Debug, Default)]
pub struct Loaded {
    pub entries: Vec<SavedMode>,
    pub ignored: Option<String>,
}

fn hex_colors(colors: &[Rgb]) -> String {
    let mut out = String::with_capacity(colors.len() * 6);
    for c in colors {
        out.push_str(&format!("{:02x}{:02x}{:02x}", c.r, c.g, c.b));
    }
    out
}

fn parse_hex_colors(text: &str) -> Option<Vec<Rgb>> {
    if !text.len().is_multiple_of(6) || text.len() / 6 > usize::from(limits::MAX_LEDS) {
        return None;
    }
    let bytes = text.as_bytes();
    bytes
        .chunks_exact(6)
        .map(|c| Rgb::parse_hex(std::str::from_utf8(c).ok()?))
        .collect()
}

fn mode_json(m: &Mode) -> Value {
    json!({
        "name": m.name, "value": m.value, "flags": m.flags,
        "speed_min": m.speed_min, "speed_max": m.speed_max,
        "brightness_min": m.brightness_min, "brightness_max": m.brightness_max,
        "colors_min": m.colors_min, "colors_max": m.colors_max,
        "speed": m.speed, "brightness": m.brightness, "direction": m.direction,
        "color_mode": m.color_mode, "colors": hex_colors(&m.colors),
    })
}

fn entry_json(s: &SavedMode, with_colors: bool) -> Value {
    json!({
        "name": s.key.name, "vendor": s.key.vendor, "serial": s.key.serial,
        "location": s.key.location, "occurrence": s.occurrence, "index": s.index,
        "mode": mode_json(&s.mode),
        "colors": if with_colors { hex_colors(&s.colors) } else { String::new() },
        "control": s.control.as_ref().map(|c| json!({"index": c.index, "name": c.name})),
    })
}

/// Encodes `entries` within [`MAX_FILE_BYTES`]; an empty set encodes as an
/// empty vector (= no file).
pub fn encode(entries: &[SavedMode]) -> Vec<u8> {
    if entries.is_empty() {
        return Vec::new();
    }
    let entries = &entries[..entries.len().min(limits::MAX_CONTROLLERS as usize)];
    let mut with_colors = vec![true; entries.len()];
    let mut keep = entries.len();
    loop {
        let list: Vec<Value> = entries[..keep]
            .iter()
            .zip(&with_colors)
            .map(|(s, c)| entry_json(s, *c))
            .collect();
        let mut bytes = json!({"version": FILE_VERSION, "type": FILE_TYPE, "entries": list})
            .to_string()
            .into_bytes();
        bytes.push(b'\n');
        if bytes.len() <= MAX_FILE_BYTES || keep == 0 {
            return bytes;
        }
        // Too large: drop the biggest colour list still present, then
        // whole entries from the end.
        match (0..keep)
            .filter(|i| with_colors[*i] && !entries[*i].colors.is_empty())
            .max_by_key(|i| entries[*i].colors.len())
        {
            Some(i) => with_colors[i] = false,
            None => keep -= 1,
        }
    }
}

fn get_str(o: &Map<String, Value>, k: &str) -> Option<String> {
    let s = o.get(k)?.as_str()?;
    (s.len() < usize::from(limits::MAX_STRING_BYTES)).then(|| s.to_string())
}

fn get_u32(o: &Map<String, Value>, k: &str) -> Option<u32> {
    u32::try_from(o.get(k)?.as_u64()?).ok()
}

fn parse_mode(v: &Value) -> Option<Mode> {
    let o = v.as_object()?;
    let colors = parse_hex_colors(o.get("colors")?.as_str()?)?;
    if colors.len() > usize::from(limits::MAX_MODE_COLORS) {
        return None;
    }
    Some(Mode {
        name: get_str(o, "name")?,
        value: i32::try_from(o.get("value")?.as_i64()?).ok()?,
        flags: get_u32(o, "flags")?,
        speed_min: get_u32(o, "speed_min")?,
        speed_max: get_u32(o, "speed_max")?,
        brightness_min: get_u32(o, "brightness_min")?,
        brightness_max: get_u32(o, "brightness_max")?,
        colors_min: get_u32(o, "colors_min")?,
        colors_max: get_u32(o, "colors_max")?,
        speed: get_u32(o, "speed")?,
        brightness: get_u32(o, "brightness")?,
        direction: get_u32(o, "direction")?,
        color_mode: get_u32(o, "color_mode")?,
        colors,
    })
}

fn parse_entry(v: &Value) -> Option<SavedMode> {
    let o = v.as_object()?;
    let index = usize::try_from(o.get("index")?.as_u64()?).ok()?;
    if index >= usize::from(limits::MAX_MODES) {
        return None;
    }
    let control = match o.get("control")? {
        Value::Null => None,
        c => {
            let c = c.as_object()?;
            let index = usize::try_from(c.get("index")?.as_u64()?).ok()?;
            Some(ControlMode {
                index,
                name: get_str(c, "name")?,
            })
        }
    };
    Some(SavedMode {
        key: DeviceKey {
            name: get_str(o, "name")?,
            vendor: get_str(o, "vendor")?,
            serial: get_str(o, "serial")?,
            location: get_str(o, "location")?,
        },
        occurrence: get_u32(o, "occurrence")?.min(limits::MAX_CONTROLLERS),
        index,
        mode: parse_mode(o.get("mode")?)?,
        colors: parse_hex_colors(o.get("colors")?.as_str()?)?,
        control,
    })
}

/// Parses file contents. `Err` names why the file is ignored.
pub fn decode(bytes: &[u8]) -> Result<Vec<SavedMode>, &'static str> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err("oversized");
    }
    let v: Value = serde_json::from_slice(bytes).map_err(|_| "not_json")?;
    let o = v.as_object().ok_or("not_an_object")?;
    if o.get("type").and_then(Value::as_str) != Some(FILE_TYPE) {
        return Err("wrong_type");
    }
    if o.get("version").and_then(Value::as_u64) != Some(FILE_VERSION) {
        return Err("unsupported_version");
    }
    let list = o
        .get("entries")
        .and_then(Value::as_array)
        .ok_or("no_entries")?;
    if list.len() > limits::MAX_CONTROLLERS as usize {
        return Err("too_many_entries");
    }
    let mut out: Vec<SavedMode> = Vec::with_capacity(list.len());
    for e in list {
        let e = parse_entry(e).ok_or("bad_entry")?;
        if !out
            .iter()
            .any(|o| o.key == e.key && o.occurrence == e.occurrence)
        {
            out.push(e);
        }
    }
    Ok(out)
}

fn open_nofollow(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
}

/// Reads the file. Missing → empty, quietly. Anything unusable → empty plus
/// a note, and the file is removed so the next write replaces it.
pub fn load(path: &Path) -> Loaded {
    let read = || -> Result<Option<Vec<u8>>, &'static str> {
        let file = match open_nofollow(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("unreadable_or_symlink"),
        };
        let meta = file.metadata().map_err(|_| "unreadable")?;
        if !meta.is_file() {
            return Err("not_a_regular_file");
        }
        if meta.len() > MAX_FILE_BYTES as u64 {
            return Err("oversized");
        }
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "unreadable")?;
        Ok(Some(bytes))
    };
    let why = match read() {
        Ok(None) => return Loaded::default(),
        Ok(Some(bytes)) => match decode(&bytes) {
            Ok(entries) => {
                return Loaded {
                    entries,
                    ignored: None,
                };
            }
            Err(why) => why,
        },
        Err(why) => why,
    };
    // Never follow or recurse: remove only the name itself (a symlink is
    // unlinked, not its target); a directory is left alone.
    let _ = fs::remove_file(path);
    Loaded {
        entries: Vec::new(),
        ignored: Some(why.to_string()),
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.tmp"))
}

/// Writes already encoded contents atomically (empty = remove the file).
pub fn write_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() {
        return match fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    if bytes.len() > MAX_FILE_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "oversized"));
    }
    let tmp = temp_path(path);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// The persisted set as the worker last wrote it (skips identical writes).
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    /// Encoding of the set on disk; empty = no file.
    on_disk: Vec<u8>,
}

impl Store {
    /// Opens `path` and returns the store plus what the file held.
    pub fn open(path: PathBuf) -> (Self, Loaded) {
        let loaded = load(&path);
        let on_disk = encode(&loaded.entries);
        (Self { path, on_disk }, loaded)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Makes the file hold exactly `entries` (no write when unchanged).
    pub fn write(&mut self, entries: &[SavedMode]) -> io::Result<()> {
        let bytes = encode(entries);
        if bytes == self.on_disk {
            return Ok(());
        }
        write_bytes(&self.path, &bytes)?;
        self.on_disk = bytes;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!(
            "kwe-led-saved-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn entry(name: &str, occurrence: u32, colors: usize) -> SavedMode {
        SavedMode {
            key: DeviceKey {
                name: name.into(),
                vendor: "V \"q\"\n".into(),
                serial: "SN".into(),
                location: "/dev/hidraw3".into(),
            },
            occurrence,
            index: 2,
            mode: Mode {
                name: "Rainbow".into(),
                value: -7,
                flags: 1 << 5,
                speed: 3,
                colors: vec![Rgb::new(1, 2, 3)],
                ..Mode::default()
            },
            colors: vec![Rgb::new(9, 8, 7); colors],
            control: Some(ControlMode {
                index: 0,
                name: "Direct".into(),
            }),
        }
    }

    #[test]
    fn round_trips_atomically_with_private_permissions() {
        let d = dir("rt");
        let path = d.join("led-saved-modes.json");
        let (mut store, loaded) = Store::open(path.clone());
        assert!(loaded.entries.is_empty() && loaded.ignored.is_none());
        assert!(!path.exists(), "nothing to remember, no file");
        let set = vec![entry("Kbd", 0, 3), entry("Kbd", 1, 0)];
        store.write(&set).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!temp_path(&path).exists(), "temp renamed away");
        assert_eq!(load(&path).entries, set);
        store.write(&[]).unwrap();
        assert!(!path.exists(), "empty set removes the file");
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn unusable_files_are_ignored_noted_and_removed() {
        let d = dir("bad");
        let path = d.join("s.json");
        let foreign = format!("{{\"version\":2,\"type\":\"{FILE_TYPE}\",\"entries\":[]}}");
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("not_json", b"{nope".to_vec()),
            ("unsupported_version", foreign.into_bytes()),
            (
                "wrong_type",
                b"{\"version\":1,\"type\":\"x\",\"entries\":[]}".to_vec(),
            ),
            (
                "bad_entry",
                format!("{{\"version\":1,\"type\":\"{FILE_TYPE}\",\"entries\":[{{\"name\":1}}]}}")
                    .into_bytes(),
            ),
            ("oversized", vec![b' '; MAX_FILE_BYTES + 1]),
        ];
        for (why, bytes) in cases {
            fs::write(&path, bytes).unwrap();
            let loaded = load(&path);
            assert!(loaded.entries.is_empty());
            assert_eq!(loaded.ignored.as_deref(), Some(why));
            assert!(
                !path.exists(),
                "{why}: removed so the next write replaces it"
            );
        }
        // A symlink is not followed; the link itself is removed.
        let target = d.join("target");
        fs::write(&target, encode(&[entry("K", 0, 0)])).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let loaded = load(&path);
        assert!(loaded.entries.is_empty() && loaded.ignored.is_some());
        assert!(target.exists() && fs::symlink_metadata(&path).is_err());
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn encoding_is_bounded_dropping_direct_colours_first() {
        let mut huge = entry(&"n".repeat(4095), 0, usize::from(limits::MAX_LEDS));
        huge.key.vendor = "v".repeat(4095);
        huge.key.serial = "s".repeat(4095);
        huge.key.location = "l".repeat(4095);
        huge.mode.name = "m".repeat(4095);
        huge.mode.colors = vec![Rgb::new(1, 2, 3); usize::from(limits::MAX_MODE_COLORS)];
        let set: Vec<SavedMode> = (0..limits::MAX_CONTROLLERS)
            .map(|i| SavedMode {
                occurrence: i,
                ..huge.clone()
            })
            .collect();
        let bytes = encode(&set);
        assert!(bytes.len() <= MAX_FILE_BYTES, "{}", bytes.len());
        let back = decode(&bytes).unwrap();
        assert_eq!(back.len(), set.len(), "every mode kept");
        assert!(back.iter().any(|e| e.colors.is_empty()));
    }
}
