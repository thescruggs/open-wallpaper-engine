// SPDX-License-Identifier: GPL-3.0-or-later
//! Process plumbing: the bounded stdin reader (latest-wins inbox), the
//! latest-wins stdout status writer, and the main loop with its bounded
//! stop/restore sequence.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use kwe_led_protocol::{
    ConfigPatch, Control, ControlError, MAX_LINE_BYTES, RestoreSummary, State, StatusLine,
    decode_control, encode_status,
};

use crate::engine::Engine;

/// Main-loop wake granularity: bounds SIGTERM latency between ticks.
pub const WAKE: Duration = Duration::from_millis(50);
/// At most one status line per this interval.
pub const STATUS_MIN_INTERVAL: Duration = Duration::from_millis(250);
/// A status line is emitted at least this often (with fresh counters).
pub const STATUS_HEARTBEAT: Duration = Duration::from_secs(5);
/// Hard bound on the restore performed on stop.
pub const RESTORE_DEADLINE: Duration = Duration::from_millis(1500);
/// Bound on flushing the final status line.
pub const FINAL_FLUSH: Duration = Duration::from_millis(300);
/// stdin read chunk.
const READ_CHUNK: usize = 8192;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// stdin → inbox
// ---------------------------------------------------------------------------

/// Everything the stdin reader hands to the main loop. Every field is
/// latest-wins (or a merge of set-type updates), so nothing can queue.
#[derive(Debug, Default)]
pub struct Inbox {
    pub config: Option<ConfigPatch>,
    pub frame: Option<Option<PathBuf>>,
    pub audio: Option<(Instant, Vec<f32>, Vec<f32>)>,
    pub status_request: bool,
    pub reprobe: bool,
    pub stop: bool,
    pub eof: bool,
    pub lines: u64,
    pub rejected: u64,
    pub ignored: u64,
    /// Most recent rejection reason not yet logged.
    pub reject_note: Option<String>,
}

pub struct Shared {
    pub inbox: Mutex<Inbox>,
    pub wake: Condvar,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inbox: Mutex::new(Inbox::default()),
            wake: Condvar::new(),
        })
    }

    fn accept(&self, line: &[u8]) {
        let decoded = decode_control(line);
        let mut inbox = lock(&self.inbox);
        inbox.lines = inbox.lines.saturating_add(1);
        let notify = match decoded {
            Ok(Control::Audio(f)) => {
                inbox.audio = Some((Instant::now(), f.left, f.right));
                false
            }
            Ok(Control::Config(patch)) => {
                inbox
                    .config
                    .get_or_insert_with(ConfigPatch::default)
                    .merge(patch);
                true
            }
            Ok(Control::Frame(p)) => {
                inbox.frame = Some(p);
                true
            }
            Ok(Control::StatusRequest) => {
                inbox.status_request = true;
                true
            }
            Ok(Control::Reprobe) => {
                inbox.reprobe = true;
                true
            }
            Ok(Control::Stop) => {
                inbox.stop = true;
                true
            }
            Err(ControlError::Ignored) => {
                inbox.ignored = inbox.ignored.saturating_add(1);
                false
            }
            Err(e) => {
                inbox.rejected = inbox.rejected.saturating_add(1);
                inbox.reject_note = Some(e.to_string());
                false
            }
        };
        drop(inbox);
        if notify {
            self.wake.notify_all();
        }
    }

    fn reject_oversized(&self) {
        let mut inbox = lock(&self.inbox);
        inbox.lines = inbox.lines.saturating_add(1);
        inbox.rejected = inbox.rejected.saturating_add(1);
        inbox.reject_note = Some(ControlError::TooLarge.to_string());
    }

    fn eof(&self) {
        lock(&self.inbox).eof = true;
        self.wake.notify_all();
    }
}

/// Reads NDJSON from `input` until EOF/error. Lines longer than
/// [`MAX_LINE_BYTES`] are discarded up to their newline without buffering
/// them; memory is bounded by `MAX_LINE_BYTES + READ_CHUNK`.
pub fn read_lines(mut input: impl Read, shared: &Shared) {
    let mut chunk = vec![0u8; READ_CHUNK];
    let mut line: Vec<u8> = Vec::with_capacity(MAX_LINE_BYTES);
    let mut discarding = false;
    loop {
        let n = match input.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let mut rest = &chunk[..n];
        while !rest.is_empty() {
            match rest.iter().position(|b| *b == b'\n') {
                Some(pos) => {
                    if discarding {
                        discarding = false;
                    } else if line.len() + pos + 1 > MAX_LINE_BYTES {
                        shared.reject_oversized();
                    } else {
                        line.extend_from_slice(&rest[..=pos]);
                        if line != b"\n" {
                            shared.accept(&line);
                        }
                    }
                    line.clear();
                    rest = &rest[pos + 1..];
                }
                None => {
                    if !discarding {
                        if line.len() + rest.len() > MAX_LINE_BYTES {
                            shared.reject_oversized();
                            line.clear();
                            discarding = true;
                        } else {
                            line.extend_from_slice(rest);
                        }
                    }
                    rest = &[];
                }
            }
        }
    }
    if !discarding && !line.is_empty() {
        shared.accept(&line);
    }
    shared.eof();
}

// ---------------------------------------------------------------------------
// status → stdout (latest-wins, never blocks the main loop)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct OutState {
    line: Option<Vec<u8>>,
    busy: bool,
    closed: bool,
    replaced: u64,
}

pub struct Outbox {
    state: Mutex<OutState>,
    cv: Condvar,
}

impl Outbox {
    /// Starts the writer thread on `out`.
    pub fn start(out: impl Write + Send + 'static) -> Arc<Self> {
        let outbox = Arc::new(Self {
            state: Mutex::new(OutState::default()),
            cv: Condvar::new(),
        });
        let o = outbox.clone();
        thread::Builder::new()
            .name("kwe-led-status".into())
            .spawn(move || o.write_loop(out))
            .ok();
        outbox
    }

    fn write_loop(&self, mut out: impl Write) {
        loop {
            let line = {
                let mut s = lock(&self.state);
                while s.line.is_none() && !s.closed {
                    s = self.cv.wait(s).unwrap_or_else(|e| e.into_inner());
                }
                if s.closed {
                    return;
                }
                s.busy = true;
                s.line.take()
            };
            let ok = match line {
                Some(l) => out.write_all(&l).and_then(|()| out.flush()).is_ok(),
                None => true,
            };
            let mut s = lock(&self.state);
            s.busy = false;
            if !ok {
                s.closed = true;
            }
            self.cv.notify_all();
        }
    }

    /// Replaces any not-yet-written line (latest wins).
    pub fn put(&self, line: Vec<u8>) {
        let mut s = lock(&self.state);
        if s.closed {
            return;
        }
        if s.line.replace(line).is_some() {
            s.replaced = s.replaced.saturating_add(1);
        }
        drop(s);
        self.cv.notify_all();
    }

    /// Waits until everything put so far was written, or the deadline.
    pub fn flush(&self, deadline: Instant) -> bool {
        let mut s = lock(&self.state);
        loop {
            if s.closed || (s.line.is_none() && !s.busy) {
                return !s.closed;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            s = self
                .cv
                .wait_timeout(s, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

// ---------------------------------------------------------------------------
// main loop
// ---------------------------------------------------------------------------

/// Synthetic stereo bands for `--synthetic-audio` (development without the
/// daemon): a 2 Hz bass pulse plus a peak sweeping across the spectrum
/// every 4 s. Pure; `n` is 16, 32, or 64.
pub fn synthetic_bands(t: f32, n: usize) -> (Vec<f32>, Vec<f32>) {
    let pulse = (0.5 + 0.5 * (std::f32::consts::TAU * 2.0 * t).sin()).powi(2);
    let peak = (t * 0.25).fract() * n as f32;
    let bands: Vec<f32> = (0..n)
        .map(|i| {
            let d = i as f32 - peak;
            let sweep = (-(d * d) / 6.0).exp();
            let bass = if i < n / 8 { pulse } else { 0.0 };
            sweep.max(bass).clamp(0.0, 1.0)
        })
        .collect();
    (bands.clone(), bands)
}

pub struct RunOptions {
    /// stdin EOF does not stop the worker (standalone development).
    pub standalone: bool,
    pub synthetic_audio: bool,
    pub duration: Option<Duration>,
}

/// Fingerprint of the status fields whose change warrants a line.
fn material(status: &StatusLine) -> String {
    let mut s = status.clone();
    s.seq = 0;
    s.counters = Default::default();
    s.server.next_probe_ms = None;
    s.frame.sequence = None;
    serde_json::to_string(&s).unwrap_or_default()
}

/// Runs until SIGTERM/SIGINT (`terminated`), `led_stop`, stdin EOF (unless
/// standalone), or the duration; then restores devices within
/// [`RESTORE_DEADLINE`], writes the final `stopped` line, and returns.
pub fn run(
    mut engine: Engine,
    shared: Arc<Shared>,
    outbox: Arc<Outbox>,
    terminated: &AtomicBool,
    opts: RunOptions,
) {
    let start = Instant::now();
    let mut next_tick = start;
    // First line before the first (possibly slow) probe: the daemon learns
    // the worker is alive within milliseconds, in state `connecting`.
    let initial = engine.status(start, 1);
    let mut seq: u64 = 1;
    let mut last_material = material(&initial);
    outbox.put(encode_status(&initial));
    // Allow the post-probe line immediately: the minimum interval exists to
    // bound steady-state volume, not to delay the first real state.
    let mut last_emit: Option<Instant> = None;
    let mut force_emit = false;
    let mut next_synth = start;
    let stop_reason: &'static str = loop {
        let now = Instant::now();
        // Drain the inbox.
        let (config, frame, audio, status_request, reprobe, stop, eof, note, counts) = {
            let mut inbox = lock(&shared.inbox);
            (
                inbox.config.take(),
                inbox.frame.take(),
                inbox.audio.take(),
                std::mem::take(&mut inbox.status_request),
                std::mem::take(&mut inbox.reprobe),
                inbox.stop,
                inbox.eof,
                inbox.reject_note.take(),
                (inbox.lines, inbox.rejected, inbox.ignored),
            )
        };
        {
            let c = engine.counters_mut();
            (c.stdin_lines, c.stdin_rejected, c.stdin_ignored) = counts;
        }
        if let Some(note) = note {
            engine.log(now, "stdin_rejected", &format!("detail={note:?}"));
        }
        if terminated.load(Ordering::Acquire) {
            break "signal";
        }
        if stop {
            break "led_stop";
        }
        if eof && !opts.standalone {
            break "stdin_eof";
        }
        if opts
            .duration
            .is_some_and(|d| now.saturating_duration_since(start) >= d)
        {
            break "duration";
        }
        if let Some(patch) = config {
            let before = engine.settings().tick_interval();
            engine.apply_config(patch);
            let after = engine.settings().tick_interval();
            if after < before {
                // Faster rate: do not wait out the old, longer interval.
                next_tick = next_tick.min(now + after);
            }
        }
        if let Some(path) = frame {
            engine.set_frame_path(path);
        }
        if let Some((t, l, r)) = audio {
            engine.set_audio(t, &l, &r);
        }
        if opts.synthetic_audio && now >= next_synth {
            let (l, r) = synthetic_bands(now.saturating_duration_since(start).as_secs_f32(), 32);
            engine.set_audio(now, &l, &r);
            next_synth = now + Duration::from_millis(33);
        }
        if reprobe {
            engine.reprobe(now);
        }
        force_emit |= status_request;

        if now >= next_tick {
            engine.tick(now);
            // Measured from the actual tick start, so a late tick never
            // causes a catch-up burst: rate ≤ rate_hz, latest-wins.
            next_tick = now + engine.settings().tick_interval();
        }

        // Status: on material change / request / heartbeat, rate-limited.
        let now = Instant::now();
        let allowed =
            last_emit.is_none_or(|t| now.saturating_duration_since(t) >= STATUS_MIN_INTERVAL);
        let heartbeat =
            last_emit.is_none_or(|t| now.saturating_duration_since(t) >= STATUS_HEARTBEAT);
        let mut pending = false;
        if allowed {
            let status = engine.status(now, seq + 1);
            let m = material(&status);
            if force_emit || heartbeat || m != last_material {
                seq += 1;
                outbox.put(encode_status(&status));
                last_emit = Some(now);
                last_material = m;
                force_emit = false;
            }
        } else {
            pending = force_emit;
        }

        let mut wait = next_tick.saturating_duration_since(now).min(WAKE);
        if pending && let Some(t) = last_emit {
            wait = wait.min((t + STATUS_MIN_INTERVAL).saturating_duration_since(now));
        }
        if !wait.is_zero() {
            let inbox = lock(&shared.inbox);
            let idle = inbox.config.is_none()
                && inbox.frame.is_none()
                && !inbox.status_request
                && !inbox.reprobe
                && !inbox.stop
                && !(inbox.eof && !opts.standalone);
            if idle {
                drop(shared.wake.wait_timeout(inbox, wait));
            }
        }
    };
    shutdown(engine, &outbox, seq, stop_reason);
}

fn shutdown(engine: Engine, outbox: &Outbox, seq: u64, reason: &str) {
    let now = Instant::now();
    let mut final_status = engine.status(now, seq + 1);
    final_status.state = State::Stopped;
    final_status.reason = Some(reason.to_string());
    let parts = engine.into_restore_parts();
    let summary = if parts.is_empty() {
        RestoreSummary {
            attempted: 0,
            restored: 0,
            failed: 0,
            timed_out: false,
        }
    } else {
        let (tx, rx) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("kwe-led-restore".into())
            .spawn(move || {
                let _ = tx.send(parts.restore());
            })
            .is_ok();
        match spawned.then(|| rx.recv_timeout(RESTORE_DEADLINE)) {
            Some(Ok(o)) => RestoreSummary {
                attempted: o.attempted,
                restored: o.restored,
                failed: o.failed,
                timed_out: false,
            },
            _ => RestoreSummary {
                attempted: 0,
                restored: 0,
                failed: 0,
                timed_out: true,
            },
        }
    };
    if summary.timed_out || summary.failed > 0 {
        eprintln!(
            "event=led.restore attempted={} restored={} failed={} timed_out={}",
            summary.attempted, summary.restored, summary.failed, summary.timed_out
        );
    }
    final_status.restore = Some(summary);
    outbox.put(encode_status(&final_status));
    outbox.flush(Instant::now() + FINAL_FLUSH);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(bytes: &[u8]) -> Inbox {
        let shared = Shared::new();
        read_lines(bytes, &shared);
        std::mem::take(&mut *lock(&shared.inbox))
    }

    #[test]
    fn reader_bounds_lines_and_keeps_latest() {
        let mut input = Vec::new();
        input.extend_from_slice(br#"{"version":1,"type":"led_config","brightness":10}"#);
        input.push(b'\n');
        input.extend(std::iter::repeat_n(b'x', MAX_LINE_BYTES * 3));
        input.push(b'\n');
        input.extend_from_slice(b"garbage\n\n");
        input.extend_from_slice(
            br#"{"version":1,"type":"render_pause","sequence":1,"paused":true}"#,
        );
        input.push(b'\n');
        input.extend_from_slice(br#"{"version":1,"type":"led_config","mode":"spectrum"}"#);
        input.push(b'\n');
        input.extend_from_slice(br#"{"version":1,"type":"led_frame","path":"/a/b"}"#);
        let inbox = feed(&input);
        assert!(inbox.eof);
        assert_eq!(inbox.lines, 6, "the blank line is not counted");
        assert_eq!(inbox.rejected, 2, "oversized + garbage");
        assert_eq!(inbox.ignored, 1);
        let cfg = inbox.config.unwrap();
        assert_eq!(cfg.brightness, Some(10));
        assert_eq!(cfg.mode, Some(kwe_led_protocol::Mode::Spectrum));
        assert_eq!(
            inbox.frame,
            Some(Some(PathBuf::from("/a/b"))),
            "unterminated last line"
        );
    }

    #[test]
    fn oversized_line_split_across_chunks_is_discarded_to_its_newline() {
        let mut input = vec![b'{'; READ_CHUNK + 10];
        input.extend_from_slice(b"}\n");
        input.extend_from_slice(br#"{"version":1,"type":"led_stop"}"#);
        input.push(b'\n');
        let inbox = feed(&input);
        assert_eq!(inbox.rejected, 1);
        assert!(inbox.stop);
    }

    #[test]
    fn synthetic_bands_are_valid_audio() {
        for t in [0.0, 0.1, 1.3, 7.9] {
            let (l, r) = synthetic_bands(t, 32);
            assert!(kwe_input_protocol::AudioFrame::new(1, l, r).is_ok());
        }
    }

    #[test]
    fn outbox_is_latest_wins_and_flushes() {
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        struct W(Arc<Mutex<Vec<u8>>>);
        impl Write for W {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                lock(&self.0).extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let outbox = Outbox::start(W(out.clone()));
        outbox.put(b"a\n".to_vec());
        assert!(outbox.flush(Instant::now() + Duration::from_secs(5)));
        outbox.put(b"b\n".to_vec());
        assert!(outbox.flush(Instant::now() + Duration::from_secs(5)));
        assert_eq!(&*lock(&out), b"a\nb\n");
    }
}
