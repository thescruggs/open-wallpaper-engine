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
//!
//! Provenance (review fixes C1): the file records the kernel boot id, and
//! its mtime is the writer's liveness — the worker refreshes it every
//! [`TOUCH_INTERVAL`] while the file exists, without rewriting content
//! ([`Store::touch`]). A successor trusts the entries only when they come
//! from the current boot and the writer was alive recently ([`judge`]).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

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

/// While the file exists its mtime is refreshed this often (liveness).
pub const TOUCH_INTERVAL: Duration = Duration::from_secs(5);
/// Where the kernel publishes the current boot's random id.
pub const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";
/// Longest accepted `boot_id` string (a UUID is 36).
const MAX_BOOT_ID_BYTES: usize = 64;

/// Where a loaded file came from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    /// The writer's boot id; `None` for a file without one (older worker).
    pub boot_id: Option<String>,
    /// The file's mtime: the writer's last write or liveness touch.
    pub modified: Option<SystemTime>,
}

/// Outcome of [`load`]: the entries, where they came from, and a note to
/// log when the file was ignored.
#[derive(Debug, Default)]
pub struct Loaded {
    pub entries: Vec<SavedMode>,
    pub provenance: Provenance,
    pub ignored: Option<String>,
}

/// The current boot id, or `None` when it cannot be read.
pub fn current_boot_id() -> Option<String> {
    let text = fs::read_to_string(BOOT_ID_PATH).ok()?;
    let id = text.trim();
    (!id.is_empty() && id.len() <= MAX_BOOT_ID_BYTES).then(|| id.to_string())
}

/// Whether entries loaded with `provenance` may be adopted: written during
/// the current boot (`current_boot`) by a worker alive within `window` of
/// `now` (mtime, either direction, so a clock step cannot make an old file
/// look fresh by more than the window). `Err` names why not:
/// `earlier_boot`, `unknown_boot` (no id on one side), `stale`.
pub fn judge(
    provenance: &Provenance,
    current_boot: Option<&str>,
    now: SystemTime,
    window: Duration,
) -> Result<(), &'static str> {
    match (provenance.boot_id.as_deref(), current_boot) {
        (Some(file), Some(ours)) if file == ours => {}
        (Some(_), Some(_)) => return Err("earlier_boot"),
        _ => return Err("unknown_boot"),
    }
    let age = match provenance.modified {
        Some(m) => now.duration_since(m).unwrap_or_else(|e| e.duration()),
        None => return Err("stale"),
    };
    if age > window {
        return Err("stale");
    }
    Ok(())
}

fn hex_colors(colors: &[Rgb]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(colors.len() * 6);
    for c in colors {
        for b in [c.r, c.g, c.b] {
            out.push(HEX[usize::from(b >> 4)]);
            out.push(HEX[usize::from(b & 15)]);
        }
    }
    // Only ASCII hex digits were pushed.
    String::from_utf8(out).unwrap_or_default()
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

/// Encodes `entries` (with the writer's `boot_id`) within
/// [`MAX_FILE_BYTES`]; an empty set encodes as an empty vector (= no file).
///
/// Every entry is serialised at most twice (with and without its Direct
/// colours) and the size budget is settled arithmetically, so the cost is
/// linear in the output (review fixes C1, R5: the old loop re-serialised
/// the whole set once per dropped colour list — ≈ 1.2 s for a hostile
/// maximal set).
pub fn encode(entries: &[SavedMode], boot_id: Option<&str>) -> Vec<u8> {
    if entries.is_empty() {
        return Vec::new();
    }
    let entries = &entries[..entries.len().min(limits::MAX_CONTROLLERS as usize)];
    let head = format!(
        "{{\"version\":{FILE_VERSION},\"type\":\"{FILE_TYPE}\",\"boot_id\":{},\"entries\":[",
        Value::from(boot_id.map(str::to_string))
    );
    let tail = "]}\n";
    let mut parts: Vec<String> = entries
        .iter()
        .map(|s| entry_json(s, true).to_string())
        .collect();
    let commas = entries.len() - 1;
    let mut total = head.len() + tail.len() + commas + parts.iter().map(String::len).sum::<usize>();
    if total > MAX_FILE_BYTES {
        // Too large: drop the biggest colour lists first (the mode itself
        // is still restored) ...
        let mut by_size: Vec<usize> = (0..entries.len())
            .filter(|i| !entries[*i].colors.is_empty())
            .collect();
        by_size.sort_by_key(|i| std::cmp::Reverse(entries[*i].colors.len()));
        for i in by_size {
            if total <= MAX_FILE_BYTES {
                break;
            }
            let bare = entry_json(&entries[i], false).to_string();
            total = total - parts[i].len() + bare.len();
            parts[i] = bare;
        }
        // ... then whole entries from the end.
        while total > MAX_FILE_BYTES && !parts.is_empty() {
            let last = parts.pop().map_or(0, |p| p.len());
            total -= last + usize::from(!parts.is_empty());
        }
    }
    let mut bytes = Vec::with_capacity(total);
    bytes.extend_from_slice(head.as_bytes());
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            bytes.push(b',');
        }
        bytes.extend_from_slice(p.as_bytes());
    }
    bytes.extend_from_slice(tail.as_bytes());
    bytes
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

/// Parses file contents: the entries and the writer's boot id (absent in
/// files of older workers). `Err` names why the file is ignored.
pub fn decode(bytes: &[u8]) -> Result<(Vec<SavedMode>, Option<String>), &'static str> {
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
    let boot_id = match o.get("boot_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) if !id.is_empty() && id.len() <= MAX_BOOT_ID_BYTES => {
            Some(id.clone())
        }
        Some(_) => return Err("bad_boot_id"),
    };
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
    Ok((out, boot_id))
}

fn open_nofollow(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
}

/// File bytes and mtime, as read by [`load`].
type RawFile = (Vec<u8>, Option<SystemTime>);

/// Reads the file. Missing → empty, quietly. Anything unusable → empty plus
/// a note, and the file is removed so the next write replaces it.
pub fn load(path: &Path) -> Loaded {
    let read = || -> Result<Option<RawFile>, &'static str> {
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
        let modified = meta.modified().ok();
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "unreadable")?;
        Ok(Some((bytes, modified)))
    };
    let why = match read() {
        Ok(None) => return Loaded::default(),
        Ok(Some((bytes, modified))) => match decode(&bytes) {
            Ok((entries, boot_id)) => {
                return Loaded {
                    entries,
                    provenance: Provenance { boot_id, modified },
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
        provenance: Provenance::default(),
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

/// The saved-modes file of one worker. It does not compare contents: the
/// caller writes only when its remembered set changed (the engine tracks
/// the client's [`kwe_openrgb::Client::saved_revision`]).
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    boot_id: Option<String>,
    /// The file exists (last write was non-empty, or it was loaded).
    exists: bool,
    next_touch: Option<Instant>,
    encodes: u64,
}

impl Store {
    /// Opens `path` and returns the store plus what the file held.
    pub fn open(path: PathBuf) -> (Self, Loaded) {
        Self::open_with_boot_id(path, current_boot_id())
    }

    /// [`Store::open`] with an explicit boot id (tests).
    pub fn open_with_boot_id(path: PathBuf, boot_id: Option<String>) -> (Self, Loaded) {
        let loaded = load(&path);
        let exists = !loaded.entries.is_empty();
        (
            Self {
                path,
                boot_id,
                exists,
                next_touch: None,
                encodes: 0,
            },
            loaded,
        )
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The boot id written into the file (and compared on adoption).
    pub fn boot_id(&self) -> Option<&str> {
        self.boot_id.as_deref()
    }

    /// How many times [`Store::write`] encoded the set (diagnostics/tests).
    pub fn encodes(&self) -> u64 {
        self.encodes
    }

    /// Makes the file hold exactly `entries` (removed when empty).
    pub fn write(&mut self, entries: &[SavedMode]) -> io::Result<()> {
        self.encodes += 1;
        let bytes = encode(entries, self.boot_id.as_deref());
        if bytes.is_empty() && !self.exists {
            return Ok(());
        }
        write_bytes(&self.path, &bytes)?;
        self.exists = !bytes.is_empty();
        // A write is itself a liveness mark.
        self.next_touch = Some(Instant::now() + TOUCH_INTERVAL);
        Ok(())
    }

    /// Liveness: while the file exists, refreshes its mtime once per
    /// [`TOUCH_INTERVAL`] without rewriting it. Returns true when touched.
    /// Errors are ignored (the next write recreates the file).
    pub fn touch(&mut self, now: Instant) -> bool {
        if !self.exists || self.next_touch.is_some_and(|t| now < t) {
            return false;
        }
        self.next_touch = Some(now + TOUCH_INTERVAL);
        let Ok(file) = open_nofollow(&self.path) else {
            return false;
        };
        match file.metadata() {
            Ok(meta) if meta.is_file() => file.set_modified(SystemTime::now()).is_ok(),
            _ => false,
        }
    }

    /// When [`Store::touch`] next has work (`None` while there is no file).
    pub fn next_touch(&self) -> Option<Instant> {
        if self.exists { self.next_touch } else { None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::UNIX_EPOCH;

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
        let (mut store, loaded) = Store::open_with_boot_id(path.clone(), Some("boot-a".into()));
        assert!(loaded.entries.is_empty() && loaded.ignored.is_none());
        assert!(!path.exists(), "nothing to remember, no file");
        let set = vec![entry("Kbd", 0, 3), entry("Kbd", 1, 0)];
        store.write(&set).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!temp_path(&path).exists(), "temp renamed away");
        let back = load(&path);
        assert_eq!(back.entries, set);
        assert_eq!(back.provenance.boot_id.as_deref(), Some("boot-a"));
        assert!(back.provenance.modified.is_some());
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
        fs::write(&target, encode(&[entry("K", 0, 0)], None)).unwrap();
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
        let t0 = Instant::now();
        let bytes = encode(&set, Some("boot"));
        let took = t0.elapsed();
        assert!(bytes.len() <= MAX_FILE_BYTES, "{}", bytes.len());
        let (back, boot) = decode(&bytes).unwrap();
        assert_eq!(boot.as_deref(), Some("boot"));
        assert_eq!(back.len(), set.len(), "every mode kept");
        assert!(back.iter().any(|e| e.colors.is_empty()));
        assert!(
            back.iter().any(|e| !e.colors.is_empty()),
            "only what must go"
        );
        // R5: one linear pass. The old loop took ≈ 1.2 s (release) here,
        // re-serialising everything once per dropped colour list. Generous
        // bound so a debug build on a slow machine passes.
        eprintln!("worst-case encode took {took:?}");
        assert!(took < Duration::from_millis(1000), "{took:?}");
    }

    #[test]
    fn provenance_is_judged_by_boot_and_liveness() {
        let now = SystemTime::now();
        let window = Duration::from_secs(60);
        let p = |boot: Option<&str>, age_s: i64| Provenance {
            boot_id: boot.map(str::to_string),
            modified: Some(if age_s >= 0 {
                now - Duration::from_secs(age_s as u64)
            } else {
                now + Duration::from_secs((-age_s) as u64)
            }),
        };
        assert_eq!(judge(&p(Some("a"), 5), Some("a"), now, window), Ok(()));
        assert_eq!(judge(&p(Some("a"), 60), Some("a"), now, window), Ok(()));
        assert_eq!(
            judge(&p(Some("a"), 61), Some("a"), now, window),
            Err("stale")
        );
        assert_eq!(
            judge(&p(Some("a"), -61), Some("a"), now, window),
            Err("stale"),
            "an mtime far in the future is not fresh either"
        );
        assert_eq!(
            judge(&p(Some("b"), 1), Some("a"), now, window),
            Err("earlier_boot")
        );
        assert_eq!(
            judge(&p(None, 1), Some("a"), now, window),
            Err("unknown_boot"),
            "file of an older worker"
        );
        assert_eq!(
            judge(&p(Some("a"), 1), None, now, window),
            Err("unknown_boot")
        );
        assert!(current_boot_id().is_some_and(|id| id.len() == 36));
    }

    #[test]
    fn touch_refreshes_mtime_without_rewriting_and_only_while_the_file_exists() {
        let d = dir("touch");
        let path = d.join("s.json");
        let (mut store, _) = Store::open_with_boot_id(path.clone(), Some("b".into()));
        let t0 = Instant::now();
        assert!(!store.touch(t0), "no file, nothing to touch");
        assert_eq!(store.next_touch(), None);
        store.write(&[entry("K", 0, 2)]).unwrap();
        let old = SystemTime::now() - Duration::from_secs(600);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let ino = || std::os::unix::fs::MetadataExt::ino(&fs::metadata(&path).unwrap());
        let before = (ino(), fs::read(&path).unwrap());
        assert!(!store.touch(t0), "within the interval after a write");
        assert!(store.touch(t0 + TOUCH_INTERVAL * 2));
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        assert!(modified > old + Duration::from_secs(590), "mtime refreshed");
        assert_eq!(
            (ino(), fs::read(&path).unwrap()),
            before,
            "content untouched"
        );
        assert!(!store.touch(t0 + TOUCH_INTERVAL * 2), "rate-limited");
        store.write(&[]).unwrap();
        assert!(!path.exists());
        assert!(!store.touch(t0 + TOUCH_INTERVAL * 10));
        assert_eq!(store.encodes(), 2);
        fs::remove_dir_all(d).unwrap();
    }
}
