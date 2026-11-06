// SPDX-License-Identifier: GPL-3.0-or-later
//! The worker's single-threaded core: OpenRGB connection state machine,
//! ambient/audio sources, mapping, smoothing, and the rate-bounded sink.
//! The caller (main loop) owns timing: it calls [`Engine::tick`] at most
//! `rate_hz` times per second and feeds control/audio between ticks.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kwe_led_protocol::{
    AudioInfo, ConfigPatch, Counters, DeviceInfo, FrameInfo, FrameState, Mode, ServerInfo, State,
    StatusLine, VERSION, ZoneInfo, ZoneKind, cut, device_id,
};
use kwe_openrgb::{
    Client, ClientConfig, Controller, ControllerId, Error, Rgb, SavedMode, zone_type,
};

use crate::color::{BLACK, Rgbf, Smoother, Taus, apply_brightness, smooth_scalar};
use crate::frame::{FrameSampler, Sample};
use crate::log::LogLimiter;
use crate::mapping::{ambient_zone, bass_energy, controller_colors, mono_bands, spectrum_zone};

/// After the frame source disappears, the last colours are held this long,
/// then faded to black with [`FADE_TAU`].
pub const FRAME_HOLD: Duration = Duration::from_secs(2);
pub const FADE_TAU: f32 = 1.0;
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
/// Brightness floor of `ambient+pulse` at silence.
pub const PULSE_FLOOR: f32 = 0.2;

/// Default probe backoff: 1 s doubling to 60 s.
pub const BACKOFF_MIN: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub client: ClientConfig,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
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
        | Error::InvalidArgument(_) => "device_error",
        _ => "error",
    }
}

fn controller_id(c: &Controller) -> String {
    device_id(&c.name, &c.vendor, &c.serial, &c.location)
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
}

impl RestoreParts {
    /// Best-effort restore of every remembered mode. With no live
    /// connection but remembered modes (server dropped mid-run), one
    /// bounded reconnect is attempted. Bounded by the client's timeouts;
    /// the caller additionally runs this on a thread with a deadline.
    pub fn restore(self) -> RestoreOutcome {
        let mut client = match self.client {
            Some(c) if !c.is_connection_lost() => *c,
            _ => {
                if self.carried.is_empty() {
                    return RestoreOutcome::default();
                }
                match Client::connect(&self.config).and_then(|mut c| {
                    c.refresh()?;
                    Ok(c)
                }) {
                    Ok(c) => c,
                    Err(_) => {
                        return RestoreOutcome {
                            attempted: self.carried.len() as u32,
                            restored: 0,
                            failed: self.carried.len() as u32,
                        };
                    }
                }
            }
        };
        client.adopt_saved_modes(self.carried);
        let reports = client.restore_all();
        let restored = reports.iter().filter(|r| r.result.is_ok()).count() as u32;
        RestoreOutcome {
            attempted: reports.len() as u32,
            restored,
            failed: reports.len() as u32 - restored,
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

pub struct Engine {
    cfg: EngineConfig,
    settings: Settings,
    frame_path: Option<PathBuf>,
    conn: Conn,
    carried: Vec<SavedMode>,
    controlled: HashSet<String>,
    sampler: FrameSampler,
    frame_state: FrameState,
    frame_ok_at: Option<Instant>,
    audio: Option<(Instant, Vec<f32>)>,
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
            sampler: FrameSampler::default(),
            frame_ok_at: None,
            audio: None,
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

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn counters_mut(&mut self) -> &mut Counters {
        &mut self.counters
    }

    /// Rate-limited diagnostic through the engine's shared log budget.
    pub fn log(&mut self, now: Instant, event: &str, detail: &str) {
        self.log.log(now, event, detail);
    }

    pub fn apply_config(&mut self, patch: ConfigPatch) {
        self.settings.apply(patch);
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
        // FRAME_HOLD instead of blinking dark while the new renderer starts.
        self.frame_path = path;
    }

    pub fn frame_path(&self) -> Option<&Path> {
        self.frame_path.as_deref()
    }

    pub fn set_audio(&mut self, now: Instant, left: &[f32], right: &[f32]) {
        self.counters.audio_frames = self.counters.audio_frames.saturating_add(1);
        self.audio = Some((now, mono_bands(left, right)));
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

    fn fresh_bands(&self, now: Instant) -> Option<&[f32]> {
        match &self.audio {
            Some((t, b)) if now.saturating_duration_since(*t) <= AUDIO_FRESH => Some(b),
            _ => None,
        }
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

    fn try_connect(&mut self, now: Instant) {
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
        match Client::connect(&self.cfg.client).and_then(|mut c| {
            c.refresh()?;
            Ok(c)
        }) {
            Ok(mut client) => {
                client.adopt_saved_modes(std::mem::take(&mut self.carried));
                self.counters.connects = self.counters.connects.saturating_add(1);
                self.conn = Conn::Up(Box::new(client));
                let state = if self.settings.mode == Mode::Off {
                    State::Off
                } else {
                    State::Active
                };
                self.set_state(now, state, None);
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

    fn connection_lost(&mut self, now: Instant, client: &mut Client, e: &Error) {
        let saved = client.take_saved_modes();
        let mut carried = std::mem::take(&mut self.carried);
        for s in saved {
            if !carried.iter().any(|c| c.key == s.key)
                && carried.len() < kwe_openrgb::limits::MAX_CONTROLLERS as usize
            {
                carried.push(s);
            }
        }
        self.carried = carried;
        self.controlled.clear();
        self.last_sent.clear();
        self.counters.disconnects = self.counters.disconnects.saturating_add(1);
        // First re-probe soon (a restarting server comes back quickly),
        // then the normal doubling backoff.
        self.conn = Conn::Down {
            next_probe: now + self.cfg.backoff_min,
            backoff: self.cfg.backoff_min,
        };
        self.set_state(now, State::Degraded, Some(error_reason(e)));
    }

    /// One LED tick: sample the sources the mode needs, keep the connection
    /// alive, and send at most one `UpdateLEDs` per enabled device.
    pub fn tick(&mut self, now: Instant) {
        self.counters.ticks = self.counters.ticks.saturating_add(1);
        let interval = self.settings.tick_interval().as_secs_f32();
        let dt = self
            .last_tick
            .map(|t| now.saturating_duration_since(t).as_secs_f32())
            .unwrap_or(interval)
            .min(0.5);
        self.last_tick = Some(now);

        if matches!(self.settings.mode, Mode::Ambient | Mode::AmbientPulse) {
            self.read_frame(now);
        }
        let pulse_target = match (self.settings.mode, self.fresh_bands(now)) {
            (Mode::AmbientPulse, Some(b)) => PULSE_FLOOR + (1.0 - PULSE_FLOOR) * bass_energy(b),
            _ => 1.0,
        };
        self.pulse = smooth_scalar(self.pulse, pulse_target, dt, PULSE_TAUS);

        self.try_connect(now);
        if !matches!(self.conn, Conn::Up(_)) {
            return;
        }
        let placeholder = Conn::Down {
            next_probe: now,
            backoff: self.cfg.backoff_min,
        };
        let Conn::Up(mut client) = std::mem::replace(&mut self.conn, placeholder) else {
            return;
        };
        match self.drive(&mut client, now, dt) {
            Ok(()) => {
                self.conn = Conn::Up(client);
                let state = if self.settings.mode == Mode::Off {
                    State::Off
                } else {
                    State::Active
                };
                self.set_state(now, state, None);
            }
            Err(e) => self.connection_lost(now, &mut client, &e),
        }
    }

    fn drive(&mut self, client: &mut Client, now: Instant, dt: f32) -> Result<(), Error> {
        client.poll_events()?;
        if client.needs_refresh() {
            client.refresh()?;
            // Indexes changed: drop per-device caches keyed by stale data.
            self.last_sent.clear();
        }
        if self.settings.mode == Mode::Off {
            if client.saved_mode_count() > 0 || !self.controlled.is_empty() {
                let reports = client.restore_all();
                for r in reports.iter().filter(|r| r.result.is_err()) {
                    let detail = format!("device={} op=restore", cut(&r.device.name));
                    self.log.log(now, "update_error", &detail);
                    self.counters.update_errors = self.counters.update_errors.saturating_add(1);
                }
                if let Some(e) = reports
                    .into_iter()
                    .filter_map(|r| r.result.err())
                    .find(Error::is_connection_lost)
                {
                    return Err(e);
                }
            }
            self.controlled.clear();
            self.last_sent.clear();
            self.smoothers.clear();
            return Ok(());
        }

        let devices: Vec<(ControllerId, String, usize)> = client
            .controllers()
            .iter()
            .map(|c| (c.id, controller_id(c), c.leds.len()))
            .collect();
        let bands: Option<Vec<f32>> = self.fresh_bands(now).map(<[f32]>::to_vec);
        let holding = self
            .frame_ok_at
            .is_some_and(|t| now.saturating_duration_since(t) <= FRAME_HOLD);
        let grid = if holding { self.sampler.grid() } else { None };
        let mode = self.settings.mode;

        for (cid, id, leds) in devices {
            if self.settings.disabled.contains(&id) {
                let was_controlled = self.controlled.remove(&id);
                let has_saved = client
                    .controller(cid)
                    .map(Controller::key)
                    .is_some_and(|key| client.saved_modes().iter().any(|s| s.key == key));
                if was_controlled || has_saved {
                    match client.restore(cid) {
                        Ok(_) => {}
                        Err(e) if e.is_connection_lost() => return Err(e),
                        Err(Error::DeviceListChanged) => return Ok(()),
                        Err(e) => {
                            device_error(&mut self.counters, &mut self.log, now, &id, "restore", &e)
                        }
                    }
                }
                self.last_sent.remove(&id);
                self.smoothers.remove(&id);
                continue;
            }
            if leds == 0 {
                continue;
            }
            if !self.controlled.contains(&id) {
                match client.take_control(cid) {
                    Ok(()) => {
                        self.controlled.insert(id.clone());
                    }
                    Err(e) if e.is_connection_lost() => return Err(e),
                    Err(Error::DeviceListChanged) => return Ok(()),
                    Err(e) => {
                        device_error(
                            &mut self.counters,
                            &mut self.log,
                            now,
                            &id,
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
            let (targets, taus) = match mode {
                Mode::Spectrum => {
                    let b = bands.as_deref().unwrap_or(&[]);
                    (
                        controller_colors(controller, |shape, n| spectrum_zone(b, shape, n)),
                        SPECTRUM_TAUS,
                    )
                }
                _ => match grid {
                    Some(g) => (
                        controller_colors(controller, |shape, n| ambient_zone(g, shape, n)),
                        AMBIENT_TAUS,
                    ),
                    None => (
                        vec![BLACK; controller.leds.len()],
                        Taus::symmetric(FADE_TAU),
                    ),
                },
            };
            let smoothed = self
                .smoothers
                .entry(id.clone())
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
            if let Some((t, prev)) = self.last_sent.get(&id)
                && *prev == out
                && now.saturating_duration_since(*t) < KEEPALIVE
            {
                continue;
            }
            match client.update_leds(cid, &out) {
                Ok(()) => {
                    self.counters.updates_sent = self.counters.updates_sent.saturating_add(1);
                    self.last_sent.insert(id, (now, out));
                }
                Err(e) if e.is_connection_lost() => return Err(e),
                Err(Error::DeviceListChanged) => return Ok(()),
                Err(e) => device_error(&mut self.counters, &mut self.log, now, &id, "update", &e),
            }
        }
        Ok(())
    }

    /// Hands the connection and remembered modes to the stop path.
    pub fn into_restore_parts(self) -> RestoreParts {
        RestoreParts {
            client: match self.conn {
                Conn::Up(c) => Some(c),
                Conn::Down { .. } => None,
            },
            carried: self.carried,
            config: self.cfg.client,
        }
    }

    /// The current status. `seq` is assigned by the caller.
    pub fn status(&self, now: Instant, seq: u64) -> StatusLine {
        let (reachable, protocol, server_protocol, next_probe_ms, devices) = match &self.conn {
            Conn::Up(c) => (
                true,
                Some(c.protocol_version()),
                Some(c.server_protocol_version()),
                None,
                c.controllers()
                    .iter()
                    .map(|c| {
                        let id = controller_id(c);
                        device_info(
                            c,
                            !self.settings.disabled.contains(&id),
                            self.controlled.contains(&id),
                        )
                    })
                    .collect(),
            ),
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
            },
            device_count: devices.len() as u32,
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
