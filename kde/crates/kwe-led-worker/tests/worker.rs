// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end tests of the `kwe-led-worker` binary against the in-process
//! recording stub SDK server (ephemeral loopback ports) and synthetic frame
//! files written through `kwe-frame-protocol`. No real OpenRGB, no real
//! wallpaper payloads. Every wait is a bounded poll on a condition; rate
//! assertions divide by the measured elapsed time, so a slow machine can
//! only make counts smaller.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kwe_frame_protocol::{FrameSpec, SharedFrameWriter};
use kwe_input_protocol::{AudioFrame, encode_audio_frame};
use kwe_led_protocol::{
    ConfigPatch, FrameState, Mode, State, StatusLine, decode_status, device_id, encode_config,
    encode_frame, encode_status_request, encode_stop,
};
use kwe_openrgb::stub::{
    Fault, StubConfig, StubServer, sample_controller, sample_matrix_controller,
};
use kwe_openrgb::wire::packet_id;
use kwe_openrgb::{Controller, Rgb, zone_type};

const WAIT: Duration = Duration::from_secs(15);
/// The worker's hysteresis/silence timings scaled down (defaults: dwell 1 s,
/// minimum retake 5 s, silence 3 s) so the suite does not sleep through
/// them; a flag also given in `extra` is taken from there instead. The
/// defaults themselves run in the daemon tests and `scripts/smoke-led.sh`.
const SCALED_TIMINGS: &[&str] = &[
    "--takeover-dwell-ms",
    "150",
    "--min-retake-ms",
    "400",
    "--silence-release-ms",
    "600",
];
const POLL: Duration = Duration::from_millis(20);

fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        thread::sleep(POLL);
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("kwe-led-it-{label}-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A frame file with `f(x) -> rgb` per column.
fn write_frame(path: &Path, f: impl Fn(u32) -> [u8; 3]) -> SharedFrameWriter {
    let (w, h) = (64, 32);
    let mut writer = SharedFrameWriter::create(path, FrameSpec::new(w, h).unwrap()).unwrap();
    publish(&mut writer, f);
    writer
}

fn publish(writer: &mut SharedFrameWriter, f: impl Fn(u32) -> [u8; 3]) {
    let spec = writer.spec();
    let mut px = Vec::with_capacity(spec.pixel_bytes());
    for _ in 0..spec.height {
        for x in 0..spec.width {
            let [r, g, b] = f(x);
            px.extend_from_slice(&[b, g, r, 255]);
        }
    }
    writer.publish(&px).unwrap();
}

fn single_zone_controller(name: &str) -> Controller {
    let mut c = sample_controller(name, &[("Logo", 1)]);
    c.zones[0].kind = zone_type::SINGLE;
    c
}

fn id_of(c: &Controller) -> String {
    device_id(&c.name, &c.vendor, &c.serial, &c.location)
}

/// A port that nothing listens on and that the kernel will not hand to a
/// parallel test's `bind(0)`: below the ephemeral range (32768+ on Linux
/// defaults), spread by pid and `slot` so concurrent test binaries and
/// tests do not collide, and checked to be bindable.
fn quiet_port(slot: u32) -> u16 {
    let pid = std::process::id();
    for k in 0..200u32 {
        let p = 20_000 + (pid.wrapping_mul(31) + slot * 977 + k * 7919) % 12_000;
        let p = p as u16;
        if TcpListener::bind((Ipv4Addr::LOCALHOST, p)).is_ok() {
            return p;
        }
    }
    panic!("no quiet port found");
}

/// Binds a stub on a fixed port, retrying for a bounded time (a port can be
/// briefly busy, e.g. a concurrent bind probe in another test thread).
fn start_on_retry(port: u16, cfg: StubConfig) -> StubServer {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        match StubServer::start_on(port, cfg.clone()) {
            Ok(s) => return s,
            Err(e) if Instant::now() < end => {
                eprintln!("bind {port} retry: {e}");
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("bind {port}: {e}"),
        }
    }
}

fn scaled_timings(extra: &[&str]) -> Vec<&'static str> {
    SCALED_TIMINGS
        .chunks(2)
        .filter(|pair| !extra.contains(&pair[0]))
        .flatten()
        .copied()
        .collect()
}

struct Worker {
    child: Child,
    stdin: Option<ChildStdin>,
    statuses: Arc<Mutex<Vec<StatusLine>>>,
    bad_stdout: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<Vec<String>>>,
}

impl Worker {
    fn spawn(port: u16, extra: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kwe-led-worker"))
            .arg("--port")
            .arg(port.to_string())
            .args(["--backoff-min-ms", "100", "--backoff-max-ms", "400"])
            .args(scaled_timings(extra))
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn worker");
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let bad_stdout = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let out = child.stdout.take().unwrap();
        let (s, b) = (statuses.clone(), bad_stdout.clone());
        thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                match decode_status(line.as_bytes()) {
                    Ok(st) => s.lock().unwrap().push(st),
                    Err(_) => b.lock().unwrap().push(line),
                }
            }
        });
        let err = child.stderr.take().unwrap();
        let e = stderr.clone();
        thread::spawn(move || {
            for line in BufReader::new(err).lines() {
                let Ok(line) = line else { break };
                e.lock().unwrap().push(line);
            }
        });
        Self {
            stdin: child.stdin.take(),
            child,
            statuses,
            bad_stdout,
            stderr,
        }
    }

    fn send(&mut self, line: &[u8]) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(line).unwrap();
        stdin.flush().unwrap();
    }

    fn config(&mut self, patch: ConfigPatch) {
        self.send(&encode_config(&patch).unwrap());
    }

    fn last_status(&self) -> Option<StatusLine> {
        self.statuses.lock().unwrap().last().cloned()
    }

    fn wait_status(&mut self, what: &str, pred: impl Fn(&StatusLine) -> bool) -> StatusLine {
        let mut found = None;
        let ok = wait_until(WAIT, || {
            found = self
                .statuses
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|s| pred(s))
                .cloned();
            found.is_some()
        });
        assert!(
            ok,
            "timed out waiting for status: {what}; last = {:?}; stderr = {:?}",
            self.last_status(),
            self.stderr.lock().unwrap()
        );
        found.unwrap()
    }

    /// Waits for a status line newer than every line seen so far that
    /// satisfies `pred`.
    fn wait_new_status(&mut self, what: &str, pred: impl Fn(&StatusLine) -> bool) -> StatusLine {
        let seen = self.last_status().map_or(0, |s| s.seq);
        self.wait_status(what, |s| s.seq > seen && pred(s))
    }

    fn sigterm(&self) {
        let ok = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }

    fn close_stdin(&mut self) {
        self.stdin.take();
    }

    fn wait_exit(&mut self, timeout: Duration) -> ExitStatus {
        let end = Instant::now() + timeout;
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                // Let the reader threads drain the final lines.
                thread::sleep(Duration::from_millis(50));
                return s;
            }
            assert!(Instant::now() < end, "worker did not exit in {timeout:?}");
            thread::sleep(POLL);
        }
    }

    fn state_log_lines(&self) -> usize {
        self.stderr
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with("event=led.state"))
            .count()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn colors(stub: &StubServer, index: usize) -> Vec<Rgb> {
    stub.controllers()[index].colors.clone()
}

fn all_near(cs: &[Rgb], want: Rgb, tol: u8) -> bool {
    !cs.is_empty()
        && cs.iter().all(|c| {
            c.r.abs_diff(want.r) <= tol
                && c.g.abs_diff(want.g) <= tol
                && c.b.abs_diff(want.b) <= tol
        })
}

fn count(stub: &StubServer, dev: u32, id: u32) -> usize {
    stub.received()
        .iter()
        .filter(|p| p.dev_id == dev && p.id == id)
        .count()
}

fn audio_line(level: f32) -> Vec<u8> {
    let mut bands = vec![0.0f32; 16];
    bands[..2].fill(level);
    encode_audio_frame(&AudioFrame::new(1, bands.clone(), bands).unwrap()).unwrap()
}

const RED: Rgb = Rgb::new(255, 0, 0);
const BLUE: Rgb = Rgb::new(0, 0, 255);
const GREEN: Rgb = Rgb::new(0, 255, 0);
const BLACK: Rgb = Rgb::new(0, 0, 0);

// ---------------------------------------------------------------------------

#[test]
fn ambient_colours_reach_every_zone_type_and_eof_restores() {
    let stub = StubServer::start(StubConfig {
        controllers: vec![
            sample_controller("Strip", &[("A", 4)]),
            sample_matrix_controller("Keyboard"),
            single_zone_controller("Logo"),
        ],
        ..StubConfig::default()
    })
    .unwrap();
    let dir = temp_dir("ambient");
    let frame = dir.join("frame.bin");
    // Left half red, right half blue.
    let mut writer = write_frame(&frame, |x| if x < 32 { [255, 0, 0] } else { [0, 0, 255] });
    let mut w = Worker::spawn(stub.port(), &["--frame", frame.to_str().unwrap()]);

    let st = w.wait_status("active with 3 controlled devices", |s| {
        s.state == State::Active && s.devices.len() == 3 && s.devices.iter().all(|d| d.controlled)
    });
    assert_eq!(st.server.protocol, Some(5));
    assert_eq!(st.server.server_protocol, Some(5));
    assert_eq!(st.mode, Mode::Ambient);
    assert_eq!(st.frame.state, FrameState::Ok);
    assert_eq!((st.frame.width, st.frame.height), (Some(64), Some(32)));
    let kb = st.devices.iter().find(|d| d.name == "Keyboard").unwrap();
    assert_eq!(kb.zones[0].matrix, Some([2, 3]));

    // Linear: left LEDs red, right LEDs blue.
    assert!(wait_until(WAIT, || {
        let c = colors(&stub, 0);
        all_near(&c[..2], RED, 8) && all_near(&c[2..], BLUE, 8)
    }));
    // Matrix 2x3 (cells 0..5 row-major): column 0 red, column 2 blue.
    assert!(wait_until(WAIT, || {
        let c = colors(&stub, 1);
        all_near(&[c[0], c[3]], RED, 8) && all_near(&[c[2], c[5]], BLUE, 8)
    }));
    // Single: one blended colour, chroma-weighted between red and blue.
    assert!(wait_until(WAIT, || {
        let c = colors(&stub, 2)[0];
        c.r > 100 && c.b > 100 && c.g < 10
    }));

    // New frame → LEDs follow (temporal smoothing converges).
    publish(&mut writer, |_| [0, 255, 0]);
    assert!(wait_until(WAIT, || (0..3).all(|i| all_near(
        &colors(&stub, i),
        GREEN,
        8
    ))));

    // stdin EOF → restore previous modes and exit 0.
    w.close_stdin();
    assert!(w.wait_exit(Duration::from_secs(5)).success());
    for c in stub.controllers() {
        assert_eq!(c.active_mode, 2, "{} restored to Rainbow", c.name);
    }
    let last = w.last_status().unwrap();
    assert_eq!(last.state, State::Stopped);
    assert_eq!(last.reason.as_deref(), Some("stdin_eof"));
    let r = last.restore.unwrap();
    assert_eq!(
        (r.attempted, r.restored, r.failed, r.timed_out),
        (3, 3, 0, false)
    );
    assert!(
        w.bad_stdout.lock().unwrap().is_empty(),
        "stdout carries status lines only"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn spectrum_reacts_to_audio_lines_and_releases_without_them() {
    let stub = StubServer::start(StubConfig {
        controllers: vec![
            sample_controller("Strip", &[("A", 8)]),
            single_zone_controller("Logo"),
        ],
        ..StubConfig::default()
    })
    .unwrap();
    let mut w = Worker::spawn(stub.port(), &["--mode", "spectrum"]);
    let st = w.wait_status("waiting for audio", |s| {
        s.state == State::Active && s.devices.len() == 2
    });
    assert_eq!(st.reason.as_deref(), Some("waiting_for_source"));
    assert_eq!(count(&stub, 0, packet_id::SET_CUSTOM_MODE), 0);

    // Loud bass: the bass end of the strip and the single-zone pulse light.
    let lit = |stub: &StubServer| {
        let strip = colors(stub, 0);
        let logo = colors(stub, 1)[0];
        strip[0].b > 200 && strip[7] == BLACK && logo.r > 200
    };
    let ok = wait_until(WAIT, || {
        w.send(&audio_line(1.0));
        lit(&stub)
    });
    assert!(
        ok,
        "strip {:?} logo {:?}",
        colors(&stub, 0),
        colors(&stub, 1)
    );
    let st = w.wait_status("audio fresh, driving", |s| {
        s.audio.fresh && s.audio.bands == Some(16) && s.driven_count == 2
    });
    assert!(st.reason.is_none());

    // Audio stops → decays, then (after the hold) both devices are given
    // back to their own mode instead of being left dark.
    assert!(wait_until(WAIT, || stub
        .controllers()
        .iter()
        .all(|c| c.active_mode == 2)));
    let st = w.wait_new_status("released", |s| {
        s.reason.as_deref() == Some("waiting_for_source") && s.driven_count == 0
    });
    assert!(st.devices.iter().all(|d| !d.controlled));
}

#[test]
fn update_rate_is_bounded_and_changeable_at_runtime() {
    let stub = StubServer::start(StubConfig::default()).unwrap();
    let mut w = Worker::spawn(stub.port(), &["--mode", "spectrum", "--rate-hz", "20"]);
    w.wait_status("active", |s| {
        s.state == State::Active && s.devices.len() == 2
    });

    // Feed audio far faster than the tick rate, alternating levels so every
    // tick has new colours (the worker must coalesce, never queue).
    let feed = |w: &mut Worker, d: Duration| {
        let end = Instant::now() + d;
        let mut loud = false;
        while Instant::now() < end {
            loud = !loud;
            w.send(&audio_line(if loud { 1.0 } else { 0.0 }));
            thread::sleep(Duration::from_millis(5));
        }
    };
    let measure = |w: &mut Worker, window: Duration| -> (f64, usize) {
        feed(w, Duration::from_millis(300));
        let before =
            count(&stub, 0, packet_id::UPDATE_LEDS) + count(&stub, 1, packet_id::UPDATE_LEDS);
        let t0 = Instant::now();
        feed(w, window);
        let after =
            count(&stub, 0, packet_id::UPDATE_LEDS) + count(&stub, 1, packet_id::UPDATE_LEDS);
        (t0.elapsed().as_secs_f64(), after - before)
    };

    let (secs, n) = measure(&mut w, Duration::from_secs(2));
    // 2 devices × (20 Hz × elapsed + 1 boundary tick each side).
    let max = 2.0 * (20.0 * secs + 2.0);
    assert!((n as f64) <= max, "{n} updates in {secs:.2}s exceeds {max}");
    assert!(n >= 10, "updates should flow ({n})");

    w.config(ConfigPatch {
        rate_hz: Some(5),
        ..ConfigPatch::default()
    });
    w.wait_new_status("rate 5", |s| s.rate_hz == 5);
    let (secs, n) = measure(&mut w, Duration::from_secs(2));
    let max = 2.0 * (5.0 * secs + 2.0);
    assert!((n as f64) <= max, "{n} updates in {secs:.2}s exceeds {max}");
    assert!(n >= 4, "updates should flow ({n})");
    // Hard maximum: 21 is rejected (counted), the rate stays 5.
    w.send(
        br#"{"version":1,"type":"led_config","rate_hz":21}
"#,
    );
    w.send(&encode_status_request());
    let st = w.wait_new_status("rejection counted", |s| s.counters.stdin_rejected >= 1);
    assert_eq!(st.rate_hz, 5);
}

#[test]
fn disabled_device_gets_no_packets_and_runtime_toggle_restores() {
    let stub = StubServer::start(StubConfig::default()).unwrap();
    let keyboard = &stub.controllers()[0];
    let strip = &stub.controllers()[1];
    let (kb_id, strip_id) = (id_of(keyboard), id_of(strip));
    let dir = temp_dir("disable");
    let frame = dir.join("frame.bin");
    let _writer = write_frame(&frame, |_| [255, 0, 0]);
    let mut w = Worker::spawn(
        stub.port(),
        &[
            "--frame",
            frame.to_str().unwrap(),
            "--disable-device",
            &strip_id,
        ],
    );
    let st = w.wait_status("keyboard controlled", |s| {
        s.devices.iter().any(|d| d.id == kb_id && d.controlled)
    });
    let s = st.devices.iter().find(|d| d.id == strip_id).unwrap();
    assert!(!s.enabled && !s.controlled);
    assert!(wait_until(WAIT, || all_near(&colors(&stub, 0), RED, 8)));
    // Several more ticks, then: the strip never saw a device packet.
    thread::sleep(Duration::from_millis(500));
    for id in [
        packet_id::SET_CUSTOM_MODE,
        packet_id::UPDATE_LEDS,
        packet_id::UPDATE_ZONE_LEDS,
        packet_id::UPDATE_MODE,
    ] {
        assert_eq!(count(&stub, 1, id), 0, "strip got packet {id}");
    }
    assert_eq!(stub.controllers()[1].active_mode, 2);

    // Enable the strip, disable the keyboard — without respawn.
    w.config(ConfigPatch {
        disabled_devices: Some(vec![kb_id.clone()]),
        ..ConfigPatch::default()
    });
    assert!(wait_until(WAIT, || all_near(&colors(&stub, 1), RED, 8)));
    assert!(
        wait_until(WAIT, || stub.controllers()[0].active_mode == 2),
        "keyboard restored"
    );
    w.wait_new_status("toggled", |s| {
        s.devices
            .iter()
            .any(|d| d.id == kb_id && !d.enabled && !d.controlled)
            && s.devices
                .iter()
                .any(|d| d.id == strip_id && d.enabled && d.controlled)
    });
    let kb_updates = count(&stub, 0, packet_id::UPDATE_LEDS);
    thread::sleep(Duration::from_millis(600));
    assert_eq!(
        count(&stub, 0, packet_id::UPDATE_LEDS),
        kb_updates,
        "disabled keyboard left alone"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn sigterm_restores_every_device_and_exits_zero() {
    let stub = StubServer::start(StubConfig::default()).unwrap();
    let mut w = Worker::spawn(stub.port(), &["--mode", "spectrum", "--synthetic-audio"]);
    w.wait_status("controlled", |s| {
        s.devices.len() == 2 && s.devices.iter().all(|d| d.controlled)
    });
    assert!(
        stub.controllers().iter().all(|c| c.active_mode == 0),
        "in Direct"
    );
    let modes_before =
        count(&stub, 0, packet_id::UPDATE_MODE) + count(&stub, 1, packet_id::UPDATE_MODE);
    assert_eq!(modes_before, 0);
    let t0 = Instant::now();
    w.sigterm();
    assert!(w.wait_exit(Duration::from_secs(5)).success());
    assert!(
        t0.elapsed() < Duration::from_secs(3),
        "stop took {:?}",
        t0.elapsed()
    );
    assert_eq!(count(&stub, 0, packet_id::UPDATE_MODE), 1);
    assert_eq!(count(&stub, 1, packet_id::UPDATE_MODE), 1);
    // The restore packet echoes the saved mode index 2 (Rainbow).
    let p = stub
        .received()
        .into_iter()
        .find(|p| p.id == packet_id::UPDATE_MODE)
        .unwrap();
    assert_eq!(&p.payload[4..8], &2i32.to_le_bytes());
    assert!(stub.controllers().iter().all(|c| c.active_mode == 2));
    let last = w.last_status().unwrap();
    assert_eq!(
        (last.state, last.reason.as_deref()),
        (State::Stopped, Some("signal"))
    );
}

#[test]
fn led_stop_and_mode_off_restore_but_off_keeps_running() {
    let stub = StubServer::start(StubConfig::default()).unwrap();
    let mut w = Worker::spawn(stub.port(), &["--mode", "spectrum", "--synthetic-audio"]);
    w.wait_status("controlled", |s| {
        s.devices.iter().all(|d| d.controlled) && s.devices.len() == 2
    });
    w.config(ConfigPatch {
        mode: Some(Mode::Off),
        ..ConfigPatch::default()
    });
    let st = w.wait_new_status("off", |s| s.state == State::Off);
    assert!(st.devices.iter().all(|d| !d.controlled));
    assert!(wait_until(WAIT, || stub
        .controllers()
        .iter()
        .all(|c| c.active_mode == 2)));
    let updates = count(&stub, 0, packet_id::UPDATE_LEDS);
    thread::sleep(Duration::from_millis(500));
    assert_eq!(
        count(&stub, 0, packet_id::UPDATE_LEDS),
        updates,
        "off sends nothing"
    );
    assert!(w.child.try_wait().unwrap().is_none(), "still alive");

    w.config(ConfigPatch {
        mode: Some(Mode::Spectrum),
        ..ConfigPatch::default()
    });
    w.wait_new_status("active again", |s| {
        s.state == State::Active && s.devices.iter().all(|d| d.controlled)
    });
    w.send(&encode_stop());
    assert!(w.wait_exit(Duration::from_secs(5)).success());
    assert!(stub.controllers().iter().all(|c| c.active_mode == 2));
    assert_eq!(w.last_status().unwrap().reason.as_deref(), Some("led_stop"));
}

#[test]
fn server_absent_is_quiet_then_appears() {
    let port = quiet_port(1);
    let mut w = Worker::spawn(port, &["--mode", "spectrum", "--synthetic-audio"]);
    let st = w.wait_status("server_absent", |s| s.state == State::ServerAbsent);
    assert!(!st.server.reachable && st.reason.is_none());
    // Several backoff periods (100..400 ms): still one state log line, no
    // error exit.
    thread::sleep(Duration::from_millis(1500));
    assert!(w.child.try_wait().unwrap().is_none());
    assert_eq!(w.state_log_lines(), 1, "{:?}", w.stderr.lock().unwrap());
    let st = w.last_status().unwrap();
    assert_eq!(st.state, State::ServerAbsent);
    assert!(st.server.next_probe_ms.unwrap() <= 400);

    let stub = start_on_retry(port, StubConfig::default());
    w.wait_status("active", |s| {
        s.state == State::Active && s.devices.len() == 2
    });
    assert!(wait_until(WAIT, || stub
        .controllers()
        .iter()
        .all(|c| c.active_mode == 0)));
    w.close_stdin();
    assert!(w.wait_exit(Duration::from_secs(5)).success());
    assert!(stub.controllers().iter().all(|c| c.active_mode == 2));
}

#[test]
fn server_dying_mid_run_then_returning_keeps_original_modes() {
    let stub = StubServer::start(StubConfig::default()).unwrap();
    let port = stub.port();
    let mut w = Worker::spawn(port, &["--mode", "spectrum", "--synthetic-audio"]);
    w.wait_status("controlled", |s| {
        s.devices.len() == 2 && s.devices.iter().all(|d| d.controlled)
    });
    drop(stub);
    let st = w.wait_new_status("connection lost", |s| {
        matches!(s.state, State::Degraded | State::ServerAbsent)
    });
    assert!(st.devices.is_empty());
    w.wait_status("quiet absent", |s| s.state == State::ServerAbsent);
    assert!(w.child.try_wait().unwrap().is_none(), "no error exit");

    // The server returns with the devices still in our Direct mode (the
    // server kept its state): the restore must use the ORIGINAL modes
    // carried across the reconnect, not re-save Direct.
    let mut controllers = StubConfig::default().controllers;
    for c in &mut controllers {
        c.active_mode = 0;
    }
    let stub = start_on_retry(
        port,
        StubConfig {
            controllers,
            ..StubConfig::default()
        },
    );
    w.wait_new_status("active again", |s| {
        s.state == State::Active && s.devices.len() == 2 && s.devices.iter().all(|d| d.controlled)
    });
    let st = w.last_status().unwrap();
    assert!(
        st.counters.connects >= 2 && st.counters.disconnects >= 1,
        "{:?}",
        st.counters
    );
    w.sigterm();
    assert!(w.wait_exit(Duration::from_secs(5)).success());
    assert!(
        stub.controllers().iter().all(|c| c.active_mode == 2),
        "originals restored"
    );
}

#[test]
fn protocol_error_is_degraded_and_recovers() {
    let stub = StubServer::start(StubConfig {
        fault: Fault::Garbage,
        ..StubConfig::default()
    })
    .unwrap();
    let mut w = Worker::spawn(stub.port(), &["--mode", "spectrum"]);
    let st = w.wait_status("degraded", |s| s.state == State::Degraded);
    assert_eq!(st.reason.as_deref(), Some("protocol_error"));
    assert!(w.child.try_wait().unwrap().is_none());
    stub.set_fault(Fault::None);
    w.wait_status("recovered", |s| {
        s.state == State::Active && s.devices.len() == 2
    });
}

#[test]
fn stalled_server_times_out_into_degraded() {
    let stub = StubServer::start(StubConfig {
        fault: Fault::Stall(Duration::from_secs(3)),
        ..StubConfig::default()
    })
    .unwrap();
    let mut w = Worker::spawn(stub.port(), &["--mode", "spectrum"]);
    let st = w.wait_status("degraded timeout", |s| s.state == State::Degraded);
    assert_eq!(st.reason.as_deref(), Some("timeout"));
    // A stop during a stalled server is still bounded.
    let t0 = Instant::now();
    w.sigterm();
    assert!(w.wait_exit(Duration::from_secs(6)).success());
    eprintln!("stalled-server stop took {:?}", t0.elapsed());
    assert!(
        t0.elapsed() < Duration::from_secs(4),
        "stop took {:?}",
        t0.elapsed()
    );
}

#[test]
fn frame_missing_replaced_truncated_and_switched() {
    let stub = StubServer::start(StubConfig {
        controllers: vec![sample_controller("Strip", &[("A", 4)])],
        ..StubConfig::default()
    })
    .unwrap();
    let dir = temp_dir("frames");
    let a = dir.join("a.bin");
    let mut w = Worker::spawn(stub.port(), &["--frame", a.to_str().unwrap()]);
    let st = w.wait_status("active, frame absent", |s| {
        s.state == State::Active && s.frame.state == FrameState::Absent
    });
    assert!(!st.devices[0].controlled, "no frame, device left alone");
    assert_eq!(st.reason.as_deref(), Some("waiting_for_source"));

    // Appears.
    let writer = write_frame(&a, |_| [255, 0, 0]);
    assert!(wait_until(WAIT, || all_near(&colors(&stub, 0), RED, 8)));
    w.wait_status("frame ok", |s| s.frame.state == FrameState::Ok);

    // Removed (renderer replaced): held, then released to the device's own
    // mode — never faded to black.
    drop(writer);
    std::fs::remove_file(&a).unwrap();
    w.wait_new_status("frame absent", |s| s.frame.state == FrameState::Absent);
    assert!(wait_until(WAIT, || stub.controllers()[0].active_mode == 2));
    assert!(all_near(&colors(&stub, 0), RED, 8), "held colours, no fade");

    // Replaced by a new file at the same path (new inode, generation 1).
    let writer = write_frame(&a, |_| [0, 255, 0]);
    assert!(wait_until(WAIT, || all_near(&colors(&stub, 0), GREEN, 8)));
    assert_eq!(stub.controllers()[0].active_mode, 0, "taken again");

    // Truncated under the worker: invalid, worker alive.
    let f = std::fs::OpenOptions::new().write(true).open(&a).unwrap();
    f.set_len(100).unwrap();
    w.wait_new_status("frame invalid", |s| s.frame.state == FrameState::Invalid);
    assert!(w.child.try_wait().unwrap().is_none());
    drop(writer);

    // Switched at runtime to another renderer's file.
    let b = dir.join("b.bin");
    let _wb = write_frame(&b, |_| [0, 0, 255]);
    w.send(&encode_frame(Some(&b)).unwrap());
    assert!(wait_until(WAIT, || all_near(&colors(&stub, 0), BLUE, 8)));
    // Cleared: no renderer live → released after the hold.
    w.send(&encode_frame(None).unwrap());
    w.wait_new_status("frame none", |s| s.frame.state == FrameState::None);
    assert!(wait_until(WAIT, || stub.controllers()[0].active_mode == 2));
    // The worker never held a descriptor on the frame files.
    let fds = std::fs::read_dir(format!("/proc/{}/fd", w.child.id())).unwrap();
    for fd in fds.flatten() {
        if let Ok(target) = std::fs::read_link(fd.path()) {
            assert!(!target.starts_with(&dir), "worker holds {target:?}");
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn oversized_garbage_and_unknown_stdin_lines_are_counted_not_fatal() {
    let stub = StubServer::start(StubConfig::default()).unwrap();
    let dir = temp_dir("garbage");
    let frame = dir.join("f.bin");
    let _writer = write_frame(&frame, |_| [255, 255, 255]);
    let mut w = Worker::spawn(stub.port(), &["--frame", frame.to_str().unwrap()]);
    w.wait_status("active", |s| s.state == State::Active);
    assert!(wait_until(WAIT, || all_near(
        &colors(&stub, 0),
        Rgb::new(255, 255, 255),
        8
    )));

    let mut huge = vec![b'{'; 1 << 20];
    huge.push(b'\n');
    w.send(&huge);
    w.send(b"not json\n");
    w.send(&[0xff, 0xfe, 0x00, 0x80, b'\n']);
    w.send(b"{\"version\":1,\"type\":\"led_config\",\"brightness\":\"max\"}\n");
    w.send(b"{\"version\":9,\"type\":\"led_config\"}\n");
    w.send(b"{\"version\":1,\"type\":\"pointer_position\",\"sequence\":1,\"phase\":\"move\",\"x\":1,\"y\":1}\n");
    w.send(b"{\"version\":1,\"type\":\"led_frame\",\"path\":\"../../etc/passwd\"}\n");
    w.send(&encode_status_request());
    let st = w.wait_new_status("counted", |s| s.counters.stdin_rejected >= 6);
    assert_eq!(st.counters.stdin_rejected, 6);
    assert_eq!(st.counters.stdin_ignored, 1);
    assert_eq!(
        st.frame.state,
        FrameState::Ok,
        "bad path did not replace the frame"
    );

    // Still responsive: brightness 0 → black.
    w.config(ConfigPatch {
        brightness: Some(0),
        ..ConfigPatch::default()
    });
    assert!(wait_until(WAIT, || all_near(&colors(&stub, 0), BLACK, 0)));
    let log = w.stderr.lock().unwrap().clone();
    assert!(log.len() <= 25, "bounded log: {log:?}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn probe_reports_inventory_or_absence() {
    let stub = StubServer::start(StubConfig::default()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_kwe-led-worker"))
        .args(["--probe", "--port", &stub.port().to_string()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1);
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["type"], "led_probe");
    assert_eq!(v["negotiated_protocol"], 5);
    assert_eq!(v["device_count"], 2);
    assert_eq!(v["devices"][0]["id"], id_of(&stub.controllers()[0]));
    assert_eq!(v["devices"][1]["zones"][1]["type"], "linear");
    // Read-only: no device packets.
    for id in [
        packet_id::SET_CUSTOM_MODE,
        packet_id::UPDATE_LEDS,
        packet_id::UPDATE_MODE,
    ] {
        assert!(!stub.received_ids().contains(&id));
    }

    let out = Command::new(env!("CARGO_BIN_EXE_kwe-led-worker"))
        .args(["--probe", "--port", &quiet_port(3).to_string()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        (v["status"].as_str(), v["reachable"].as_bool()),
        (Some("server_absent"), Some(false))
    );

    let out = Command::new(env!("CARGO_BIN_EXE_kwe-led-worker"))
        .args(["--rate-hz", "21"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "usage error");
}

/// CPU ticks (utime + stime) of a live process.
fn cpu_ticks(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let after_comm = &stat[stat.rfind(')').unwrap() + 2..];
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // Fields 14 and 15 of stat(5); index 11/12 after the comm field.
    fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
}

#[test]
fn standalone_with_closed_stdin_idles_instead_of_spinning() {
    let stub = StubServer::start(StubConfig::default()).unwrap();
    let mut w = Worker::spawn(
        stub.port(),
        &["--standalone", "--synthetic-audio", "--mode", "spectrum"],
    );
    w.close_stdin();
    w.wait_status("active", |s| s.state == State::Active);
    let pid = w.child.id();
    let (t0, c0) = (Instant::now(), cpu_ticks(pid));
    thread::sleep(Duration::from_secs(2));
    let (wall, used) = (t0.elapsed().as_secs_f64(), cpu_ticks(pid) - c0);
    // CLK_TCK is 100 on Linux; a spinning loop burns ~wall × 100 ticks. The
    // bound is a quarter of one core, far above the measured idle cost.
    assert!(
        (used as f64) < wall * 100.0 * 0.25,
        "{used} ticks in {wall:.2}s"
    );
    assert!(
        w.child.try_wait().unwrap().is_none(),
        "EOF does not stop standalone"
    );
    w.sigterm();
    assert!(w.wait_exit(Duration::from_secs(5)).success());
}

// ---------------------------------------------------------------------------
// Review fixes A
// ---------------------------------------------------------------------------

fn untouched(stub: &StubServer) -> bool {
    [
        packet_id::SET_CUSTOM_MODE,
        packet_id::UPDATE_LEDS,
        packet_id::UPDATE_ZONE_LEDS,
        packet_id::UPDATE_MODE,
    ]
    .iter()
    .all(|id| !stub.received_ids().contains(id))
        && stub.controllers().iter().all(|c| c.active_mode == 2)
}

/// MUST-FIX 1: connected with nothing to show (no wallpaper promoted, or
/// spectrum without audio) never touches a device.
#[test]
fn no_source_never_takes_or_blanks_devices() {
    for mode in ["ambient", "ambient+pulse", "spectrum"] {
        let stub = StubServer::start(StubConfig::default()).unwrap();
        let mut w = Worker::spawn(stub.port(), &["--mode", mode]);
        w.send(&encode_frame(None).unwrap());
        w.wait_status("connected", |s| {
            s.state == State::Active && s.devices.len() == 2
        });
        thread::sleep(Duration::from_millis(1500));
        assert!(untouched(&stub), "{mode}: {:?}", stub.received_ids());
        w.send(&encode_status_request());
        let st = w.wait_new_status("status", |_| true);
        assert_eq!(st.reason.as_deref(), Some("waiting_for_source"), "{mode}");
        assert_eq!(st.driven_count, 0);
        assert!(st.devices.iter().all(|d| !d.controlled));
        w.close_stdin();
        assert!(w.wait_exit(Duration::from_secs(5)).success());
        assert!(untouched(&stub), "{mode}: stop sent nothing either");
        let r = w.last_status().unwrap().restore.unwrap();
        assert_eq!(r.attempted, 0);
    }
}

/// MUST-FIX 1: a source that appears is driven (exact driven_count), and
/// ambient+pulse with a frame but no audio still shows steady ambient.
#[test]
fn source_appearing_is_driven_and_counted() {
    let stub = StubServer::start(StubConfig::default()).unwrap();
    let dir = temp_dir("appear");
    let frame = dir.join("f.bin");
    let mut w = Worker::spawn(stub.port(), &["--mode", "ambient+pulse"]);
    w.wait_status("waiting", |s| {
        s.reason.as_deref() == Some("waiting_for_source")
    });
    let _writer = write_frame(&frame, |_| [255, 0, 0]);
    w.send(&encode_frame(Some(&frame)).unwrap());
    assert!(wait_until(WAIT, || (0..2).all(|i| all_near(
        &colors(&stub, i),
        RED,
        8
    ))));
    let st = w.wait_new_status("driving", |s| s.driven_count == 2);
    assert!(st.reason.is_none() && !st.audio.fresh);
    std::fs::remove_dir_all(dir).unwrap();
}

/// MUST-FIX 2: SIGKILL leaves the devices in Direct; the next worker with
/// the same `--saved-modes` file restores the user's ORIGINAL modes (not
/// Direct) and removes the file after the clean restore.
#[test]
fn unclean_death_then_next_worker_restores_the_originals() {
    let stub = StubServer::start(StubConfig::default()).unwrap();
    let dir = temp_dir("saved");
    let saved = dir.join("led-saved-modes.json");
    let args = [
        "--mode",
        "spectrum",
        "--synthetic-audio",
        "--saved-modes",
        saved.to_str().unwrap(),
    ];
    let mut a = Worker::spawn(stub.port(), &args);
    a.wait_status("controlled", |s| {
        s.devices.len() == 2 && s.devices.iter().all(|d| d.controlled)
    });
    let text = std::fs::read_to_string(&saved).expect("written before take-over");
    assert!(text.contains("\"Rainbow\""), "{text}");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&saved).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    a.child.kill().unwrap();
    a.child.wait().unwrap();
    assert!(stub.controllers().iter().all(|c| c.active_mode == 0));

    let mut b = Worker::spawn(stub.port(), &args);
    b.wait_status("controlled again", |s| {
        s.devices.len() == 2 && s.devices.iter().all(|d| d.controlled)
    });
    b.sigterm();
    assert!(b.wait_exit(Duration::from_secs(5)).success());
    assert!(
        stub.controllers().iter().all(|c| c.active_mode == 2),
        "originals restored: {:?}",
        stub.controllers()
            .iter()
            .map(|c| c.active_mode)
            .collect::<Vec<_>>()
    );
    assert!(!saved.exists(), "removed after a full restore");
    std::fs::remove_dir_all(dir).unwrap();
}

/// MUST-FIX 2: an adopted file is honoured even when no source ever
/// arrives (the devices are released at once), and a corrupt file is
/// ignored with one log line, never fatal.
#[test]
fn saved_modes_file_is_adopted_without_a_source_and_corrupt_is_ignored() {
    let stub = StubServer::start(StubConfig::default()).unwrap();
    let dir = temp_dir("saved2");
    let saved = dir.join("s.json");
    let args = [
        "--mode",
        "spectrum",
        "--synthetic-audio",
        "--saved-modes",
        saved.to_str().unwrap(),
    ];
    let mut a = Worker::spawn(stub.port(), &args);
    a.wait_status("controlled", |s| {
        s.devices.len() == 2 && s.devices.iter().all(|d| d.controlled)
    });
    a.child.kill().unwrap();
    a.child.wait().unwrap();
    assert!(stub.controllers().iter().all(|c| c.active_mode == 0));
    // Next worker: ambient, no frame → never drives, but gives back.
    let mut b = Worker::spawn(
        stub.port(),
        &[
            "--mode",
            "ambient",
            "--saved-modes",
            saved.to_str().unwrap(),
        ],
    );
    assert!(wait_until(WAIT, || stub
        .controllers()
        .iter()
        .all(|c| c.active_mode == 2)));
    assert!(wait_until(WAIT, || !saved.exists()));
    b.close_stdin();
    assert!(b.wait_exit(Duration::from_secs(5)).success());

    std::fs::write(&saved, b"{garbage").unwrap();
    let mut c = Worker::spawn(
        stub.port(),
        &[
            "--mode",
            "ambient",
            "--saved-modes",
            saved.to_str().unwrap(),
        ],
    );
    c.wait_status("running", |s| s.state == State::Active);
    assert!(
        !saved.exists(),
        "corrupt file removed (replaced on next write)"
    );
    let lines: Vec<String> = c
        .stderr
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.contains("led.saved_modes"))
        .cloned()
        .collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("reason=not_json"), "{lines:?}");
    std::fs::remove_dir_all(dir).unwrap();
}

/// R1: a slow-but-alive server (each post-switch re-read takes 400 ms, under
/// the 500 ms request timeout) with many devices: SIGTERM still ends the
/// worker well inside the daemon's 4 s grace, with the restore attempted.
#[test]
fn sigterm_during_slow_take_over_of_many_devices_is_bounded() {
    let controllers: Vec<Controller> = (0..24)
        .map(|i| sample_controller(&format!("Dev {i}"), &[("A", 2)]))
        .collect();
    let stub = StubServer::start(StubConfig {
        controllers,
        fault: Fault::StallAfterCustomMode(Duration::from_millis(400)),
        ..StubConfig::default()
    })
    .unwrap();
    let mut w = Worker::spawn(stub.port(), &["--mode", "spectrum", "--synthetic-audio"]);
    assert!(wait_until(WAIT, || stub
        .received_ids()
        .contains(&packet_id::SET_CUSTOM_MODE)));
    let t0 = Instant::now();
    w.sigterm();
    assert!(w.wait_exit(Duration::from_secs(20)).success());
    let took = t0.elapsed();
    eprintln!("slow take-over stop took {took:?}");
    // Documented worst case 3.35 s (LED_PROTOCOL_V1.md); this path is
    // one in-flight 400 ms re-read + a fire-and-forget restore.
    assert!(took < Duration::from_millis(3350), "stop took {took:?}");
    let last = w.last_status().unwrap();
    let r = last.restore.expect("restore summary");
    assert!(r.attempted >= 1 && r.restored == r.attempted, "{r:?}");
    assert!(!r.timed_out);
    assert_eq!(stub.controllers()[0].active_mode, 2, "first device back");
}

fn voluntary_switches(pid: u32) -> u64 {
    let mut total = 0;
    for task in std::fs::read_dir(format!("/proc/{pid}/task"))
        .unwrap()
        .flatten()
    {
        if let Ok(text) = std::fs::read_to_string(task.path().join("status")) {
            for line in text.lines() {
                if let Some(v) = line.strip_prefix("voluntary_ctxt_switches:") {
                    total += v.trim().parse::<u64>().unwrap_or(0);
                }
            }
        }
    }
    total
}

/// R3: with no OpenRGB the worker sleeps until its next probe or a control
/// line (no 50 ms polling, no frame reads), yet still answers SIGTERM at
/// once.
#[test]
fn idle_without_openrgb_is_quiet_and_still_stops_promptly() {
    let port = quiet_port(7);
    let dir = temp_dir("idle");
    let frame = dir.join("f.bin");
    let _writer = write_frame(&frame, |_| [255, 0, 0]);
    let mut child = Command::new(env!("CARGO_BIN_EXE_kwe-led-worker"))
        .args([
            "--port",
            &port.to_string(),
            "--frame",
            frame.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(500));
    let pid = child.id();
    let before = voluntary_switches(pid);
    thread::sleep(Duration::from_secs(4));
    let used = voluntary_switches(pid) - before;
    eprintln!("idle (no server) voluntary context switches in 4 s: {used}");
    // Old loop: ~80 in 4 s (50 ms wake + per-tick frame open). Now: the
    // probes (1 s, 2 s, 4 s backoff) and one 5 s heartbeat.
    assert!(used <= 30, "{used} switches in 4 s");
    let t0 = Instant::now();
    // SAFETY: our own child.
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    let status = child.wait().unwrap();
    assert!(status.success());
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "{:?}",
        t0.elapsed()
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// NIT: a device name with a newline cannot forge a log line.
#[test]
fn server_strings_cannot_inject_log_lines() {
    let mut evil = sample_controller("Evil\nevent=led.fatal detail=forged", &[("A", 1)]);
    evil.modes.truncate(2); // Rainbow saved index 2 then vanishes below
    evil.active_mode = 1;
    let stub = StubServer::start(StubConfig {
        controllers: vec![evil],
        ..StubConfig::default()
    })
    .unwrap();
    let mut w = Worker::spawn(stub.port(), &["--mode", "spectrum", "--synthetic-audio"]);
    w.wait_status("controlled", |s| {
        s.devices.len() == 1 && s.devices.iter().all(|d| d.controlled)
    });
    let mut list = stub.controllers();
    list[0].modes.truncate(1); // remembered Static vanished → restore error logged
    stub.set_controllers(list);
    stub.notify_device_list_updated();
    w.config(ConfigPatch {
        mode: Some(Mode::Off),
        ..ConfigPatch::default()
    });
    assert!(wait_until(WAIT, || w
        .stderr
        .lock()
        .unwrap()
        .iter()
        .any(|l| l.contains("op=restore"))));
    let log = w.stderr.lock().unwrap().clone();
    assert!(log.iter().all(|l| l.starts_with("event=led.")), "{log:?}");
    assert!(
        !log.iter().any(|l| l.starts_with("event=led.fatal")),
        "{log:?}"
    );
}

// ---------------------------------------------------------------------------
// Review fixes C1
// ---------------------------------------------------------------------------

/// Worker A drives both stub devices, then is SIGKILLed (leaving them in
/// Direct and the saved-modes file behind).
fn killed_while_driving(stub: &StubServer, saved: &Path) {
    let mut a = Worker::spawn(
        stub.port(),
        &[
            "--mode",
            "spectrum",
            "--synthetic-audio",
            "--saved-modes",
            saved.to_str().unwrap(),
        ],
    );
    a.wait_status("controlled", |s| {
        s.devices.len() == 2 && s.devices.iter().all(|d| d.controlled)
    });
    a.child.kill().unwrap();
    a.child.wait().unwrap();
    assert!(stub.controllers().iter().all(|c| c.active_mode == 0));
    assert!(saved.exists());
}

fn device_packets_since(stub: &StubServer, from: usize) -> Vec<u32> {
    stub.received()[from..]
        .iter()
        .map(|p| p.id)
        .filter(|id| {
            [
                packet_id::SET_CUSTOM_MODE,
                packet_id::UPDATE_LEDS,
                packet_id::UPDATE_ZONE_LEDS,
                packet_id::UPDATE_MODE,
            ]
            .contains(id)
        })
        .collect()
}

/// N2: leftovers from a worker that has not been alive for longer than the
/// freshness window (or from an earlier boot) never overwrite the user's
/// new lighting, even when that lighting uses Direct like ours.
#[test]
fn stale_or_foreign_boot_leftovers_leave_new_direct_lighting_alone() {
    for case in ["stale", "earlier_boot"] {
        let stub = StubServer::start(StubConfig::default()).unwrap();
        let dir = temp_dir(case);
        let saved = dir.join("led-saved-modes.json");
        killed_while_driving(&stub, &saved);
        // The user applies their own Direct lighting (an OpenRGB profile).
        let magenta = Rgb::new(255, 0, 255);
        let mut list = stub.controllers();
        for c in &mut list {
            c.active_mode = 0;
            c.colors.fill(magenta);
        }
        stub.set_controllers(list);
        match case {
            "stale" => {
                let old = SystemTime::now() - Duration::from_secs(120);
                std::fs::File::options()
                    .write(true)
                    .open(&saved)
                    .unwrap()
                    .set_modified(old)
                    .unwrap();
            }
            _ => {
                let text = std::fs::read_to_string(&saved).unwrap();
                let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                let ours = v["boot_id"].as_str().expect("boot id recorded").to_string();
                std::fs::write(
                    &saved,
                    text.replace(&ours, "00000000-0000-0000-0000-000000000000"),
                )
                .unwrap();
            }
        }
        let from = stub.received().len();
        let mut b = Worker::spawn(
            stub.port(),
            &[
                "--mode",
                "spectrum",
                "--saved-modes",
                saved.to_str().unwrap(),
            ],
        );
        b.wait_status("connected", |s| {
            s.state == State::Active && s.devices.len() == 2
        });
        thread::sleep(Duration::from_millis(1000));
        assert_eq!(
            device_packets_since(&stub, from),
            Vec::<u32>::new(),
            "{case}"
        );
        assert!(
            stub.controllers()
                .iter()
                .all(|c| c.active_mode == 0 && c.colors.iter().all(|k| *k == magenta)),
            "{case}: the user's lighting is untouched"
        );
        assert!(!saved.exists(), "{case}: forgotten file removed");
        let log = b.stderr.lock().unwrap().clone();
        assert!(
            log.iter()
                .any(|l| l.contains("detail=forgotten") && l.contains(&format!("reason={case}"))),
            "{case}: {log:?}"
        );
        b.close_stdin();
        assert!(b.wait_exit(Duration::from_secs(5)).success());
        assert_eq!(device_packets_since(&stub, from), Vec::<u32>::new());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// N3 (the review's `kill_absent`): a device missing from OpenRGB's list
/// when the successor starts is not forgotten — it is restored when it is
/// listed again, and the file is removed only after that.
#[test]
fn a_device_missing_when_the_successor_starts_is_restored_when_back() {
    let stub = StubServer::start(StubConfig::default()).unwrap();
    let dir = temp_dir("absent");
    let saved = dir.join("led-saved-modes.json");
    killed_while_driving(&stub, &saved);
    let full = stub.controllers();
    stub.set_controllers(vec![full[0].clone()]);
    let mut b = Worker::spawn(
        stub.port(),
        &[
            "--mode",
            "spectrum",
            "--saved-modes",
            saved.to_str().unwrap(),
        ],
    );
    assert!(wait_until(WAIT, || stub.controllers()[0].active_mode == 2));
    assert!(wait_until(WAIT, || std::fs::read_to_string(&saved)
        .is_ok_and(|t| t.matches("\"occurrence\"").count() == 1)));
    // Rescan finished: the strip is back, still in Direct.
    let mut back = stub.controllers();
    back.push(full[1].clone());
    stub.set_controllers(back);
    stub.notify_device_list_updated();
    assert!(
        wait_until(WAIT, || stub.controllers()[1].active_mode == 2),
        "strip restored: {:?}",
        stub.controllers()
            .iter()
            .map(|c| c.active_mode)
            .collect::<Vec<_>>()
    );
    assert!(wait_until(WAIT, || !saved.exists()));
    b.sigterm();
    assert!(b.wait_exit(Duration::from_secs(5)).success());
    std::fs::remove_dir_all(dir).unwrap();
}

/// R4 end to end: an audio source flapping like the review's reproducer
/// (0.6 s bursts, 3 s gaps) costs a bounded number of takeovers. Real
/// minimum retake (5 s), scaled dwell.
#[test]
fn flapping_audio_is_bounded_end_to_end() {
    let stub = StubServer::start(StubConfig::default()).unwrap();
    let mut w = Worker::spawn(
        stub.port(),
        &["--mode", "spectrum", "--min-retake-ms", "5000"],
    );
    w.wait_status("connected", |s| {
        s.state == State::Active && s.devices.len() == 2
    });
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(12) {
        let burst = Instant::now();
        while burst.elapsed() < Duration::from_millis(600) {
            w.send(&audio_line(1.0));
            thread::sleep(Duration::from_millis(30));
        }
        thread::sleep(Duration::from_millis(3000));
    }
    let secs = t0.elapsed().as_secs_f64();
    let takes = count(&stub, 0, packet_id::SET_CUSTOM_MODE);
    eprintln!("flap {secs:.1}s: {takes} takeovers of device 0");
    // A release comes ≥ 2.5 s after a burst's last frame and the next
    // takeover ≥ 5 s after that: ≤ 1 + secs / 7. Without hysteresis: one
    // per burst (4 here).
    let bound = 1 + (secs / 7.0) as usize;
    assert!(takes >= 1 && takes <= bound, "{takes} > {bound}");
}
