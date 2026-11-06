// SPDX-License-Identifier: GPL-3.0-or-later
//! Synchronous loopback-only SDK client.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use crate::model::{Controller, ControllerId, DeviceKey, Mode, Rgb, parse_controller};
use crate::wire::{self, Packet, Reader, Writer, packet_id};
use crate::{Error, PROTOCOL_MAX, PROTOCOL_MIN, Result, limits};

/// OpenRGB's default SDK port.
pub const DEFAULT_PORT: u16 = 6742;

/// Connection settings. There is deliberately no address field: the client
/// only ever dials `127.0.0.1`, because the SDK protocol is unauthenticated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientConfig {
    pub port: u16,
    /// Shown in OpenRGB's client list. ≤64 bytes, no NUL.
    pub client_name: String,
    pub connect_timeout: Duration,
    /// Deadline for one request/reply exchange and for one write.
    pub io_timeout: Duration,
    /// Deadline for a whole [`Client::refresh`].
    pub enumerate_budget: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            client_name: "KDE Wallpaper Engine".to_string(),
            connect_timeout: Duration::from_millis(250),
            io_timeout: Duration::from_millis(1000),
            enumerate_budget: Duration::from_secs(5),
        }
    }
}

impl ClientConfig {
    pub fn with_port(port: u16) -> Self {
        Self {
            port,
            ..Self::default()
        }
    }

    fn validate(&self) -> Result<()> {
        if self.port == 0 {
            return Err(Error::InvalidArgument("port must be non-zero"));
        }
        if self.client_name.len() > limits::MAX_CLIENT_NAME_BYTES || self.client_name.contains('\0')
        {
            return Err(Error::InvalidArgument(
                "client name must be at most 64 bytes without NUL",
            ));
        }
        let max = Duration::from_millis(limits::MAX_TIMEOUT_MS);
        for t in [self.connect_timeout, self.io_timeout, self.enumerate_budget] {
            if t.is_zero() || t > max {
                return Err(Error::InvalidArgument("timeouts must be in 1 ms..=30 s"));
            }
        }
        Ok(())
    }
}

/// What [`Client::poll_events`] drained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PollOutcome {
    pub packets: usize,
    /// True when the device list changed and [`Client::refresh`] is needed.
    pub device_list_updated: bool,
}

/// Result of restoring one device's saved mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    pub device: DeviceKey,
    /// See [`SavedMode::occurrence`].
    pub occurrence: u32,
    pub result: Result<()>,
    /// The remembered mode is still held (the restore failed for a reason
    /// other than [`Error::drops_saved_mode`]) and will be tried again.
    pub retained: bool,
}

/// The per-LED mode `SetCustomMode` put a device in, as re-read right
/// after the switch. Lets a later process tell "still in our mode" (restore
/// the original) from "the user picked something else since" (forget it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlMode {
    pub index: usize,
    pub name: String,
}

/// A device's mode as it was before [`Client::take_control`] switched it
/// to per-LED control. Exportable so a caller that loses its connection (or
/// its process) can carry the originals into the next one
/// ([`Client::adopt_saved_modes`]) instead of re-saving the per-LED mode it
/// left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedMode {
    pub key: DeviceKey,
    /// Disambiguates controllers whose [`DeviceKey`] is identical: 0 for the
    /// first such controller in the server's list order, 1 for the second...
    pub occurrence: u32,
    pub index: usize,
    pub mode: Mode,
    /// The LED colours at save time, kept only when the saved mode is
    /// [`Mode::is_direct`] (that mode shows the last colours sent, so they
    /// are part of the user's lighting); empty otherwise, and empty when
    /// every colour is black (review fixes C1: a freshly started OpenRGB
    /// with no profile reports Direct with an all-zero buffer while the
    /// hardware shows its own effect — re-sending zeros would blank the
    /// device). Restore re-sends non-empty colours when the LED count still
    /// matches.
    pub colors: Vec<Rgb>,
    /// Filled in once the switch to per-LED control was observed.
    pub control: Option<ControlMode>,
}

impl SavedMode {
    fn same_slot(&self, key: &DeviceKey, occurrence: u32) -> bool {
        self.occurrence == occurrence && self.key == *key
    }
}

pub struct Client {
    stream: TcpStream,
    config: ClientConfig,
    server_version: u32,
    version: u32,
    controllers: Vec<Controller>,
    list_dirty: bool,
    poisoned: bool,
    saved: Vec<SavedMode>,
    /// +1 on every change of `saved` ([`Client::saved_revision`]).
    saved_revision: u64,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("port", &self.config.port)
            .field("version", &self.version)
            .field("controllers", &self.controllers.len())
            .field("list_dirty", &self.list_dirty)
            .field("poisoned", &self.poisoned)
            .finish()
    }
}

impl Client {
    /// Connects to `127.0.0.1:<port>`, negotiates the protocol version and
    /// sends the client name. Does not enumerate; call [`Client::refresh`].
    ///
    /// Connection refused → [`Error::ServerAbsent`] (cheap, expected).
    pub fn connect(config: &ClientConfig) -> Result<Self> {
        config.validate()?;
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, config.port));
        let stream = match TcpStream::connect_timeout(&addr, config.connect_timeout) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                return Err(Error::ServerAbsent { port: config.port });
            }
            Err(e) => return Err(Error::from_io("connect", &e)),
        };
        let _ = stream.set_nodelay(true);
        let mut client = Self {
            stream,
            config: config.clone(),
            server_version: 0,
            version: 0,
            controllers: Vec::new(),
            list_dirty: true,
            poisoned: false,
            saved: Vec::new(),
            saved_revision: 0,
        };
        client.negotiate()?;
        let mut name = client.config.client_name.clone().into_bytes();
        name.push(0);
        client.send(0, packet_id::SET_CLIENT_NAME, &name)?;
        Ok(client)
    }

    fn negotiate(&mut self) -> Result<()> {
        let deadline = Instant::now() + self.config.io_timeout;
        self.send(
            0,
            packet_id::REQUEST_PROTOCOL_VERSION,
            &PROTOCOL_MAX.to_le_bytes(),
        )?;
        // A protocol-0 server never answers this request; the published
        // rule is to assume 0 after silence, which this client refuses.
        let reply = match self.read_reply(packet_id::REQUEST_PROTOCOL_VERSION, deadline) {
            Err(Error::Timeout { .. }) => {
                return Err(Error::UnsupportedVersion {
                    server: 0,
                    min: PROTOCOL_MIN,
                    max: PROTOCOL_MAX,
                });
            }
            other => other?,
        };
        let server = Reader::new(&reply.payload).u32("protocol version reply")?;
        let negotiated = server.min(PROTOCOL_MAX);
        if negotiated < PROTOCOL_MIN {
            return Err(Error::UnsupportedVersion {
                server,
                min: PROTOCOL_MIN,
                max: PROTOCOL_MAX,
            });
        }
        self.server_version = server;
        self.version = negotiated;
        Ok(())
    }

    pub fn port(&self) -> u16 {
        self.config.port
    }

    /// Negotiated version: `min(server, PROTOCOL_MAX)`.
    pub fn protocol_version(&self) -> u32 {
        self.version
    }

    /// The server's own highest version, as it reported it.
    pub fn server_protocol_version(&self) -> u32 {
        self.server_version
    }

    /// Controllers from the last successful [`Client::refresh`]; empty
    /// after a failed one.
    pub fn controllers(&self) -> &[Controller] {
        &self.controllers
    }

    pub fn controller(&self, id: ControllerId) -> Option<&Controller> {
        self.controllers
            .get(id.index() as usize)
            .filter(|c| c.id == id)
    }

    /// True before the first refresh, after a device-list-updated
    /// notification, and after a failed refresh.
    pub fn needs_refresh(&self) -> bool {
        self.list_dirty
    }

    /// True once a transport error made this connection unusable.
    pub fn is_connection_lost(&self) -> bool {
        self.poisoned
    }

    /// Re-enumerates every controller within `enumerate_budget`.
    pub fn refresh(&mut self) -> Result<&[Controller]> {
        self.ensure_live()?;
        self.controllers.clear();
        self.list_dirty = false;
        match self.enumerate() {
            Ok(list) => {
                self.controllers = list;
                Ok(&self.controllers)
            }
            Err(e) => {
                self.list_dirty = true;
                Err(e)
            }
        }
    }

    fn enumerate(&mut self) -> Result<Vec<Controller>> {
        let budget_end = Instant::now() + self.config.enumerate_budget;
        let reply = self.transact(0, packet_id::REQUEST_CONTROLLER_COUNT, &[], budget_end)?;
        let count = Reader::new(&reply.payload).u32("controller count")?;
        if count > limits::MAX_CONTROLLERS {
            return Err(Error::LimitExceeded {
                what: "controller count",
                value: u64::from(count),
                max: u64::from(limits::MAX_CONTROLLERS),
            });
        }
        let mut list = Vec::with_capacity(count as usize);
        let mut retained = 0usize;
        for index in 0..count {
            let controller = self.fetch_controller(ControllerId::new(index), budget_end)?;
            retained = retained.saturating_add(controller.approx_heap_bytes());
            if retained > limits::MAX_ENUMERATION_BYTES {
                return Err(Error::LimitExceeded {
                    what: "enumeration bytes",
                    value: retained as u64,
                    max: limits::MAX_ENUMERATION_BYTES as u64,
                });
            }
            list.push(controller);
        }
        Ok(list)
    }

    fn fetch_controller(&mut self, id: ControllerId, budget_end: Instant) -> Result<Controller> {
        let version = self.version;
        let reply = self.transact(
            id.index(),
            packet_id::REQUEST_CONTROLLER_DATA,
            &version.to_le_bytes(),
            budget_end,
        )?;
        parse_controller(&reply.payload, version, id)
    }

    /// Drains packets the server sent on its own (without blocking when
    /// none are waiting), at most `MAX_UNSOLICITED` per call. Call this
    /// regularly when idle so notifications and acks never back up.
    pub fn poll_events(&mut self) -> Result<PollOutcome> {
        self.ensure_live()?;
        let mut out = PollOutcome::default();
        for _ in 0..limits::MAX_UNSOLICITED {
            if !self.inbound_pending()? {
                break;
            }
            let deadline = Instant::now() + self.config.io_timeout;
            let packet = self.recv(deadline)?;
            out.packets += 1;
            self.note_unsolicited(&packet);
        }
        out.device_list_updated = self.list_dirty;
        Ok(out)
    }

    fn inbound_pending(&mut self) -> Result<bool> {
        let fail = |this: &mut Self, e: &io::Error| {
            this.poisoned = true;
            Error::from_io("poll", e)
        };
        if let Err(e) = self.stream.set_nonblocking(true) {
            return Err(fail(self, &e));
        }
        let mut byte = [0u8; 1];
        let peeked = self.stream.peek(&mut byte);
        if let Err(e) = self.stream.set_nonblocking(false) {
            return Err(fail(self, &e));
        }
        match peeked {
            Ok(0) => {
                self.poisoned = true;
                Err(Error::Disconnected)
            }
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(false),
            Err(e) => Err(fail(self, &e)),
        }
    }

    /// The device's [`DeviceKey`] and its occurrence among controllers
    /// with an identical key (list order). `None` for an unknown id.
    pub fn slot(&self, id: ControllerId) -> Option<(DeviceKey, u32)> {
        let controller = self.controller(id)?;
        let key = controller.key();
        let occurrence = self.controllers[..id.index() as usize]
            .iter()
            .filter(|c| c.key() == key)
            .count() as u32;
        Some((key, occurrence))
    }

    fn find_slot(&self, key: &DeviceKey, occurrence: u32) -> Option<ControllerId> {
        self.controllers
            .iter()
            .filter(|c| c.key() == *key)
            .nth(occurrence as usize)
            .map(|c| c.id)
    }

    /// True when a pre-control mode is remembered for this device.
    pub fn has_saved_mode(&self, id: ControllerId) -> bool {
        self.slot(id).is_some_and(|(key, occurrence)| {
            self.saved.iter().any(|s| s.same_slot(&key, occurrence))
        })
    }

    /// What [`Client::take_control`] would remember for this device:
    /// `Ok(None)` when a mode is already remembered (nothing new to save).
    /// Lets a caller persist the entry *before* the device is switched.
    /// Errors exactly where `take_control` would refuse: the device reports
    /// no valid active mode, or `MAX_CONTROLLERS` modes are remembered.
    pub fn mode_to_save(&mut self, id: ControllerId) -> Result<Option<SavedMode>> {
        self.sync()?;
        let (key, occurrence) = self.slot(id).ok_or(Error::UnknownController(id.index()))?;
        if self.saved.iter().any(|s| s.same_slot(&key, occurrence)) {
            return Ok(None);
        }
        if self.saved.len() >= limits::MAX_CONTROLLERS as usize {
            return Err(Error::LimitExceeded {
                what: "remembered modes",
                value: self.saved.len() as u64 + 1,
                max: u64::from(limits::MAX_CONTROLLERS),
            });
        }
        let controller = self
            .controller(id)
            .ok_or(Error::UnknownController(id.index()))?;
        let (index, mode) = controller.active_mode().ok_or(Error::InvalidArgument(
            "device reports no valid active mode to remember",
        ))?;
        Ok(Some(SavedMode {
            key,
            occurrence,
            index,
            mode: mode.clone(),
            colors: if mode.is_direct() && !all_black(&controller.colors) {
                controller.colors.clone()
            } else {
                Vec::new()
            },
            control: None,
        }))
    }

    /// Switches the device to its per-LED (direct/custom) mode, first
    /// remembering its current mode for [`Client::restore`] (once per
    /// device until restored), then re-reads the controller. Refuses (and
    /// sends nothing) when the current mode cannot be remembered — a device
    /// is never taken over without a way to give it back.
    pub fn take_control(&mut self, id: ControllerId) -> Result<()> {
        if let Some(entry) = self.mode_to_save(id)? {
            self.saved.push(entry);
            self.saved_revision += 1;
        }
        let (key, occurrence) = self.slot(id).ok_or(Error::UnknownController(id.index()))?;
        self.send(id.index(), packet_id::SET_CUSTOM_MODE, &[])?;
        let deadline = Instant::now() + self.config.io_timeout;
        let updated = self.fetch_controller(id, deadline)?;
        if updated.key() != key {
            self.list_dirty = true;
            return Err(Error::DeviceListChanged);
        }
        let control = updated.active_mode().map(|(index, m)| ControlMode {
            index,
            name: m.name.clone(),
        });
        if let Some(entry) = self
            .saved
            .iter_mut()
            .find(|s| s.same_slot(&key, occurrence))
            && entry.control.is_none()
        {
            entry.control = control;
            self.saved_revision += 1;
        }
        self.controllers[id.index() as usize] = updated;
        Ok(())
    }

    /// Sets every LED of a controller. `colors.len()` must equal the
    /// controller's LED count. Fire-and-forget: the server does not reply.
    pub fn update_leds(&mut self, id: ControllerId, colors: &[Rgb]) -> Result<()> {
        self.sync()?;
        let controller = self
            .controller(id)
            .ok_or(Error::UnknownController(id.index()))?;
        if colors.len() != controller.leds.len() {
            return Err(Error::InvalidArgument(
                "colour count must equal the controller's LED count",
            ));
        }
        let mut w = Writer::default();
        w.u16(colors.len() as u16);
        for c in colors {
            c.write(&mut w);
        }
        self.send(id.index(), packet_id::UPDATE_LEDS, &w.finish_sized())
    }

    /// Sets every LED of one zone. `colors.len()` must equal the zone's LED
    /// count.
    pub fn update_zone_leds(
        &mut self,
        id: ControllerId,
        zone: usize,
        colors: &[Rgb],
    ) -> Result<()> {
        self.sync()?;
        let controller = self
            .controller(id)
            .ok_or(Error::UnknownController(id.index()))?;
        let z = controller
            .zones
            .get(zone)
            .ok_or(Error::InvalidArgument("zone index out of range"))?;
        if colors.len() != z.leds_count as usize || colors.len() > usize::from(limits::MAX_LEDS) {
            return Err(Error::InvalidArgument(
                "colour count must equal the zone's LED count",
            ));
        }
        let mut w = Writer::default();
        w.u32(zone as u32);
        w.u16(colors.len() as u16);
        for c in colors {
            c.write(&mut w);
        }
        self.send(id.index(), packet_id::UPDATE_ZONE_LEDS, &w.finish_sized())
    }

    /// Applies `mode` at `mode_index` on the device (fire-and-forget).
    pub fn update_mode(&mut self, id: ControllerId, mode_index: usize, mode: &Mode) -> Result<()> {
        self.sync()?;
        let controller = self
            .controller(id)
            .ok_or(Error::UnknownController(id.index()))?;
        if mode_index >= controller.modes.len() {
            return Err(Error::InvalidArgument("mode index out of range"));
        }
        let mut w = Writer::default();
        w.i32(mode_index as i32);
        mode.write(&mut w, self.version);
        self.send(id.index(), packet_id::UPDATE_MODE, &w.finish_sized())?;
        if let Some(c) = self.controllers.get_mut(id.index() as usize) {
            c.active_mode = mode_index as i32;
            c.modes[mode_index] = mode.clone();
        }
        Ok(())
    }

    /// Number of devices with a remembered previous mode.
    pub fn saved_mode_count(&self) -> usize {
        self.saved.len()
    }

    /// The remembered pre-control modes, in the order they were saved.
    pub fn saved_modes(&self) -> &[SavedMode] {
        &self.saved
    }

    /// Changes whenever the remembered set changes (adds, removals, a
    /// recorded control mode), so a caller persisting it can skip the
    /// encoding when nothing changed. Starts at 0 per connection.
    pub fn saved_revision(&self) -> u64 {
        self.saved_revision
    }

    /// Remembered modes whose device is in the current device list (the
    /// ones a restore could apply right now).
    pub fn saved_present_count(&self) -> usize {
        self.saved
            .iter()
            .filter(|s| self.find_slot(&s.key, s.occurrence).is_some())
            .count()
    }

    /// Removes and returns every remembered mode without restoring anything
    /// (no I/O; works on a lost connection).
    pub fn take_saved_modes(&mut self) -> Vec<SavedMode> {
        if !self.saved.is_empty() {
            self.saved_revision += 1;
        }
        std::mem::take(&mut self.saved)
    }

    /// Seeds remembered modes, typically taken from a previous connection
    /// or process. An entry for a device that already has one is ignored
    /// (the first saved mode wins, like [`Client::take_control`]); the set
    /// stays capped at `MAX_CONTROLLERS`. No I/O.
    pub fn adopt_saved_modes(&mut self, modes: Vec<SavedMode>) {
        for m in modes {
            if self.saved.len() >= limits::MAX_CONTROLLERS as usize {
                break;
            }
            if !self.saved.iter().any(|s| s.same_slot(&m.key, m.occurrence)) {
                self.saved.push(m);
                self.saved_revision += 1;
            }
        }
    }

    /// Forgets remembered modes the user has evidently replaced: the device
    /// is present but no longer in the per-LED mode this client (or an
    /// earlier process) switched it to ([`SavedMode::control`]). Restoring
    /// those would overwrite lighting chosen after the switch. Entries for
    /// absent devices or without a recorded control mode are kept. Uses the
    /// current device list; no I/O. Returns the forgotten entries.
    ///
    /// Limit: a device still in the recorded control mode is kept, but
    /// OpenRGB profiles and effects also run in that mode (`Direct`), so
    /// "still frozen on our colours" cannot be told from "the user applied
    /// their own Direct lighting since". Callers adopting entries from an
    /// earlier process must decide first whether they are trustworthy
    /// (the LED worker checks boot id and liveness, `LED_PROTOCOL_V1.md`).
    pub fn drop_stale_saved_modes(&mut self) -> Vec<SavedMode> {
        if self.list_dirty {
            return Vec::new();
        }
        let mut stale = Vec::new();
        let mut kept = Vec::with_capacity(self.saved.len());
        for entry in std::mem::take(&mut self.saved) {
            let current = self
                .find_slot(&entry.key, entry.occurrence)
                .and_then(|id| self.controller(id))
                .and_then(|c| c.active_mode().map(|(i, m)| (i, m.name.clone())));
            let is_stale = match (&entry.control, current) {
                (Some(control), Some((index, name))) => {
                    control.index != index || control.name != name
                }
                _ => false,
            };
            if is_stale {
                stale.push(entry);
            } else {
                kept.push(entry);
            }
        }
        if !stale.is_empty() {
            self.saved_revision += 1;
        }
        self.saved = kept;
        stale
    }

    /// Restores this device's remembered mode. `Ok(false)` when nothing
    /// was saved for it. The entry is forgotten only after a successful
    /// restore or when it can never apply again
    /// ([`Error::drops_saved_mode`]: the mode is gone); any other error —
    /// including [`Error::DeviceGone`] — keeps it.
    pub fn restore(&mut self, id: ControllerId) -> Result<bool> {
        self.sync()?;
        let (key, occurrence) = self.slot(id).ok_or(Error::UnknownController(id.index()))?;
        let Some(saved) = self
            .saved
            .iter()
            .find(|s| s.same_slot(&key, occurrence))
            .cloned()
        else {
            return Ok(false);
        };
        let result = self.restore_with_retry(&saved);
        if result.is_ok() || result.as_ref().is_err_and(Error::drops_saved_mode) {
            self.saved.retain(|s| !s.same_slot(&key, occurrence));
            self.saved_revision += 1;
        }
        result.map(|()| true)
    }

    /// Best-effort restore of every remembered mode (re-enumerating first
    /// when the device list changed). Devices are matched by
    /// [`DeviceKey`] + occurrence, not index. Restored entries and entries
    /// whose mode no longer exists are forgotten; every other failure —
    /// including a device missing from the list ([`Error::DeviceGone`]) —
    /// keeps its entry ([`RestoreReport::retained`]) for a later attempt.
    pub fn restore_all(&mut self) -> Vec<RestoreReport> {
        self.restore_all_until(&|| false)
    }

    /// [`Client::restore_all`] that stops starting new restores once `stop`
    /// returns true; entries not attempted stay remembered and unreported.
    pub fn restore_all_until(&mut self, stop: &dyn Fn() -> bool) -> Vec<RestoreReport> {
        if self.saved.is_empty() || stop() {
            return Vec::new();
        }
        if !self.poisoned {
            let _ = self.poll_events();
        }
        if self.list_dirty && !self.poisoned {
            let _ = self.refresh();
        }
        let entries = self.saved.clone();
        let mut reports = Vec::with_capacity(entries.len());
        for entry in entries {
            if stop() {
                break;
            }
            let result = if self.poisoned {
                Err(Error::Disconnected)
            } else {
                self.restore_with_retry(&entry)
            };
            let forget = result.is_ok() || result.as_ref().is_err_and(Error::drops_saved_mode);
            if forget {
                self.saved
                    .retain(|s| !s.same_slot(&entry.key, entry.occurrence));
                self.saved_revision += 1;
            }
            reports.push(RestoreReport {
                device: entry.key,
                occurrence: entry.occurrence,
                result,
                retained: !forget,
            });
        }
        reports
    }

    /// One restore; a device-list change in between is answered with one
    /// re-enumeration and one retry.
    fn restore_with_retry(&mut self, saved: &SavedMode) -> Result<()> {
        match self.restore_saved(saved) {
            Err(Error::DeviceListChanged) => {
                self.refresh()?;
                self.restore_saved(saved)
            }
            other => other,
        }
    }

    fn restore_saved(&mut self, saved: &SavedMode) -> Result<()> {
        self.sync()?;
        let id = self
            .find_slot(&saved.key, saved.occurrence)
            .ok_or(Error::DeviceGone)?;
        let controller = self
            .controller(id)
            .ok_or(Error::UnknownController(id.index()))?;
        match controller.modes.get(saved.index) {
            Some(m) if m.name == saved.mode.name => {}
            _ => return Err(Error::ModeGone),
        }
        // An all-black Direct buffer is never re-sent (entries from older
        // files may still carry one): it would blank the device.
        let colors_fit = !saved.colors.is_empty()
            && !all_black(&saved.colors)
            && saved.colors.len() == controller.leds.len();
        self.update_mode(id, saved.index, &saved.mode)?;
        if colors_fit {
            self.update_leds(id, &saved.colors)?;
        }
        Ok(())
    }

    fn ensure_live(&self) -> Result<()> {
        if self.poisoned {
            Err(Error::Disconnected)
        } else {
            Ok(())
        }
    }

    /// Drains pending server packets (so a queued device-list update is
    /// seen before any index is used), then requires a fresh list.
    fn sync(&mut self) -> Result<()> {
        self.poll_events()?;
        if self.list_dirty {
            return Err(Error::DeviceListChanged);
        }
        Ok(())
    }

    fn note_unsolicited(&mut self, packet: &Packet) {
        if packet.id == packet_id::DEVICE_LIST_UPDATED {
            self.list_dirty = true;
        }
        // Everything else (acks, server name/flags, update signals of newer
        // servers) is irrelevant to this client and dropped.
    }

    fn send(&mut self, dev_id: u32, id: u32, payload: &[u8]) -> Result<()> {
        self.ensure_live()?;
        let r = wire::write_packet(
            &mut self.stream,
            dev_id,
            id,
            payload,
            self.config.io_timeout,
        );
        if r.is_err() {
            self.poisoned = true;
        }
        r
    }

    fn recv(&mut self, deadline: Instant) -> Result<Packet> {
        self.ensure_live()?;
        let r = wire::read_packet(&mut self.stream, deadline);
        if r.is_err() {
            self.poisoned = true;
        }
        r
    }

    fn read_reply(&mut self, expect: u32, deadline: Instant) -> Result<Packet> {
        for _ in 0..=limits::MAX_SKIPPED_PER_REPLY {
            let packet = self.recv(deadline)?;
            if packet.id == expect {
                return Ok(packet);
            }
            self.note_unsolicited(&packet);
        }
        self.poisoned = true;
        Err(Error::TooManyUnsolicited {
            max: limits::MAX_SKIPPED_PER_REPLY,
        })
    }

    fn transact(&mut self, dev_id: u32, id: u32, payload: &[u8], limit: Instant) -> Result<Packet> {
        let now = Instant::now();
        if now >= limit {
            return Err(Error::Timeout { op: "enumerate" });
        }
        let deadline = (now + self.config.io_timeout).min(limit);
        self.send(dev_id, id, payload)?;
        self.read_reply(id, deadline)
    }
}

/// True when every colour is black (also for an empty slice).
fn all_black(colors: &[Rgb]) -> bool {
    colors.iter().all(|c| c.r == 0 && c.g == 0 && c.b == 0)
}
