// SPDX-License-Identifier: GPL-3.0-or-later
//! The worker's single-threaded core: OpenRGB connection state machine,
//! ambient/audio sources, mapping, smoothing, and the rate-bounded sink.
//! The caller (main loop) owns timing: it calls [`Engine::tick`] at most
//! `rate_hz` times per second and feeds control/audio between ticks.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use kwe_led_protocol::{
    AudioInfo, ConfigPatch, Counters, DeviceInfo, FrameInfo, FrameState, Mode, ServerInfo, State,
    StatusLine, VERSION, ZoneInfo, ZoneKind, cut, device_id, device_key_hash,
};
use kwe_openrgb::{
    Client, ClientConfig, Controller, ControllerId, DeviceKey, Error, Rgb, SavedMode, zone_type,
};

use crate::color::{Rgbf, Smoother, Taus, apply_brightness, smooth_scalar};
use crate::frame::{FrameSampler, Sample};
use crate::log::LogLimiter;
use crate::mapping::{
    AutoGain, ambient_zone, bass_energy, controller_colors, mono_raw, spectrum_zone, window_level,
};
use crate::saved::{Store, judge};

/// After the source disappears (no valid frame for the ambient modes, no
/// fresh audio for `spectrum`), the last colours are held this long; then
/// every device is released (its remembered mode restored) until a source
/// returns. Nothing is ever driven to black for lack of a source.
pub const FRAME_HOLD: Duration = Duration::from_secs(2);
/// A frame read within this long counts as "a frame is showing now" for
/// takeover (tolerates one busy read at the 4 Hz waiting tick).
pub const FRAME_NOW: Duration = Duration::from_millis(500);
/// An audio frame whose [`window_level`] (sum of the mono raw bands) is
/// below this is silence. Review fixes D (M1): the old rule (loudest
/// `sqrt(band)` < 0.05, i.e. raw 0.0025) called −20 dBFS music silent,
/// because `analyze_stereo` bands are ≈ 100× smaller than `0..1`.
///
/// Measured through `kwe_core::audio::analyze_stereo` (2048-sample
/// window): the window level is ≈ 0.43 × RMS for noise-like signals —
/// 4.3e-4 at −60 dBFS, 1.4e-3 at −50, 4.3e-3 at −40, 4.3e-2 at −20.
/// 6.4e-4 (≈ −56 dBFS) keeps digital silence, dither and noise-floor hiss
/// (−60 dBFS and below) silent and quiet-but-audible music (−50 dBFS and
/// up) sound, with ≥ 4 dB margin on either side and 16 dB to −40 dBFS.
///
/// This is the level sound must **reach** to start (a frame that never
/// got here is silence: hiss under it, or only blipping over it, never
/// takes over). Once sound has started it continues down to
/// [`SOUND_HOLD_LEVEL`] (review fixes E, R2).
pub const SILENCE_LEVEL: f32 = 6.4e-4;
/// Level hysteresis (review fixes E, R2): once sound has started —
/// frames reaching [`SILENCE_LEVEL`] present (one within [`AUDIO_FRESH`])
/// continuously for the takeover dwell (1 s), the same test a `spectrum`
/// takeover makes — following frames count as sound until one falls below
/// this; a gap in the frames longer than [`AUDIO_FRESH`] resets it.
/// One rule drives everything audio: the `spectrum` source and its 3 s
/// silence release, status `audio.silent`, the auto-gain, and the
/// `ambient+pulse` silence behaviour.
///
/// 3.2e-4 = half of [`SILENCE_LEVEL`] (6 dB): ≈ −62 dBFS (window level of
/// −62 dBFS noise 3.3e-4..3.6e-4, tone 3.1e-4..3.3e-4). Quiet tonal
/// material swelling ±10 dB around −48 dBFS reaches −58 dBFS in its
/// troughs (4.8e-4..5.3e-4, ≥ 3.5 dB above) and no longer releases there;
/// digital silence and hiss at −65 dBFS (≤ 2.6e-4, ≥ 1.8 dB below) still
/// release after 3 s. Cost: hiss between ≈ −62 and −56 dBFS that follows
/// sound keeps it going (it never starts it).
pub const SOUND_HOLD_LEVEL: f32 = 3.2e-4;
/// `spectrum` releases the devices after this long without sound even
/// while (silent) audio frames keep arriving. Gaps between songs or
/// sentences are shorter, so they do not release. (`ambient+pulse` does not
/// wait for it: without sound it eases to steady ambient at once, see
/// [`PULSE_RESTING_TAUS`].)
pub const SILENCE_RELEASE: Duration = Duration::from_secs(3);
/// A source must be present continuously this long before devices are
/// taken over (filters blips: a single audio burst, a frame published for
/// an instant).
pub const TAKEOVER_DWELL: Duration = Duration::from_secs(1);
/// Devices are not taken over again sooner than this after a release (a
/// flapping source, or the daemon's pause toggling mode `off` on every
/// fullscreen switch, costs at most one mode-switch cycle per this long).
/// A mode change between two non-`off` modes, or enabling a device, skips
/// it once.
pub const MIN_RETAKE: Duration = Duration::from_secs(5);
/// Entries adopted from the saved-modes file are trusted only when the
/// file is from this boot and its writer was alive (mtime) within this
/// long. Covers a crash respawn (1 s), a daemon restart by systemd (2 s +
/// start), and a wedged worker killed by the daemon (no touch for up to
/// 30 s stale + 4 s grace + 1 s respawn), plus the 5 s touch interval.
pub const SAVED_FRESH: Duration = Duration::from_secs(60);
/// Tick period while connected but not driving (waiting for a source):
/// bounds how fast a returning source is picked up.
pub const IDLE_TICK: Duration = Duration::from_millis(250);
/// Tick period in mode `off` (drains server notifications, retries a
/// failed restore).
pub const OFF_TICK: Duration = Duration::from_secs(1);
/// A tick stops starting device operations (take-control, restore) after
/// this long; the remaining devices are handled from the next tick on.
pub const TICK_BUDGET: Duration = Duration::from_millis(500);
/// Status `reason` on `active` while no device is driven.
pub const WAITING_FOR_SOURCE: &str = "waiting_for_source";
/// An audio frame older than this counts as silence.
pub const AUDIO_FRESH: Duration = Duration::from_millis(500);
/// Unchanged colours are re-sent at most this often (keeps devices in sync
/// if another client painted them, without a packet per tick).
pub const KEEPALIVE: Duration = Duration::from_secs(2);
pub const AMBIENT_TAUS: Taus = Taus::symmetric(0.25);
pub const SPECTRUM_TAUS: Taus = Taus {
    attack: 0.03,
    release: 0.2,
};
pub const PULSE_TAUS: Taus = Taus {
    attack: 0.03,
    release: 0.25,
};
/// Brightness floor of `ambient+pulse` while sound plays: the pulse moves
/// the ambient colours between 50 % (no bass right now) and 100 % (bass at
/// the running peak). Review fixes D: 0.2 sat near-dark between beats; in
/// sustained silence the colours are steady 100 % ambient (unchanged).
pub const PULSE_FLOOR: f32 = 0.5;
/// `ambient+pulse` without sound (a silent frame, or no fresh audio)
/// eases the pulse up to 1 with the pulse release time constant (review
/// fixes E, R4): a pause never dips to [`PULSE_FLOOR`], and the step into
/// steady ambient is at most `0.5 × (1 − e^(−dt/0.25 s))` per tick
/// (≤ 0.091 at 20 Hz). Was: silent frames pulled it to the floor, then it
/// snapped to 1 with the 30 ms attack after 3 s.
pub const PULSE_RESTING_TAUS: Taus = Taus::symmetric(0.25);

/// Default probe backoff: 1 s doubling to 60 s.
pub const BACKOFF_MIN: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Hysteresis and silence timings (hidden CLI flags scale them in tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timings {
    pub takeover_dwell: Duration,
    pub min_retake: Duration,
    pub silence_release: Duration,
    pub saved_fresh: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            takeover_dwell: TAKEOVER_DWELL,
            min_retake: MIN_RETAKE,
            silence_release: SILENCE_RELEASE,
            saved_fresh: SAVED_FRESH,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub client: ClientConfig,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub timings: Timings,
}

impl EngineConfig {
    /// Worker defaults: tighter I/O bounds than the client's defaults so a
    /// stalled server delays a tick (and a stop) by at most ~1.5 s.
    pub fn with_port(port: u16) -> Self {
        Self {
            client: ClientConfig {
                port,
                client_name: "KDE Wallpaper Engine LED sync".into(),
                connect_timeout: Duration::from_millis(250),
                io_timeout: Duration::from_millis(500),
                enumerate_budget: Duration::from_millis(1500),
            },
            backoff_min: BACKOFF_MIN,
            backoff_max: BACKOFF_MAX,
            timings: Timings::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub mode: Mode,
    pub brightness: u8,
    pub rate_hz: u8,
    pub disabled: BTreeSet<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::Ambient,
            brightness: kwe_led_protocol::BRIGHTNESS_DEFAULT,
            rate_hz: kwe_led_protocol::RATE_DEFAULT_HZ,
            disabled: BTreeSet::new(),
        }
    }
}

impl Settings {
    /// Applies an already validated patch.
    pub fn apply(&mut self, patch: ConfigPatch) {
        if let Some(m) = patch.mode {
            self.mode = m;
        }
        if let Some(b) = patch.brightness {
            self.brightness = b.min(kwe_led_protocol::BRIGHTNESS_MAX);
        }
        if let Some(r) = patch.rate_hz {
            self.rate_hz = r.clamp(kwe_led_protocol::RATE_MIN_HZ, kwe_led_protocol::RATE_MAX_HZ);
        }
        if let Some(list) = patch.disabled_devices {
            self.disabled = list.into_iter().collect();
        }
    }

    pub fn tick_interval(&self) -> Duration {
        Duration::from_millis(1000 / u64::from(self.rate_hz.max(1)))
    }
}

enum Conn {
    Down {
        next_probe: Instant,
        backoff: Duration,
    },
    Up(Box<Client>),
}

/// Machine-readable reason for an OpenRGB error (status `reason`).
pub fn error_reason(e: &Error) -> &'static str {
    match e {
        Error::ServerAbsent { .. } => "server_absent",
        Error::Timeout { .. } => "timeout",
        Error::Disconnected => "disconnected",
        Error::Io { .. } => "io_error",
        Error::UnsupportedVersion { .. } => "unsupported_version",
        Error::BadMagic
        | Error::PacketTooLarge { .. }
        | Error::TooManyUnsolicited { .. }
        | Error::Malformed { .. }
        | Error::LimitExceeded { .. } => "protocol_error",
        Error::DeviceListChanged
        | Error::UnknownController(_)
        | Error::DeviceGone
        | Error::ModeGone
        | Error::InvalidArgument(_) => "device_error",
        _ => "error",
    }
}

fn controller_id(c: &Controller) -> String {
    device_id(&c.name, &c.vendor, &c.serial, &c.location)
}

/// Internal per-controller key: full-key hash + occurrence among identical
/// keys (two identical units are two slots, one settings id).
fn slot_key(key: &DeviceKey, occurrence: u32) -> String {
    format!(
        "{}/{occurrence}",
        device_key_hash(&key.name, &key.vendor, &key.serial, &key.location)
    )
}

/// `(id, settings id, slot, LED count)` for every controller, list order.
fn device_slots(client: &Client) -> Vec<(ControllerId, String, String, usize)> {
    let mut seen: HashMap<DeviceKey, u32> = HashMap::new();
    client
        .controllers()
        .iter()
        .map(|c| {
            let key = c.key();
            let n = seen.entry(key.clone()).or_insert(0);
            let slot = slot_key(&key, *n);
            *n += 1;
            (c.id, controller_id(c), slot, c.leds.len())
        })
        .collect()
}

fn merge_saved(into: &mut Vec<SavedMode>, more: Vec<SavedMode>) {
    for s in more {
        if !into
            .iter()
            .any(|c| c.key == s.key && c.occurrence == s.occurrence)
            && into.len() < kwe_openrgb::limits::MAX_CONTROLLERS as usize
        {
            into.push(s);
        }
    }
}

fn device_error(
    counters: &mut Counters,
    log: &mut LogLimiter,
    now: Instant,
    id: &str,
    op: &str,
    e: &Error,
) {
    counters.update_errors = counters.update_errors.saturating_add(1);
    let detail = format!("device={id} op={op} reason={}", error_reason(e));
    log.log(now, "update_error", &detail);
}

/// Outcome of the bounded restore performed on stop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestoreOutcome {
    pub attempted: u32,
    pub restored: u32,
    pub failed: u32,
}

/// What the stop path needs to restore devices without the engine.
pub struct RestoreParts {
    client: Option<Box<Client>>,
    carried: Vec<SavedMode>,
    config: ClientConfig,
    store: Option<Store>,
}

impl RestoreParts {
    /// Best-effort restore of every remembered mode. With no live
    /// connection but remembered modes (server dropped mid-run, or modes
    /// adopted from the saved-modes file), one bounded reconnect is
    /// attempted. Bounded by the client's timeouts; the caller additionally
    /// runs this on a thread with a deadline. The saved-modes file is then
    /// made to hold exactly what is still unrestored (removed when empty);
    /// when nothing could be attempted it is left untouched.
    pub fn restore(mut self) -> RestoreOutcome {
        let mut client = match self.client.take() {
            Some(c) if !c.is_connection_lost() => *c,
            other => {
                if let Some(mut c) = other {
                    merge_saved(&mut self.carried, c.take_saved_modes());
                }
                if self.carried.is_empty() {
                    return RestoreOutcome::default();
                }
                match Client::connect(&self.config).and_then(|mut c| {
                    c.refresh()?;
                    Ok(c)
                }) {
                    Ok(mut c) => {
                        c.adopt_saved_modes(std::mem::take(&mut self.carried));
                        c.drop_stale_saved_modes();
                        c
                    }
                    Err(_) => {
                        if let Some(store) = &mut self.store {
                            let _ = store.write(&self.carried);
                        }
                        return RestoreOutcome {
                            attempted: self.carried.len() as u32,
                            restored: 0,
                            failed: self.carried.len() as u32,
                        };
                    }
                }
            }
        };
        client.adopt_saved_modes(std::mem::take(&mut self.carried));
        let reports = client.restore_all();
        if let Some(store) = &mut self.store {
            let _ = store.write(client.saved_modes());
        }
        let restored = reports.iter().filter(|r| r.result.is_ok()).count() as u32;
        RestoreOutcome {
            attempted: reports.len() as u32,
            restored,
            failed: reports.len() as u32 - restored,
        }
    }

    /// Nothing to restore: makes sure no saved-modes file is left (a
    /// release in the tick the stop interrupted may not have been
    /// persisted). Bounded: at most one unlink.
    pub fn settle_empty(mut self) {
        if let Some(store) = &mut self.store {
            let _ = store.write(&[]);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.carried.is_empty()
            && self
                .client
                .as_ref()
                .is_none_or(|c| c.saved_mode_count() == 0)
    }
}

/// `--restore-saved`: operations are started only within this long of the
/// pass's start; with the in-flight operation (≤ 0.5 s I/O timeout) the
/// pass ends within ≈ 2.5 s, inside the daemon's 3 s hard bound
/// (`RESTORE_PASS_LIMIT`) and its 4 s stop grace.
pub const RESTORE_SAVED_BUDGET: Duration = Duration::from_secs(2);

/// What a `--restore-saved` pass did (review fixes D, R-a).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreSaved {
    /// No file, an unusable file (removed), or an empty one.
    NoFile,
    /// The file was not trusted (other boot / stale): removed, devices
    /// left alone (same rule as a normal worker's adoption).
    Forgotten(&'static str),
    /// OpenRGB unreachable (or the connection failed): file untouched.
    Unreachable { kept: u32 },
    /// Restored what it could; `kept` entries stay in the file (device
    /// missing, failure, or out of time).
    Done {
        restored: u32,
        forgotten: u32,
        kept: u32,
    },
}

/// One bounded restore-only pass over the saved-modes file at `path`
/// (`--restore-saved`, run by the daemon when a worker died uncleanly and
/// no successor will run). Applies the normal adoption rules (same boot,
/// writer alive within `saved_fresh`), connects once, restores every
/// remembered device it finds, and rewrites the file to hold what is left
/// (removed when nothing is). It never switches a device to the per-LED
/// mode, and leaves the file untouched when it cannot connect (no
/// liveness touch either, so the file still ages out).
pub fn restore_saved(
    path: PathBuf,
    mut client_cfg: ClientConfig,
    saved_fresh: Duration,
    wall: SystemTime,
    boot_id: Option<String>,
    log: &mut LogLimiter,
) -> RestoreSaved {
    let start = Instant::now();
    let deadline = start + RESTORE_SAVED_BUDGET;
    let (mut store, loaded) = Store::open_with_boot_id(path, boot_id);
    if let Some(why) = &loaded.ignored {
        log.log(
            start,
            "restore_saved",
            &format!("detail=ignored reason={why}"),
        );
        return RestoreSaved::NoFile;
    }
    if loaded.entries.is_empty() {
        return RestoreSaved::NoFile;
    }
    if let Err(why) = judge(&loaded.provenance, store.boot_id(), wall, saved_fresh) {
        let _ = store.write(&[]);
        log.log(
            start,
            "restore_saved",
            &format!(
                "detail=forgotten reason={why} entries={}",
                loaded.entries.len()
            ),
        );
        return RestoreSaved::Forgotten(why);
    }
    let entries = loaded.entries.len() as u32;
    // Enumeration must also end inside the budget.
    client_cfg.enumerate_budget = client_cfg
        .enumerate_budget
        .min(deadline.saturating_duration_since(Instant::now()));
    let connected = Client::connect(&client_cfg).and_then(|mut c| {
        c.refresh()?;
        Ok(c)
    });
    let mut client = match connected {
        Ok(c) => c,
        Err(e) => {
            log.log(
                Instant::now(),
                "restore_saved",
                &format!(
                    "detail=unreachable reason={} kept={entries}",
                    error_reason(&e)
                ),
            );
            return RestoreSaved::Unreachable { kept: entries };
        }
    };
    client.adopt_saved_modes(loaded.entries);
    let forgotten = client.drop_stale_saved_modes().len() as u32;
    let reports = client.restore_all_until(&|| Instant::now() >= deadline);
    let restored = reports.iter().filter(|r| r.result.is_ok()).count() as u32;
    let kept = client.saved_modes().len() as u32;
    if restored > 0 || forgotten > 0 || kept < entries {
        let _ = store.write(client.saved_modes());
    }
    log.log(
        Instant::now(),
        "restore_saved",
        &format!("detail=done restored={restored} forgotten={forgotten} kept={kept}"),
    );
    RestoreSaved::Done {
        restored,
        forgotten,
        kept,
    }
}

pub struct Engine {
    cfg: EngineConfig,
    settings: Settings,
    frame_path: Option<PathBuf>,
    conn: Conn,
    /// Remembered modes while not connected (from a lost connection or the
    /// saved-modes file); adopted by the next connection.
    carried: Vec<SavedMode>,
    /// Slots ([`slot_key`]) this worker switched to its per-LED mode and
    /// has not restored.
    controlled: HashSet<String>,
    /// True while a source is live and devices are being driven.
    driving: bool,
    /// Device index to start the next drive loop at (after a tick ran out
    /// of budget), so no device starves.
    resume_at: usize,
    store: Option<Store>,
    sampler: FrameSampler,
    frame_state: FrameState,
    frame_ok_at: Option<Instant>,
    /// Latest audio frame: arrival and display bands (auto-gained, `0..=1`).
    audio: Option<(Instant, Vec<f32>)>,
    /// Display gain shared by spectrum and the bass pulse.
    gain: AutoGain,
    /// When the last audio frame with sound arrived (see [`SOUND_HOLD_LEVEL`]).
    loud_at: Option<Instant>,
    /// The latest audio frame was sound: it reached [`SILENCE_LEVEL`], or
    /// sound is held and it stayed ≥ [`SOUND_HOLD_LEVEL`].
    sounding: bool,
    /// Level hysteresis (R2): sound has started — frames reaching
    /// [`SILENCE_LEVEL`] were present (one within [`AUDIO_FRESH`])
    /// continuously for [`Timings::takeover_dwell`] — so quieter frames
    /// down to [`SOUND_HOLD_LEVEL`] still count as sound.
    sound_held: bool,
    /// Last frame reaching [`SILENCE_LEVEL`], and the start of the current
    /// unbroken stretch of such frames (none more than [`AUDIO_FRESH`] apart).
    start_level_at: Option<Instant>,
    start_level_since: Option<Instant>,
    /// Start of the current unbroken stretch with a source present now.
    live_since: Option<Instant>,
    /// When devices were last given back (a release that restored any).
    released_at: Option<Instant>,
    /// A user action (mode change between non-`off` modes, device enabled)
    /// lets the next takeover skip [`Timings::min_retake`] once.
    retake_bypass: bool,
    /// Bumped whenever the remembered set moves between the client and
    /// `carried` (connect, connection lost, file adoption); with the
    /// client's revision it tells [`Engine::persist`] whether anything
    /// changed since the last write.
    saved_epoch: u64,
    persisted: Option<(u64, u64)>,
    /// Slots whose remembered device was reported missing (logged once).
    gone_noted: HashSet<String>,
    pulse: f32,
    smoothers: HashMap<String, Smoother>,
    last_sent: HashMap<String, (Instant, Vec<Rgb>)>,
    state: State,
    reason: Option<String>,
    counters: Counters,
    last_tick: Option<Instant>,
    log: LogLimiter,
}

impl Engine {
    pub fn new(
        cfg: EngineConfig,
        settings: Settings,
        frame_path: Option<PathBuf>,
        now: Instant,
        log: LogLimiter,
    ) -> Self {
        let backoff = cfg.backoff_min;
        Self {
            cfg,
            settings,
            frame_state: if frame_path.is_some() {
                FrameState::Absent
            } else {
                FrameState::None
            },
            frame_path,
            conn: Conn::Down {
                next_probe: now,
                backoff,
            },
            carried: Vec::new(),
            controlled: HashSet::new(),
            driving: false,
            resume_at: 0,
            store: None,
            sampler: FrameSampler::default(),
            frame_ok_at: None,
            audio: None,
            gain: AutoGain::default(),
            loud_at: None,
            sounding: false,
            sound_held: false,
            start_level_at: None,
            start_level_since: None,
            live_since: None,
            released_at: None,
            retake_bypass: false,
            saved_epoch: 0,
            persisted: None,
            gone_noted: HashSet::new(),
            pulse: 1.0,
            smoothers: HashMap::new(),
            last_sent: HashMap::new(),
            state: State::Connecting,
            reason: None,
            counters: Counters::default(),
            last_tick: None,
            log,
        }
    }

    /// Persists remembered modes at `path` (`--saved-modes`): adopts what
    /// the file holds (a previous worker that died without restoring) and
    /// keeps the file a superset of the devices this worker switched.
    ///
    /// Adoption is conservative (review fixes C1): the entries are trusted
    /// only when the file is from the current boot and its writer was alive
    /// within [`Timings::saved_fresh`] (see [`judge`]). Otherwise every
    /// entry is forgotten (one log line) and the devices are left alone: a
    /// device still in the recorded per-LED mode may be showing lighting
    /// the user applied since (OpenRGB profiles and effects use `Direct`).
    pub fn with_saved_modes(self, path: PathBuf, now: Instant) -> Self {
        let (store, loaded) = Store::open(path);
        self.adopt_store(store, loaded, now, SystemTime::now())
    }

    /// [`Engine::with_saved_modes`] with an opened store and an explicit
    /// wall-clock time (tests).
    pub fn adopt_store(
        mut self,
        mut store: Store,
        loaded: crate::saved::Loaded,
        now: Instant,
        wall: SystemTime,
    ) -> Self {
        if let Some(why) = &loaded.ignored {
            self.log.log(
                now,
                "saved_modes",
                &format!("detail=ignored reason={why} path={:?}", store.path()),
            );
        } else if !loaded.entries.is_empty() {
            match judge(
                &loaded.provenance,
                store.boot_id(),
                wall,
                self.cfg.timings.saved_fresh,
            ) {
                Ok(()) => {
                    self.log.log(
                        now,
                        "saved_modes",
                        &format!("detail=adopted entries={}", loaded.entries.len()),
                    );
                    merge_saved(&mut self.carried, loaded.entries);
                }
                Err(why) => {
                    // Remove it now: liveness touches must never make an
                    // untrusted file look fresh to a later worker.
                    let _ = store.write(&[]);
                    self.log.log(
                        now,
                        "saved_modes",
                        &format!(
                            "detail=forgotten reason={why} entries={}",
                            loaded.entries.len()
                        ),
                    );
                }
            }
        }
        self.saved_epoch += 1;
        self.store = Some(store);
        self
    }

    /// The saved-modes store, if any (tests, diagnostics).
    pub fn store(&self) -> Option<&Store> {
        self.store.as_ref()
    }

    /// Liveness of the saved-modes file: refreshes its mtime at most every
    /// [`crate::saved::TOUCH_INTERVAL`] while it exists. Cheap; call every
    /// loop iteration.
    pub fn housekeeping(&mut self, now: Instant) {
        if let Some(store) = self.store.as_mut() {
            store.touch(now);
        }
    }

    /// When [`Engine::housekeeping`] next has work.
    pub fn next_housekeeping(&self) -> Option<Instant> {
        self.store.as_ref().and_then(Store::next_touch)
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.conn, Conn::Up(_))
    }

    pub fn counters_mut(&mut self) -> &mut Counters {
        &mut self.counters
    }

    /// Rate-limited diagnostic through the engine's shared log budget.
    pub fn log(&mut self, now: Instant, event: &str, detail: &str) {
        self.log.log(now, event, detail);
    }

    pub fn apply_config(&mut self, patch: ConfigPatch) {
        let (old_mode, old_disabled) = (self.settings.mode, self.settings.disabled.clone());
        self.settings.apply(patch);
        let mode = self.settings.mode;
        let switched = old_mode != mode && old_mode != Mode::Off && mode != Mode::Off;
        let enabled_one = old_disabled
            .difference(&self.settings.disabled)
            .next()
            .is_some();
        // Mode off → on is not a bypass: the daemon's pause uses exactly
        // that, and it must stay bounded.
        if switched || enabled_one {
            self.retake_bypass = true;
        }
    }

    pub fn set_frame_path(&mut self, path: Option<PathBuf>) {
        if self.frame_path == path {
            return;
        }
        self.sampler.forget_source();
        self.frame_state = if path.is_some() {
            FrameState::Absent
        } else {
            FrameState::None
        };
        // Keep `frame_ok_at`: a wallpaper switch holds the old colours for
        // FRAME_HOLD instead of blinking while the new renderer starts.
        self.frame_path = path;
    }

    pub fn frame_path(&self) -> Option<&Path> {
        self.frame_path.as_deref()
    }

    pub fn set_audio(&mut self, now: Instant, left: &[f32], right: &[f32]) {
        self.counters.audio_frames = self.counters.audio_frames.saturating_add(1);
        let raw = mono_raw(left, right);
        let level = window_level(&raw);
        let dt = self.audio.as_ref().map_or(0.0, |(t, _)| {
            now.saturating_duration_since(*t).as_secs_f32()
        });
        // Level hysteresis (R2): sound starts at SILENCE_LEVEL (held once
        // present for the takeover dwell, exactly what a spectrum takeover
        // needs) and, while held and frames keep arriving, continues down
        // to SOUND_HOLD_LEVEL. Isolated frames over SILENCE_LEVEL in hiss
        // never start it.
        if !Self::within(self.audio.as_ref().map(|(t, _)| *t), now, AUDIO_FRESH) {
            self.sound_held = false;
        }
        if level >= SILENCE_LEVEL {
            if !Self::within(self.start_level_at, now, AUDIO_FRESH) {
                self.start_level_since = Some(now);
            }
            self.start_level_at = Some(now);
        }
        if self.start_level_since.is_some_and(|since| {
            Self::within(self.start_level_at, now, AUDIO_FRESH)
                && now.saturating_duration_since(since) >= self.cfg.timings.takeover_dwell
        }) {
            self.sound_held = true;
        }
        if level < SOUND_HOLD_LEVEL {
            self.sound_held = false;
        }
        let sound = level >= SILENCE_LEVEL || self.sound_held;
        self.sounding = sound;
        if sound {
            self.loud_at = Some(now);
        }
        let bands = self.gain.step(dt, &raw, sound);
        self.audio = Some((now, bands));
    }

    /// Probe now (resets the backoff) when not connected.
    pub fn reprobe(&mut self, now: Instant) {
        if let Conn::Down {
            next_probe,
            backoff,
        } = &mut self.conn
        {
            *next_probe = now;
            *backoff = self.cfg.backoff_min;
        }
    }

    /// When the engine next needs a tick, given a tick that started at
    /// `tick_start`: the next probe while disconnected (no frame reads, no
    /// periodic wake-ups), the LED rate while driving, [`IDLE_TICK`] while
    /// waiting for a source, [`OFF_TICK`] in mode `off`.
    pub fn next_tick(&self, tick_start: Instant) -> Instant {
        match &self.conn {
            Conn::Down { next_probe, .. } => *next_probe,
            Conn::Up(_) if self.settings.mode == Mode::Off => tick_start + OFF_TICK,
            Conn::Up(_) if self.driving => tick_start + self.settings.tick_interval(),
            Conn::Up(_) => tick_start + IDLE_TICK,
        }
    }

    fn fresh_bands(&self, now: Instant) -> Option<&[f32]> {
        match &self.audio {
            Some((t, b)) if now.saturating_duration_since(*t) <= AUDIO_FRESH => Some(b),
            _ => None,
        }
    }

    fn within(at: Option<Instant>, now: Instant, d: Duration) -> bool {
        at.is_some_and(|t| now.saturating_duration_since(t) <= d)
    }

    /// Fresh audio frames are arriving but carry no sound.
    fn silent(&self, now: Instant) -> bool {
        self.fresh_bands(now).is_some() && !Self::within(self.loud_at, now, AUDIO_FRESH)
    }

    /// Sound was heard recently enough to keep reacting to it: within
    /// [`Timings::silence_release`] and with frames still (nearly) fresh.
    fn sound_recent(&self, now: Instant) -> bool {
        Self::within(self.loud_at, now, self.cfg.timings.silence_release)
            && Self::within(
                self.audio.as_ref().map(|(t, _)| *t),
                now,
                AUDIO_FRESH + FRAME_HOLD,
            )
    }

    /// The selected mode has something real to show right now (takeover
    /// needs this continuously for [`Timings::takeover_dwell`]).
    fn source_now(&self, now: Instant) -> bool {
        match self.settings.mode {
            Mode::Off => false,
            Mode::Ambient | Mode::AmbientPulse => {
                Self::within(self.frame_ok_at, now, FRAME_NOW) && self.sampler.grid().is_some()
            }
            Mode::Spectrum => {
                self.fresh_bands(now).is_some() && Self::within(self.loud_at, now, AUDIO_FRESH)
            }
        }
    }

    /// The selected mode has something real to show (or is inside the
    /// hold after its source went away). Sustained silence is no source:
    /// `spectrum` would otherwise paint every device black for as long as
    /// nothing plays.
    fn source_live(&self, now: Instant) -> bool {
        match self.settings.mode {
            Mode::Off => false,
            Mode::Ambient | Mode::AmbientPulse => {
                Self::within(self.frame_ok_at, now, FRAME_HOLD) && self.sampler.grid().is_some()
            }
            Mode::Spectrum => self.sound_recent(now),
        }
    }

    /// Hysteresis for taking devices over (they are not driven now).
    fn may_take(&self, now: Instant) -> bool {
        let t = self.cfg.timings;
        self.live_since
            .is_some_and(|since| now.saturating_duration_since(since) >= t.takeover_dwell)
            && (self.retake_bypass
                || self
                    .released_at
                    .is_none_or(|r| now.saturating_duration_since(r) >= t.min_retake))
    }

    fn set_state(&mut self, now: Instant, state: State, reason: Option<&str>) {
        let reason = reason.map(str::to_owned);
        if self.state == state && self.reason == reason {
            return;
        }
        self.state = state;
        self.reason = reason;
        let (proto, devices) = match &self.conn {
            Conn::Up(c) => (c.protocol_version(), c.controllers().len()),
            Conn::Down { .. } => (0, 0),
        };
        let detail = format!(
            "state={} reason={} protocol={proto} devices={devices}",
            serde_json::to_value(state)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
            self.reason.as_deref().unwrap_or("none"),
        );
        self.log.log(now, "state", &detail);
    }

    fn connected_state(&mut self, now: Instant) {
        if self.settings.mode == Mode::Off {
            self.set_state(now, State::Off, None);
        } else if self.driving {
            self.set_state(now, State::Active, None);
        } else {
            self.set_state(now, State::Active, Some(WAITING_FOR_SOURCE));
        }
    }

    fn read_frame(&mut self, now: Instant) {
        let Some(path) = self.frame_path.clone() else {
            self.frame_state = FrameState::None;
            return;
        };
        let (state, detail) = match self.sampler.read(&path) {
            Sample::Fresh => {
                self.counters.frames_read = self.counters.frames_read.saturating_add(1);
                self.frame_ok_at = Some(now);
                (FrameState::Ok, None)
            }
            Sample::Unchanged => {
                self.frame_ok_at = Some(now);
                (FrameState::Ok, None)
            }
            Sample::Absent => (FrameState::Absent, None),
            Sample::Invalid(why) => (FrameState::Invalid, Some(why)),
            Sample::Busy => (FrameState::Busy, None),
        };
        if state != self.frame_state && state != FrameState::Busy {
            let detail = format!(
                "state={} detail={}",
                serde_json::to_value(state)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                detail.unwrap_or("none")
            );
            self.log.log(now, "frame", &detail);
        }
        self.frame_state = state;
    }

    /// One probe: connect, then (unless a stop arrived meanwhile)
    /// enumerate. Never drives in the same tick, so a tick that connected
    /// holds at most one in-flight bounded operation after a stop request.
    fn try_connect(&mut self, now: Instant, stop: &dyn Fn() -> bool) {
        let Conn::Down {
            next_probe,
            backoff,
        } = self.conn
        else {
            return;
        };
        if now < next_probe {
            return;
        }
        let connected = Client::connect(&self.cfg.client).and_then(|mut c| {
            if stop() {
                // Between connect and enumerate: do not start the longest
                // bounded operation when the worker is about to exit.
                return Err(Error::Disconnected);
            }
            c.refresh()?;
            Ok(c)
        });
        if stop() {
            return;
        }
        match connected {
            Ok(mut client) => {
                client.adopt_saved_modes(std::mem::take(&mut self.carried));
                self.saved_epoch += 1;
                self.forget_stale(now, &mut client);
                self.counters.connects = self.counters.connects.saturating_add(1);
                self.conn = Conn::Up(Box::new(client));
                self.driving = false;
                self.connected_state(now);
            }
            Err(e) => {
                let next = (backoff * 2).min(self.cfg.backoff_max);
                self.conn = Conn::Down {
                    next_probe: now + backoff,
                    backoff: next,
                };
                if e.is_server_absent() {
                    self.set_state(now, State::ServerAbsent, None);
                } else {
                    self.set_state(now, State::Degraded, Some(error_reason(&e)));
                }
            }
        }
    }

    /// Forgets remembered modes whose device is listed in another mode than
    /// the recorded per-LED one (the user changed it), with a log line each.
    fn forget_stale(&mut self, now: Instant, client: &mut Client) {
        for stale in client.drop_stale_saved_modes() {
            let detail = format!(
                "detail=forgotten reason=device_left_control_mode device={:?}",
                stale.key.name
            );
            self.log.log(now, "saved_modes", &detail);
        }
    }

    /// After a re-enumeration: devices no longer listed are no longer
    /// controlled (a returning one is switched again, its original still
    /// remembered), and remembered devices that came back in another mode
    /// are forgotten.
    fn after_refresh(&mut self, now: Instant, client: &mut Client) {
        let present: HashSet<String> = device_slots(client)
            .into_iter()
            .map(|(_, _, slot, _)| slot)
            .collect();
        self.controlled.retain(|slot| present.contains(slot));
        self.forget_stale(now, client);
    }

    fn connection_lost(&mut self, now: Instant, client: &mut Client, e: &Error) {
        merge_saved(&mut self.carried, client.take_saved_modes());
        self.saved_epoch += 1;
        self.controlled.clear();
        self.last_sent.clear();
        self.smoothers.clear();
        self.driving = false;
        self.counters.disconnects = self.counters.disconnects.saturating_add(1);
        // First re-probe soon (a restarting server comes back quickly),
        // then the normal doubling backoff.
        self.conn = Conn::Down {
            next_probe: now + self.cfg.backoff_min,
            backoff: self.cfg.backoff_min,
        };
        self.set_state(now, State::Degraded, Some(error_reason(e)));
    }

    /// Keeps the saved-modes file equal to what is remembered right now.
    /// Encodes and writes only when the remembered set changed since the
    /// last successful write (epoch + client revision; review fixes C1 R5 —
    /// it used to re-encode the whole set every tick). A failed write is
    /// retried next tick.
    fn persist(&mut self, now: Instant) {
        let Some(store) = self.store.as_mut() else {
            return;
        };
        let (key, current): ((u64, u64), &[SavedMode]) = match &self.conn {
            Conn::Up(c) => ((self.saved_epoch, c.saved_revision()), c.saved_modes()),
            Conn::Down { .. } => ((self.saved_epoch, u64::MAX), &self.carried),
        };
        if self.persisted == Some(key) {
            return;
        }
        match store.write(current) {
            Ok(()) => self.persisted = Some(key),
            Err(e) => {
                let detail = format!("detail=write_failed kind={:?}", e.kind());
                self.log.log(now, "saved_modes", &detail);
            }
        }
    }

    /// One LED tick: keep the connection alive, sample the source the mode
    /// needs, and either drive (at most one `UpdateLEDs` per enabled device)
    /// or — with no source — release every device. `stop` is polled between
    /// connect phases and between device operations.
    pub fn tick(&mut self, now: Instant, stop: &dyn Fn() -> bool) {
        self.counters.ticks = self.counters.ticks.saturating_add(1);
        let interval = self.settings.tick_interval().as_secs_f32();
        let dt = self
            .last_tick
            .map(|t| now.saturating_duration_since(t).as_secs_f32())
            .unwrap_or(interval)
            .min(0.5);
        self.last_tick = Some(now);

        if !matches!(self.conn, Conn::Up(_)) {
            // Disconnected: no frame reads, no LED work; just the probe.
            self.try_connect(now, stop);
            if !stop() {
                self.persist(now);
            }
            return;
        }

        if matches!(self.settings.mode, Mode::Ambient | Mode::AmbientPulse) {
            self.read_frame(now);
        }
        // Without sound ambient+pulse eases to steady ambient instead of the
        // pulse floor ([`PULSE_FLOOR`]): capture runs whenever an audio mode
        // is selected, so silent frames arrive while nothing plays, and a
        // pause inside music must not dip then snap back (R4).
        let (pulse_target, taus) = match (self.settings.mode, self.fresh_bands(now)) {
            (Mode::AmbientPulse, Some(b)) if self.sounding => (
                PULSE_FLOOR + (1.0 - PULSE_FLOOR) * bass_energy(b),
                PULSE_TAUS,
            ),
            _ => (1.0, PULSE_RESTING_TAUS),
        };
        self.pulse = smooth_scalar(self.pulse, pulse_target, dt, taus);

        let placeholder = Conn::Down {
            next_probe: now,
            backoff: self.cfg.backoff_min,
        };
        let Conn::Up(mut client) = std::mem::replace(&mut self.conn, placeholder) else {
            return;
        };
        match self.drive(&mut client, now, dt, stop) {
            Ok(()) => {
                self.conn = Conn::Up(client);
                self.connected_state(now);
            }
            Err(e) => self.connection_lost(now, &mut client, &e),
        }
        // On a stop the restore path writes the file (bounded by its own
        // deadline); the file is already a superset of the devices in the
        // per-LED mode (written before each switch), so skipping this keeps
        // a large encode out of the in-flight tick.
        if !stop() {
            self.persist(now);
        }
    }

    /// Restores every remembered mode whose device is listed (mode `off`,
    /// no source). Failures other than "that mode no longer exists" stay
    /// remembered and are retried; a device missing from the list is
    /// restored when it is listed again (still in the recorded per-LED
    /// mode, see [`Engine::after_refresh`]).
    fn release(
        &mut self,
        client: &mut Client,
        now: Instant,
        stop: &dyn Fn() -> bool,
    ) -> Result<(), Error> {
        let was_driving = std::mem::replace(&mut self.driving, false);
        self.last_sent.clear();
        self.smoothers.clear();
        let mut gave_back = false;
        if client.saved_present_count() > 0 {
            let reports = client.restore_all_until(stop);
            for r in reports.iter() {
                let slot = slot_key(&r.device, r.occurrence);
                match &r.result {
                    Ok(()) => {
                        gave_back = true;
                        self.gone_noted.remove(&slot);
                    }
                    // Missing from the list: kept; logged once, not per tick.
                    Err(Error::DeviceGone) if !self.gone_noted.insert(slot) => {}
                    Err(e) => {
                        let detail = format!(
                            "device={:?} op=restore reason={} kept={}",
                            cut(&r.device.name),
                            error_reason(e),
                            r.retained
                        );
                        self.log.log(now, "update_error", &detail);
                        self.counters.update_errors = self.counters.update_errors.saturating_add(1);
                    }
                }
            }
            if let Some(e) = reports
                .into_iter()
                .filter_map(|r| r.result.err())
                .find(Error::is_connection_lost)
            {
                return Err(e);
            }
        }
        if was_driving || gave_back {
            self.released_at = Some(now);
        }
        // Still controlled = still remembered (a restore that will be
        // retried); everything else is back on the user's lighting.
        let remembered: HashSet<String> = client
            .saved_modes()
            .iter()
            .map(|s| slot_key(&s.key, s.occurrence))
            .collect();
        self.controlled.retain(|slot| remembered.contains(slot));
        Ok(())
    }

    fn drive(
        &mut self,
        client: &mut Client,
        now: Instant,
        dt: f32,
        stop: &dyn Fn() -> bool,
    ) -> Result<(), Error> {
        client.poll_events()?;
        if client.needs_refresh() {
            client.refresh()?;
            // Indexes changed: drop per-device caches keyed by stale data.
            self.last_sent.clear();
            self.after_refresh(now, client);
        }
        let present = self.source_now(now);
        if !present {
            self.live_since = None;
        } else if self.live_since.is_none() {
            self.live_since = Some(now);
        }
        if self.driving {
            if !self.source_live(now) {
                return self.release(client, now, stop);
            }
        } else if !present {
            // Nothing to show: give back anything still remembered.
            return self.release(client, now, stop);
        } else if !self.may_take(now) {
            // A source, but inside the takeover dwell or the minimum
            // retake interval: leave every device exactly as it is.
            return Ok(());
        } else {
            self.retake_bypass = false;
        }
        self.driving = true;

        let devices = device_slots(client);
        let bands: Option<Vec<f32>> = self.fresh_bands(now).map(<[f32]>::to_vec);
        let grid = self.sampler.grid();
        let mode = self.settings.mode;
        let budget_end = Instant::now() + TICK_BUDGET;
        let n = devices.len();
        let start = if self.resume_at < n {
            self.resume_at
        } else {
            0
        };
        self.resume_at = 0;

        for k in 0..n {
            let i = (start + k) % n;
            if stop() || Instant::now() >= budget_end {
                self.resume_at = i;
                return Ok(());
            }
            let (cid, id, slot, leds) = &devices[i];
            let (cid, leds) = (*cid, *leds);
            if self.settings.disabled.contains(id) {
                let was_controlled = self.controlled.remove(slot);
                if was_controlled || client.has_saved_mode(cid) {
                    match client.restore(cid) {
                        Ok(_) => {}
                        Err(e) if e.is_connection_lost() => return Err(e),
                        Err(Error::DeviceListChanged) => return Ok(()),
                        Err(e) => {
                            if !e.drops_saved_mode() {
                                // Still in our mode; retried next tick.
                                self.controlled.insert(slot.clone());
                            }
                            device_error(&mut self.counters, &mut self.log, now, id, "restore", &e)
                        }
                    }
                }
                self.last_sent.remove(slot);
                self.smoothers.remove(slot);
                continue;
            }
            if leds == 0 {
                continue;
            }
            if !self.controlled.contains(slot) {
                // Persist the original before the device is switched.
                if let Some(store) = self.store.as_mut() {
                    match client.mode_to_save(cid) {
                        Ok(Some(entry)) => {
                            let mut all = client.saved_modes().to_vec();
                            all.push(entry);
                            // The file now differs from the last persisted
                            // set whatever happens next.
                            self.persisted = None;
                            if let Err(e) = store.write(&all) {
                                let detail = format!(
                                    "device={id} op=persist kind={:?} detail=not_taken",
                                    e.kind()
                                );
                                self.counters.update_errors =
                                    self.counters.update_errors.saturating_add(1);
                                self.log.log(now, "update_error", &detail);
                                continue;
                            }
                        }
                        Ok(None) => {}
                        Err(e) if e.is_connection_lost() => return Err(e),
                        Err(Error::DeviceListChanged) => return Ok(()),
                        Err(e) => {
                            device_error(
                                &mut self.counters,
                                &mut self.log,
                                now,
                                id,
                                "take_control",
                                &e,
                            );
                            continue;
                        }
                    }
                }
                match client.take_control(cid) {
                    Ok(()) => {
                        self.controlled.insert(slot.clone());
                    }
                    Err(e) if e.is_connection_lost() => return Err(e),
                    Err(Error::DeviceListChanged) => return Ok(()),
                    Err(e) => {
                        device_error(
                            &mut self.counters,
                            &mut self.log,
                            now,
                            id,
                            "take_control",
                            &e,
                        );
                        continue;
                    }
                }
            }
            let Some(controller) = client.controller(cid) else {
                continue;
            };
            let (targets, taus) = match (mode, grid) {
                (Mode::Spectrum, _) => {
                    let b = bands.as_deref().unwrap_or(&[]);
                    (
                        controller_colors(controller, |shape, n| spectrum_zone(b, shape, n)),
                        SPECTRUM_TAUS,
                    )
                }
                (_, Some(g)) => (
                    controller_colors(controller, |shape, n| ambient_zone(g, shape, n)),
                    AMBIENT_TAUS,
                ),
                // source_live() guarantees a grid for the ambient modes.
                (_, None) => continue,
            };
            let smoothed = self
                .smoothers
                .entry(slot.clone())
                .or_default()
                .step(&targets, dt, taus);
            let pulse = if mode == Mode::AmbientPulse {
                self.pulse
            } else {
                1.0
            };
            let out: Vec<Rgb> = smoothed
                .iter()
                .map(|c: &Rgbf| apply_brightness(c.scale(pulse), self.settings.brightness).to_rgb())
                .collect();
            if let Some((t, prev)) = self.last_sent.get(slot)
                && *prev == out
                && now.saturating_duration_since(*t) < KEEPALIVE
            {
                continue;
            }
            match client.update_leds(cid, &out) {
                Ok(()) => {
                    self.counters.updates_sent = self.counters.updates_sent.saturating_add(1);
                    self.last_sent.insert(slot.clone(), (now, out));
                }
                Err(e) if e.is_connection_lost() => return Err(e),
                Err(Error::DeviceListChanged) => return Ok(()),
                Err(e) => device_error(&mut self.counters, &mut self.log, now, id, "update", &e),
            }
        }
        Ok(())
    }

    /// Hands the connection, the remembered modes and the saved-modes file
    /// to the stop path.
    pub fn into_restore_parts(self) -> RestoreParts {
        RestoreParts {
            client: match self.conn {
                Conn::Up(c) => Some(c),
                Conn::Down { .. } => None,
            },
            carried: self.carried,
            config: self.cfg.client,
            store: self.store,
        }
    }

    /// The current status. `seq` is assigned by the caller.
    pub fn status(&self, now: Instant, seq: u64) -> StatusLine {
        let (reachable, protocol, server_protocol, next_probe_ms, devices) = match &self.conn {
            Conn::Up(c) => {
                let slots = device_slots(c);
                (
                    true,
                    Some(c.protocol_version()),
                    Some(c.server_protocol_version()),
                    None,
                    c.controllers()
                        .iter()
                        .zip(slots)
                        .map(|(c, (_, id, slot, _))| {
                            device_info(
                                c,
                                !self.settings.disabled.contains(&id),
                                self.controlled.contains(&slot),
                            )
                        })
                        .collect(),
                )
            }
            Conn::Down { next_probe, .. } => (
                false,
                None,
                None,
                Some(next_probe.saturating_duration_since(now).as_millis() as u64),
                Vec::new(),
            ),
        };
        let devices: Vec<DeviceInfo> = devices;
        let (width, height) = match self.sampler.size() {
            Some((w, h)) => (Some(w), Some(h)),
            None => (None, None),
        };
        let driven_count = match &self.conn {
            Conn::Up(c) if self.driving && self.settings.mode != Mode::Off => device_slots(c)
                .iter()
                .filter(|(_, _, slot, _)| self.controlled.contains(slot))
                .count()
                as u32,
            _ => 0,
        };
        StatusLine {
            version: VERSION,
            message_type: "led_status".into(),
            seq,
            state: self.state,
            reason: self.reason.clone(),
            mode: self.settings.mode,
            brightness: self.settings.brightness,
            rate_hz: self.settings.rate_hz,
            server: ServerInfo {
                port: self.cfg.client.port,
                reachable,
                protocol,
                server_protocol,
                next_probe_ms,
            },
            frame: FrameInfo {
                state: self.frame_state,
                sequence: self.sampler.sequence(),
                width,
                height,
            },
            audio: AudioInfo {
                fresh: self.fresh_bands(now).is_some(),
                bands: self.fresh_bands(now).map(|b| b.len() as u32),
                silent: self.silent(now),
            },
            device_count: devices.len() as u32,
            driven_count,
            devices,
            devices_truncated: false,
            counters: self.counters.clone(),
            restore: None,
        }
    }
}

/// Inventory entry for one controller (status lines and `--probe`).
pub fn device_info(c: &Controller, enabled: bool, controlled: bool) -> DeviceInfo {
    let zones: Vec<ZoneInfo> = c
        .zones
        .iter()
        .take(kwe_led_protocol::MAX_STATUS_ZONES)
        .map(|z| ZoneInfo {
            name: cut(&z.name),
            kind: match z.kind {
                zone_type::SINGLE => ZoneKind::Single,
                zone_type::LINEAR => ZoneKind::Linear,
                zone_type::MATRIX => ZoneKind::Matrix,
                _ => ZoneKind::Unknown,
            },
            leds: z.leds_count,
            matrix: z.matrix.as_ref().map(|m| [m.height, m.width]),
        })
        .collect();
    DeviceInfo {
        id: controller_id(c),
        name: cut(&c.name),
        vendor: cut(&c.vendor),
        serial: cut(&c.serial),
        location: cut(&c.location),
        kind: c.kind,
        leds: c.leds.len() as u32,
        enabled,
        controlled,
        zones_truncated: c.zones.len() > zones.len(),
        zones,
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
