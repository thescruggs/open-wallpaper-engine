// SPDX-License-Identifier: GPL-3.0-or-later
//! OpenRGB LED sync, slice L3: the daemon side of `kwe-led-worker`
//! (contract: `docs/LED_PROTOCOL_V1.md`; plan: `docs/backlog/OPENRGB_LED.md`).
//!
//! Owned by the supervisor runtime and driven synchronously from its tick
//! (like `occlusion.rs`), so the worker always sees the supervisor's own view
//! of the active frame file and the pause verdict. Nothing here may block
//! that thread:
//!
//! - every stdin write is a nonblocking, atomic (≤ `PIPE_BUF`) line write;
//!   a full pipe leaves exactly one pending line per stream (config patches
//!   merge, the frame path and audio are replaced latest-wins), so a worker
//!   that stops reading costs one syscall per stream and tick, never a wait;
//! - stdout is read nonblocking with a per-tick byte budget; only the latest
//!   complete `led_status` line (≤ 32 KiB) is kept, longer lines are dropped;
//! - stopping is asynchronous: close stdin + SIGTERM to the worker's process
//!   group, then later ticks reap it or SIGKILL after `STOP_GRACE` (≥ 4 s,
//!   so a stalled OpenRGB server still gets its devices restored). Only the
//!   daemon's own shutdown waits for that grace in place.
//!
//! The worker runs whenever `led.enabled` is on (default on) — whether or
//! not a renderer is live — so the device inventory and the "SDK server not
//! running" state are always available to the UI; with no renderer it gets
//! `led_frame null` and holds-then-fades by its own contract. A missing
//! worker binary is a quiet `worker_unavailable` state; an unreachable SDK
//! server is the worker's quiet `server_absent` state; neither is an error.

use std::{
    collections::VecDeque,
    io,
    os::unix::{
        io::AsRawFd,
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use kwe_led_protocol::{
    self as proto, ConfigPatch, MAX_STATUS_LINE_BYTES, Mode, RestoreSummary, State, StatusLine,
};
use serde::Serialize;

use crate::settings::LedSettings;
use crate::supervisor::{PipeWrite, set_nonblocking, try_write_input};

/// SIGTERM → SIGKILL grace. The worker's documented worst case is 3.35 s
/// (one in-flight bounded OpenRGB operation ≤ 1.5 s — stop is checked
/// between connect phases and between devices — + restore 1.5 s + flush
/// 0.3 s); `LED_PROTOCOL_V1.md` requires ≥ 4 s.
pub const STOP_GRACE: Duration = Duration::from_secs(4);
/// Restart budget, same shape as the audio/occlusion workers: at most
/// `MAX_RESTARTS` restarts within `RESTART_WINDOW`, so the
/// `MAX_RESTARTS + 1`-th unexpected exit inside the window exhausts it.
const MAX_RESTARTS: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(600);
const RESTART_DELAY: Duration = Duration::from_secs(1);
/// The worker heartbeats every 5 s; no line for this long while it lives is
/// reported as `degraded` / `worker_unresponsive`.
const STATUS_STALE: Duration = Duration::from_secs(15);
/// ... and for this long it is treated as wedged: stopped and restarted
/// (counts toward the restart budget).
const WORKER_WEDGED: Duration = Duration::from_secs(30);
/// `led.reprobe` ("Check again") rate limit.
const REPROBE_MIN_INTERVAL: Duration = Duration::from_secs(2);
/// After SIGKILL, how long a stopping worker may stay unreaped (e.g. stuck
/// in uninterruptible sleep) before the handle is dropped with one log line
/// — so neither a supervisor tick nor daemon shutdown can wait forever.
const KILL_REAP_LIMIT: Duration = Duration::from_secs(2);
/// stdout bytes read per tick (the pipe's capacity).
const STDOUT_READ_BUDGET: usize = 64 * 1024;

/// How to launch the worker. `None` at the manager level (`--no-led-worker`,
/// or a daemon that cannot resolve its own directory) = no worker at all.
#[derive(Debug, Clone)]
pub struct LedWorkerConfig {
    pub worker_path: PathBuf,
    /// OpenRGB SDK port on 127.0.0.1 (`--port`); `None` = the worker's
    /// default 6742. Tests and smokes point it at a stub server.
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Copy)]
struct Timings {
    stop_grace: Duration,
    restart_delay: Duration,
    status_stale: Duration,
    wedged: Duration,
    reprobe_interval: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            stop_grace: STOP_GRACE,
            restart_delay: RESTART_DELAY,
            status_stale: STATUS_STALE,
            wedged: WORKER_WEDGED,
            reprobe_interval: REPROBE_MIN_INTERVAL,
        }
    }
}

/// Bounded nonblocking reader of the worker's stdout: keeps only the latest
/// complete, decodable `led_status` line.
#[derive(Default)]
struct StatusReader {
    partial: Vec<u8>,
    /// Inside an over-long line: skip to its newline without buffering.
    discarding: bool,
    eof: bool,
    latest: Option<StatusLine>,
    latest_at: Option<Instant>,
    rejected: u64,
}

impl StatusReader {
    fn drain(&mut self, stdout: &ChildStdout) {
        if self.eof {
            return;
        }
        let mut newest: Option<Vec<u8>> = None;
        let mut total = 0_usize;
        let mut chunk = [0_u8; 4096];
        while total < STDOUT_READ_BUDGET {
            // SAFETY: reads into a stack buffer of the stated length from a
            // descriptor this struct's owner keeps open.
            let read = unsafe {
                libc::read(
                    stdout.as_raw_fd(),
                    chunk.as_mut_ptr().cast::<libc::c_void>(),
                    chunk.len(),
                )
            };
            if read == 0 {
                self.eof = true;
                break;
            }
            if read < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            let read = read as usize;
            total += read;
            let mut rest = &chunk[..read];
            while !rest.is_empty() {
                match rest.iter().position(|byte| *byte == b'\n') {
                    Some(newline) => {
                        let (head, tail) = rest.split_at(newline);
                        rest = &tail[1..];
                        if self.discarding {
                            self.discarding = false;
                            continue;
                        }
                        if self.partial.len() + head.len() + 1 > MAX_STATUS_LINE_BYTES {
                            self.partial.clear();
                            self.rejected = self.rejected.saturating_add(1);
                            continue;
                        }
                        self.partial.extend_from_slice(head);
                        newest = Some(std::mem::take(&mut self.partial));
                    }
                    None => {
                        if !self.discarding {
                            if self.partial.len() + rest.len() >= MAX_STATUS_LINE_BYTES {
                                self.partial.clear();
                                self.discarding = true;
                                self.rejected = self.rejected.saturating_add(1);
                            } else {
                                self.partial.extend_from_slice(rest);
                            }
                        }
                        rest = &[];
                    }
                }
            }
        }
        if let Some(line) = newest {
            match proto::decode_status(&line) {
                Ok(status) => {
                    self.latest = Some(status);
                    self.latest_at = Some(Instant::now());
                }
                Err(_) => self.rejected = self.rejected.saturating_add(1),
            }
        }
    }
}

struct Running {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    reader: StatusReader,
    spawned_at: Instant,
    /// One pending line per stream (latest wins; config patches merge).
    pending_config: Option<ConfigPatch>,
    pending_frame: Option<Vec<u8>>,
    pending_reprobe: bool,
    pending_status_request: bool,
    pending_audio: Option<Vec<u8>>,
    /// The pipe reported a hard error (EPIPE: the worker closed stdin or
    /// died); nothing more is written, the reaper notices the exit.
    stdin_broken: bool,
}

enum RunningOutcome {
    Alive,
    Exited(String),
    Wedged,
    WaitError(String),
}

struct Stopping {
    child: Child,
    stdout: ChildStdout,
    reader: StatusReader,
    deadline: Instant,
    killed: bool,
    /// A `try_wait` error was already logged for this worker.
    wait_error_logged: bool,
}

/// Audio tee counters, surfaced in `led.status` for diagnostics.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct LedTeeCounters {
    /// `audio_bands` lines written to the worker.
    pub audio_sent: u64,
    /// Lines replaced unsent because the pipe was full (latest wins).
    pub audio_coalesced: u64,
    /// Hard write failures on the worker's stdin (any stream).
    pub write_errors: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LedWorkerInfo {
    /// A worker binary is configured and present.
    pub available: bool,
    pub running: bool,
    pub pid: Option<u32>,
    /// A previous worker is still restoring devices after a stop.
    pub stopping: bool,
    pub restarts: u64,
    pub restart_budget_exhausted: bool,
    /// How the last worker that exited without being asked to ended.
    pub last_exit: Option<String>,
    /// Age of the latest `led_status` line, while running.
    pub status_age_ms: Option<u64>,
    /// The restore summary from the last worker's final `stopped` line.
    pub last_restore: Option<RestoreSummary>,
    pub status_lines_rejected: u64,
}

/// `led.status`: the daemon's view plus the worker's latest status line.
#[derive(Debug, Clone, Serialize)]
pub struct LedStatusReport {
    /// The `led.enabled` setting.
    pub enabled: bool,
    /// `disabled | worker_unavailable | starting | connecting |
    /// server_absent | active | off | degraded`.
    pub state: &'static str,
    /// Why: `not_configured`/`not_installed`/`not_executable`/
    /// `restart_budget_exhausted` for `worker_unavailable`; `restarting`/`worker_stopping` for
    /// `starting`; the worker's own reason or `worker_unresponsive` for
    /// `degraded`; `paused` for `off` while the pause policy holds the LEDs
    /// off. `null` otherwise.
    pub reason: Option<String>,
    /// True while the F3/F4 pause verdict forces the worker to mode `off`.
    pub paused: bool,
    pub settings: LedSettings,
    pub worker: LedWorkerInfo,
    /// The latest `led_status` line (`LED_PROTOCOL_V1.md`, ≤ 32 KiB) while
    /// a worker runs; `null` otherwise.
    pub worker_status: Option<StatusLine>,
    pub tee: LedTeeCounters,
}

/// `led.reprobe` outcome.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReprobeOutcome {
    pub accepted: bool,
    /// `reprobe` (sent to the running worker) or `respawn` (a missing
    /// binary or exhausted restart budget was reset and the worker is being
    /// started again).
    pub action: Option<&'static str>,
    /// Why it was not accepted: `disabled`, `not_configured`,
    /// `rate_limited`, `worker_restarting`.
    pub reason: Option<&'static str>,
    pub retry_after_ms: Option<u64>,
}

pub struct LedManager {
    config: Option<LedWorkerConfig>,
    settings: LedSettings,
    paused: bool,
    frame: Option<PathBuf>,
    running: Option<Running>,
    stopping: Option<Stopping>,
    restart_history: VecDeque<Instant>,
    restarts: u64,
    next_spawn_at: Option<Instant>,
    budget_exhausted: bool,
    /// `not_configured` (no worker path), `not_installed` (path missing) or
    /// `not_executable` (present but cannot be executed). None of them is a
    /// restart strike.
    unavailable: Option<&'static str>,
    last_exit: Option<String>,
    last_restore: Option<RestoreSummary>,
    last_reprobe: Option<Instant>,
    tee: LedTeeCounters,
    status_lines_rejected: u64,
    timings: Timings,
    /// `--saved-modes` for the worker (inside the daemon's state
    /// directory): remembered pre-control device modes survive a worker
    /// that died without restoring them.
    saved_modes: Option<PathBuf>,
}

/// File name of the worker's remembered-modes file in the state directory.
pub const SAVED_MODES_FILE: &str = "led-saved-modes.json";

impl LedManager {
    pub fn new(config: Option<LedWorkerConfig>, settings: LedSettings) -> Self {
        let unavailable = config.is_none().then_some("not_configured");
        let next_spawn_at = settings.enabled.then(Instant::now);
        Self {
            config,
            settings,
            paused: false,
            frame: None,
            running: None,
            stopping: None,
            restart_history: VecDeque::new(),
            restarts: 0,
            next_spawn_at,
            budget_exhausted: false,
            unavailable,
            last_exit: None,
            last_restore: None,
            last_reprobe: None,
            tee: LedTeeCounters::default(),
            status_lines_rejected: 0,
            timings: Timings::default(),
            saved_modes: None,
        }
    }

    /// Passes `<state_dir>/led-saved-modes.json` to every spawned worker
    /// (`--saved-modes`). Without it the worker remembers modes in memory
    /// only.
    pub fn set_state_dir(&mut self, state_dir: &Path) {
        self.saved_modes = Some(state_dir.join(SAVED_MODES_FILE));
    }

    /// The mode the worker should run: `off` while paused.
    fn effective_mode(&self) -> Mode {
        if self.paused {
            Mode::Off
        } else {
            self.settings.mode
        }
    }

    /// LED sync wants system audio: enabled, a worker is configured and
    /// usable, and the configured mode reads audio (`spectrum`,
    /// `ambient+pulse`). The supervisor publishes this to the audio
    /// capture service, which runs capture while it holds (follows the
    /// settings, not the pause verdict).
    pub fn wants_audio(&self) -> bool {
        self.settings.enabled
            && self.config.is_some()
            && self.unavailable.is_none()
            && !self.budget_exhausted
            && mode_reads_audio(self.settings.mode)
    }

    /// A running worker would use an audio frame right now (wants audio,
    /// not paused). Gates the renderer-less audio path (generation 0).
    pub fn consumes_audio(&self) -> bool {
        self.wants_audio()
            && !self.paused
            && self
                .running
                .as_ref()
                .is_some_and(|running| !running.stdin_broken)
    }

    /// Applies a new (validated) settings record live: enabling spawns the
    /// worker (fresh restart budget), disabling stops it (devices restored),
    /// anything else becomes one merged `led_config` line — no respawn.
    pub fn set_settings(&mut self, settings: LedSettings) {
        let previous = std::mem::replace(&mut self.settings, settings);
        if previous.enabled != self.settings.enabled {
            if self.settings.enabled {
                self.restart_history.clear();
                self.budget_exhausted = false;
                if self.config.is_some() {
                    self.unavailable = None;
                }
                self.next_spawn_at = Some(Instant::now());
            } else {
                self.next_spawn_at = None;
                self.begin_stop("disabled");
            }
            return;
        }
        let mut patch = ConfigPatch::default();
        if previous.mode != self.settings.mode && !self.paused {
            patch.mode = Some(self.settings.mode);
        }
        if previous.brightness != self.settings.brightness {
            patch.brightness = Some(self.settings.brightness);
        }
        if previous.rate_hz != self.settings.rate_hz {
            patch.rate_hz = Some(self.settings.rate_hz);
        }
        if previous.disabled_devices != self.settings.disabled_devices {
            patch.disabled_devices = Some(self.settings.disabled_devices.clone());
        }
        if !patch.is_empty() {
            self.queue_config(patch);
        }
    }

    /// F3/F4 pause verdict: while paused the worker runs mode `off`
    /// (devices back on the user's own lighting); resume restores the
    /// configured mode. Level-triggered: only a change sends a line.
    pub fn set_paused(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        eprintln!(
            "event=led.pause paused={paused} mode={}",
            self.effective_mode().as_str()
        );
        self.queue_config(ConfigPatch {
            mode: Some(self.effective_mode()),
            ..ConfigPatch::default()
        });
    }

    /// The active renderer's frame file (`None` = no renderer live).
    pub fn set_frame(&mut self, frame: Option<&Path>) {
        if self.frame.as_deref() == frame {
            return;
        }
        self.frame = frame.map(Path::to_path_buf);
        if let Some(running) = self.running.as_mut() {
            running.pending_frame = Some(encode_frame_line(self.frame.as_deref()));
            flush(running, &mut self.tee);
        }
    }

    /// Tees one encoded `audio_bands` line (the renderer's exact bytes) to
    /// the worker. Never blocks: a full pipe keeps only this newest line.
    pub fn offer_audio(&mut self, line: &[u8]) {
        let Some(running) = self.running.as_mut() else {
            return;
        };
        if running.stdin_broken {
            return;
        }
        if running.pending_audio.take().is_some() {
            self.tee.audio_coalesced = self.tee.audio_coalesced.saturating_add(1);
        }
        let others_pending = running.pending_config.is_some()
            || running.pending_frame.is_some()
            || running.pending_reprobe
            || running.pending_status_request;
        if others_pending {
            running.pending_audio = Some(line.to_vec());
            flush(running, &mut self.tee);
            return;
        }
        match write_line(running, line, &mut self.tee) {
            Ok(PipeWrite::Written) => {
                self.tee.audio_sent = self.tee.audio_sent.saturating_add(1);
            }
            Ok(PipeWrite::WouldBlock) => running.pending_audio = Some(line.to_vec()),
            Err(()) => {}
        }
    }

    /// `led.reprobe`: "Check again". Rate-limited.
    pub fn reprobe(&mut self) -> ReprobeOutcome {
        let refuse = |reason| ReprobeOutcome {
            accepted: false,
            action: None,
            reason: Some(reason),
            retry_after_ms: None,
        };
        if !self.settings.enabled {
            return refuse("disabled");
        }
        if self.config.is_none() {
            return refuse("not_configured");
        }
        let now = Instant::now();
        if let Some(last) = self.last_reprobe {
            let elapsed = now.saturating_duration_since(last);
            if elapsed < self.timings.reprobe_interval {
                return ReprobeOutcome {
                    retry_after_ms: Some(
                        (self.timings.reprobe_interval - elapsed)
                            .as_millis()
                            .max(1)
                            .try_into()
                            .unwrap_or(u64::MAX),
                    ),
                    ..refuse("rate_limited")
                };
            }
        }
        self.last_reprobe = Some(now);
        if self.running.is_none() && (self.unavailable.is_some() || self.budget_exhausted) {
            self.unavailable = None;
            self.budget_exhausted = false;
            self.restart_history.clear();
            self.next_spawn_at = Some(now);
            eprintln!("event=led.worker.reprobe action=respawn");
            return ReprobeOutcome {
                accepted: true,
                action: Some("respawn"),
                reason: None,
                retry_after_ms: None,
            };
        }
        let Some(running) = self.running.as_mut() else {
            // Between a stop and the next spawn: say when to ask again.
            let wait = self
                .next_spawn_at
                .map(|at| at.saturating_duration_since(now))
                .or_else(|| {
                    self.stopping
                        .as_ref()
                        .map(|stopping| stopping.deadline.saturating_duration_since(now))
                })
                .unwrap_or(self.timings.restart_delay);
            return ReprobeOutcome {
                retry_after_ms: Some(
                    wait.as_millis()
                        .clamp(1, u128::from(u64::MAX))
                        .try_into()
                        .unwrap_or(u64::MAX),
                ),
                ..refuse("worker_restarting")
            };
        };
        running.pending_reprobe = true;
        running.pending_status_request = true;
        flush(running, &mut self.tee);
        ReprobeOutcome {
            accepted: true,
            action: Some("reprobe"),
            reason: None,
            retry_after_ms: None,
        }
    }

    pub fn status(&self) -> LedStatusReport {
        let now = Instant::now();
        let latest = self
            .running
            .as_ref()
            .and_then(|running| running.reader.latest.clone());
        let status_age = self.running.as_ref().and_then(|running| {
            running
                .reader
                .latest_at
                .map(|at| now.saturating_duration_since(at))
        });
        let (state, reason) = self.derive_state(latest.as_ref(), status_age);
        let rejected = self.status_lines_rejected.saturating_add(
            self.running
                .as_ref()
                .map_or(0, |running| running.reader.rejected),
        );
        LedStatusReport {
            enabled: self.settings.enabled,
            state,
            reason,
            paused: self.paused,
            settings: self.settings.clone(),
            worker: LedWorkerInfo {
                available: self.config.is_some() && self.unavailable.is_none(),
                running: self.running.is_some(),
                pid: self.running.as_ref().map(|running| running.child.id()),
                stopping: self.stopping.is_some(),
                restarts: self.restarts,
                restart_budget_exhausted: self.budget_exhausted,
                last_exit: self.last_exit.clone(),
                status_age_ms: status_age.map(|age| age.as_millis().try_into().unwrap_or(u64::MAX)),
                last_restore: self.last_restore.clone(),
                status_lines_rejected: rejected,
            },
            worker_status: latest,
            tee: self.tee,
        }
    }

    fn derive_state(
        &self,
        latest: Option<&StatusLine>,
        status_age: Option<Duration>,
    ) -> (&'static str, Option<String>) {
        if !self.settings.enabled {
            return ("disabled", None);
        }
        if let Some(reason) = self.unavailable {
            return ("worker_unavailable", Some(reason.to_string()));
        }
        if self.budget_exhausted {
            return (
                "worker_unavailable",
                Some("restart_budget_exhausted".to_string()),
            );
        }
        let Some(running) = self.running.as_ref() else {
            let reason = if self.stopping.is_some() {
                "worker_stopping"
            } else {
                "restarting"
            };
            return ("starting", Some(reason.to_string()));
        };
        let silent_for = status_age
            .unwrap_or_else(|| Instant::now().saturating_duration_since(running.spawned_at));
        if silent_for >= self.timings.status_stale {
            return ("degraded", Some("worker_unresponsive".to_string()));
        }
        let Some(latest) = latest else {
            return ("starting", None);
        };
        match latest.state {
            State::Connecting => ("connecting", None),
            State::ServerAbsent => ("server_absent", None),
            // `waiting_for_source` passes through: connected, nothing
            // driven until the mode has a frame / audio.
            State::Active => ("active", latest.reason.clone()),
            State::Off => (
                "off",
                (self.paused && self.settings.mode != Mode::Off).then(|| "paused".to_string()),
            ),
            State::Degraded => (
                "degraded",
                Some(latest.reason.clone().unwrap_or_else(|| "error".to_string())),
            ),
            State::Stopped => ("starting", Some("worker_stopping".to_string())),
        }
    }

    /// One supervisor tick: reap/kill a stopping worker, reap an exited
    /// running worker (bounded restart), read status, detect a wedged
    /// worker, flush pending lines, spawn when due. Never waits.
    pub fn tick(&mut self) {
        self.tick_stopping();
        let now = Instant::now();
        let wedged_after = self.timings.wedged;
        let outcome = self.running.as_mut().map(|running| {
            running.reader.drain(&running.stdout);
            match running.child.try_wait() {
                Ok(Some(status)) => RunningOutcome::Exited(exit_detail(&status)),
                Ok(None) => {
                    let silent_since = running.reader.latest_at.unwrap_or(running.spawned_at);
                    if now.saturating_duration_since(silent_since) >= wedged_after {
                        RunningOutcome::Wedged
                    } else {
                        RunningOutcome::Alive
                    }
                }
                Err(error) => RunningOutcome::WaitError(error.to_string()),
            }
        });
        match outcome {
            None => {}
            Some(RunningOutcome::Alive) => {
                if let Some(running) = self.running.as_mut() {
                    flush(running, &mut self.tee);
                }
            }
            Some(RunningOutcome::Exited(detail)) => {
                eprintln!("event=led.worker.exited detail={detail}");
                if let Some(running) = self.running.take() {
                    self.absorb_reader(&running.reader);
                }
                self.last_exit = Some(detail);
                self.schedule_restart();
            }
            Some(RunningOutcome::Wedged) => {
                eprintln!(
                    "event=led.worker.wedged detail=no status line for {} ms",
                    wedged_after.as_millis()
                );
                self.last_exit = Some("wedged".to_string());
                self.begin_stop("wedged");
                self.schedule_restart();
            }
            Some(RunningOutcome::WaitError(error)) => {
                eprintln!("event=led.worker.wait_error detail={error}");
                self.last_exit = Some("wait_error".to_string());
                self.begin_stop("wait_error");
                self.schedule_restart();
            }
        }
        if self.settings.enabled
            && self.running.is_none()
            && self.stopping.is_none()
            && self.unavailable.is_none()
            && !self.budget_exhausted
            && self.next_spawn_at.is_some_and(|at| now >= at)
        {
            self.next_spawn_at = None;
            if let Err(error) = self.spawn() {
                eprintln!("event=led.worker.spawn_failed detail={error:#}");
                self.last_exit = Some("spawn_failed".to_string());
                self.schedule_restart();
            }
        }
    }

    /// Daemon shutdown, first half: ask the worker to restore and exit
    /// (non-blocking), so renderers can be stopped in parallel.
    pub fn begin_shutdown(&mut self) {
        self.next_spawn_at = None;
        self.begin_stop("shutdown");
    }

    /// Daemon shutdown, second half: wait out the stop grace (bounded), then
    /// SIGKILL and reap. The only place that waits: at most `stop_grace +
    /// KILL_REAP_LIMIT` (`tick_stopping` drops a killed worker that never
    /// reaps), plus a hard backstop in case the clock misbehaves.
    pub fn finish_shutdown(&mut self) {
        self.begin_shutdown();
        let backstop = Instant::now() + self.timings.stop_grace + KILL_REAP_LIMIT * 2;
        while self.stopping.is_some() {
            self.tick_stopping();
            if self.stopping.is_some() {
                if Instant::now() >= backstop {
                    if let Some(stopping) = self.stopping.take() {
                        eprintln!(
                            "event=led.worker.unreaped pid={} detail=shutdown gave up waiting",
                            stopping.child.id()
                        );
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn absorb_reader(&mut self, reader: &StatusReader) {
        self.status_lines_rejected = self.status_lines_rejected.saturating_add(reader.rejected);
        if let Some(restore) = reader
            .latest
            .as_ref()
            .filter(|status| status.state == State::Stopped)
            .and_then(|status| status.restore.clone())
        {
            self.last_restore = Some(restore);
        }
    }

    fn tick_stopping(&mut self) {
        let Some(stopping) = self.stopping.as_mut() else {
            return;
        };
        stopping.reader.drain(&stopping.stdout);
        let pid = stopping.child.id();
        match stopping.child.try_wait() {
            Ok(Some(status)) => {
                stopping.reader.drain(&stopping.stdout);
                let stopping = self.stopping.take().expect("stopping worker");
                eprintln!(
                    "event=led.worker.stopped pid={pid} detail={}",
                    exit_detail(&status)
                );
                self.absorb_reader(&stopping.reader);
            }
            result => {
                if let Err(error) = result {
                    if !stopping.wait_error_logged {
                        stopping.wait_error_logged = true;
                        eprintln!("event=led.worker.wait_error pid={pid} detail={error}");
                    }
                    if stopping.killed {
                        // Nothing more can be done; drop the handle.
                        self.stopping = None;
                        return;
                    }
                }
                let now = Instant::now();
                if !stopping.killed && now >= stopping.deadline {
                    signal_process_group(pid, libc::SIGKILL);
                    let _ = stopping.child.kill();
                    stopping.killed = true;
                    eprintln!("event=led.worker.forced_kill pid={pid}");
                } else if stopping.killed && now >= stopping.deadline + KILL_REAP_LIMIT {
                    // SIGKILLed yet still not reapable (uninterruptible
                    // sleep): stop waiting so the manager is never stuck.
                    eprintln!(
                        "event=led.worker.unreaped pid={pid} detail=not reaped {} ms after SIGKILL",
                        KILL_REAP_LIMIT.as_millis()
                    );
                    self.stopping = None;
                }
            }
        }
    }

    /// Close stdin (EOF = restore + exit 0 by contract) and SIGTERM the
    /// worker's group; later ticks reap it or SIGKILL after the grace.
    fn begin_stop(&mut self, why: &str) {
        let Some(running) = self.running.take() else {
            return;
        };
        let Running {
            child,
            stdin,
            stdout,
            reader,
            ..
        } = running;
        drop(stdin);
        let pid = child.id();
        signal_process_group(pid, libc::SIGTERM);
        eprintln!("event=led.worker.stopping pid={pid} why={why}");
        if let Some(previous) = self.stopping.take() {
            // Defensive: spawning waits for the previous worker, so two
            // stopping workers cannot exist; never leak one if it happens.
            let mut previous = previous;
            signal_process_group(previous.child.id(), libc::SIGKILL);
            let _ = previous.child.kill();
            let _ = previous.child.wait();
        }
        self.stopping = Some(Stopping {
            child,
            stdout,
            reader,
            deadline: Instant::now() + self.timings.stop_grace,
            killed: false,
            wait_error_logged: false,
        });
    }

    fn queue_config(&mut self, patch: ConfigPatch) {
        let Some(running) = self.running.as_mut() else {
            return;
        };
        match running.pending_config.as_mut() {
            Some(pending) => pending.merge(patch),
            None => running.pending_config = Some(patch),
        }
        flush(running, &mut self.tee);
    }

    fn schedule_restart(&mut self) {
        if !self.settings.enabled {
            return;
        }
        let now = Instant::now();
        let cutoff = now.checked_sub(RESTART_WINDOW).unwrap_or(now);
        while self
            .restart_history
            .front()
            .is_some_and(|front| *front < cutoff)
        {
            self.restart_history.pop_front();
        }
        if self.restart_history.len() >= MAX_RESTARTS {
            if !self.budget_exhausted {
                eprintln!(
                    "event=led.worker.disabled detail=restart budget exhausted ({MAX_RESTARTS} restarts within {}s already used)",
                    RESTART_WINDOW.as_secs()
                );
            }
            self.budget_exhausted = true;
            self.next_spawn_at = None;
            return;
        }
        self.restart_history.push_back(now);
        self.restarts = self.restarts.saturating_add(1);
        self.next_spawn_at = Some(now + self.timings.restart_delay);
    }

    fn spawn(&mut self) -> Result<()> {
        let Some(config) = self.config.clone() else {
            self.unavailable = Some("not_configured");
            return Ok(());
        };
        if !config.worker_path.is_file() {
            // Quiet: one line per transition, no restart strike.
            eprintln!(
                "event=led.worker.unavailable detail=not installed path={}",
                config.worker_path.display()
            );
            self.unavailable = Some("not_installed");
            return Ok(());
        }
        if !is_executable(&config.worker_path) {
            // Present but not runnable (mode bits): a packaging/permission
            // problem, not a crash — same quiet treatment as a missing file.
            eprintln!(
                "event=led.worker.unavailable detail=not executable path={}",
                config.worker_path.display()
            );
            self.unavailable = Some("not_executable");
            return Ok(());
        }
        let mut command = Command::new(&config.worker_path);
        command
            .env_clear()
            .arg("--mode")
            .arg(self.effective_mode().as_str())
            .arg("--brightness")
            .arg(self.settings.brightness.to_string())
            .arg("--rate-hz")
            .arg(self.settings.rate_hz.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(port) = config.port {
            command.arg("--port").arg(port.to_string());
        }
        if let Some(path) = &self.saved_modes {
            command.arg("--saved-modes").arg(path);
        }
        for id in &self.settings.disabled_devices {
            command.arg("--disable-device").arg(id);
        }
        let expected_parent = i32::try_from(std::process::id()).context("daemon pid overflow")?;
        // SAFETY: runs in the child between fork and exec; only
        // async-signal-safe libc calls, no allocation. Own process group,
        // parent-death SIGTERM (not SIGKILL: the worker restores devices on
        // SIGTERM), parent check, no_new_privs. No network namespace: the
        // worker must reach the HOST's loopback (LED_PROTOCOL_V1.md).
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != expected_parent {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "daemon exited before LED worker exec",
                    ));
                }
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            // EACCES at exec (e.g. a noexec mount) despite the mode bits:
            // still not a crash of the worker, so no restart strike.
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!(
                    "event=led.worker.unavailable detail=not executable path={} error={error}",
                    config.worker_path.display()
                );
                self.unavailable = Some("not_executable");
                return Ok(());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("launch {}", config.worker_path.display()));
            }
        };
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            signal_process_group(child.id(), libc::SIGKILL);
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("LED worker pipes missing");
        };
        if let Err(error) =
            set_nonblocking(stdin.as_raw_fd()).and_then(|()| set_nonblocking(stdout.as_raw_fd()))
        {
            signal_process_group(child.id(), libc::SIGKILL);
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("LED worker pipes nonblocking");
        }
        eprintln!(
            "event=led.worker.spawned pid={} mode={}",
            child.id(),
            self.effective_mode().as_str()
        );
        let mut running = Running {
            child,
            stdin,
            stdout,
            reader: StatusReader::default(),
            spawned_at: Instant::now(),
            pending_config: None,
            // The current frame source right after every (re)spawn.
            pending_frame: Some(encode_frame_line(self.frame.as_deref())),
            pending_reprobe: false,
            pending_status_request: false,
            pending_audio: None,
            stdin_broken: false,
        };
        flush(&mut running, &mut self.tee);
        self.running = Some(running);
        Ok(())
    }

    #[cfg(test)]
    fn set_timings_for_test(&mut self, timings: Timings) {
        self.timings = timings;
    }

    #[cfg(test)]
    pub(crate) fn running_pid(&self) -> Option<u32> {
        self.running.as_ref().map(|running| running.child.id())
    }

    #[cfg(test)]
    pub(crate) fn is_stopping(&self) -> bool {
        self.stopping.is_some()
    }
}

impl Drop for LedManager {
    fn drop(&mut self) {
        self.finish_shutdown();
    }
}

/// The modes whose output depends on captured system audio.
pub fn mode_reads_audio(mode: Mode) -> bool {
    matches!(mode, Mode::Spectrum | Mode::AmbientPulse)
}

/// Any execute bit set (the spawn still handles EACCES at exec).
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

/// `led_frame` for a path the frame protocol produced; a path the wire
/// cannot carry (non-UTF-8, `..`) degrades to `null` — the worker then
/// holds and fades instead of reading the wrong file.
fn encode_frame_line(frame: Option<&Path>) -> Vec<u8> {
    match proto::encode_frame(frame) {
        Ok(line) => line,
        Err(error) => {
            eprintln!("event=led.frame_unencodable detail={error}");
            proto::encode_frame(None).unwrap_or_default()
        }
    }
}

/// One atomic nonblocking line write. `Err(())` = hard failure (already
/// counted and logged; the stream is marked broken).
fn write_line(
    running: &mut Running,
    line: &[u8],
    tee: &mut LedTeeCounters,
) -> std::result::Result<PipeWrite, ()> {
    match try_write_input(&running.stdin, line) {
        Ok(result) => Ok(result),
        Err(error) => {
            tee.write_errors = tee.write_errors.saturating_add(1);
            if !running.stdin_broken {
                eprintln!("event=led.stdin_write_error detail={error}");
            }
            running.stdin_broken = true;
            running.pending_config = None;
            running.pending_frame = None;
            running.pending_reprobe = false;
            running.pending_status_request = false;
            running.pending_audio = None;
            Err(())
        }
    }
}

/// Writes pending lines in priority order (config, frame, reprobe, status
/// request, audio); stops at the first full-pipe answer so nothing is
/// reordered behind a line that did not fit.
fn flush(running: &mut Running, tee: &mut LedTeeCounters) {
    if running.stdin_broken {
        return;
    }
    if let Some(patch) = running.pending_config.take() {
        match proto::encode_config(&patch) {
            Ok(line) => match write_line(running, &line, tee) {
                Ok(PipeWrite::Written) => {}
                Ok(PipeWrite::WouldBlock) => {
                    running.pending_config = Some(patch);
                    return;
                }
                Err(()) => return,
            },
            Err(error) => eprintln!("event=led.config_unencodable detail={error}"),
        }
    }
    if let Some(line) = running.pending_frame.take() {
        match write_line(running, &line, tee) {
            Ok(PipeWrite::Written) => {}
            Ok(PipeWrite::WouldBlock) => {
                running.pending_frame = Some(line);
                return;
            }
            Err(()) => return,
        }
    }
    if running.pending_reprobe {
        match write_line(running, &proto::encode_reprobe(), tee) {
            Ok(PipeWrite::Written) => running.pending_reprobe = false,
            Ok(PipeWrite::WouldBlock) | Err(()) => return,
        }
    }
    if running.pending_status_request {
        match write_line(running, &proto::encode_status_request(), tee) {
            Ok(PipeWrite::Written) => running.pending_status_request = false,
            Ok(PipeWrite::WouldBlock) | Err(()) => return,
        }
    }
    if let Some(line) = running.pending_audio.take() {
        match write_line(running, &line, tee) {
            Ok(PipeWrite::Written) => tee.audio_sent = tee.audio_sent.saturating_add(1),
            Ok(PipeWrite::WouldBlock) => running.pending_audio = Some(line),
            Err(()) => {}
        }
    }
}

fn signal_process_group(pid: u32, signal: libc::c_int) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: the worker is placed in a process group whose id equals
        // its pid before exec; a negative pid addresses that group.
        unsafe {
            libc::kill(-pid, signal);
        }
    }
}

fn exit_detail(status: &ExitStatus) -> String {
    if let Some(code) = status.code() {
        format!("exit_code_{code}")
    } else if let Some(signal) = status.signal() {
        format!("signal_{signal}")
    } else {
        "unknown_exit".to_string()
    }
}

#[cfg(test)]
pub(crate) mod tests;
