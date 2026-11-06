// SPDX-License-Identifier: GPL-3.0-or-later
//! L3 lifecycle tests: fake shell workers for the daemon-side contract and,
//! when it is built, the real `kwe-led-worker` against the in-process
//! recording stub SDK server on an ephemeral loopback port. Nothing here
//! touches port 6742 or a real OpenRGB.

use super::*;
use std::{
    fs,
    net::{Ipv4Addr, TcpListener},
    os::unix::fs::PermissionsExt,
    thread,
};

use kwe_frame_protocol::{FrameSpec, SharedFrameWriter};
use kwe_input_protocol::{AudioFrame, encode_audio_frame};
use kwe_openrgb::stub::{StubConfig, StubServer};
use kwe_openrgb::wire::packet_id;

const WAIT: Duration = Duration::from_secs(15);

pub(crate) fn temporary_directory(label: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "kwe-led-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::create_dir_all(&directory).unwrap();
    directory
}

/// A `/bin/sh` fake worker. The environment is cleared at spawn, so the
/// script finds its side files through `$0`.
pub(crate) fn fake_worker(root: &Path, body: &str) -> PathBuf {
    let script = root.join("led-worker");
    fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// Records argv to `$0.argv` and every stdin line to `$0.stdin`.
pub(crate) const RECORDING_BODY: &str =
    "printf '%s\\n' \"$@\" > \"$0.argv\"\nexec /bin/cat > \"$0.stdin\"";

pub(crate) fn status_json(state: &str, mode: &str) -> String {
    format!(
        concat!(
            r#"{{"version":1,"type":"led_status","seq":1,"state":"{state}","reason":null,"#,
            r#""mode":"{mode}","brightness":100,"rate_hz":10,"#,
            r#""server":{{"port":6742,"reachable":false,"protocol":null,"server_protocol":null,"next_probe_ms":1000}},"#,
            r#""frame":{{"state":"none","sequence":null,"width":null,"height":null}},"#,
            r#""audio":{{"fresh":false,"bands":null}},"device_count":0,"devices":[],"devices_truncated":false,"#,
            r#""counters":{{"ticks":0,"updates_sent":0,"update_errors":0,"frames_read":0,"connects":0,"#,
            r#""disconnects":0,"stdin_lines":0,"stdin_rejected":0,"stdin_ignored":0,"audio_frames":0}}}}"#
        ),
        state = state,
        mode = mode
    )
}

/// `status_json` with the server reachable (connected) and a reason.
pub(crate) fn connected_status_json(state: &str, mode: &str, reason: Option<&str>) -> String {
    let reason = reason.map_or_else(|| "null".to_string(), |reason| format!("\"{reason}\""));
    status_json(state, mode)
        .replace(r#""reachable":false"#, r#""reachable":true"#)
        .replace(r#""reason":null"#, &format!(r#""reason":{reason}"#))
}

/// A fake worker that re-prints `$0.status` (whatever the test last wrote
/// there) every 50 ms, so a test can move it between states.
pub(crate) const STATUS_FILE_BODY: &str =
    "while :; do cat \"$0.status\" 2>/dev/null; sleep 0.05; done";

pub(crate) fn set_worker_status(worker: &Path, line: &str) {
    let path = PathBuf::from(format!("{}.status", worker.display()));
    let temporary = PathBuf::from(format!("{}.status.tmp", worker.display()));
    fs::write(&temporary, format!("{line}\n")).unwrap();
    fs::rename(temporary, path).unwrap();
}

fn manager(worker: &Path, settings: LedSettings) -> LedManager {
    LedManager::new(
        Some(LedWorkerConfig {
            worker_path: worker.to_path_buf(),
            port: None,
        }),
        settings,
    )
}

fn fast_timings() -> Timings {
    Timings {
        stop_grace: Duration::from_millis(300),
        restart_delay: Duration::from_millis(30),
        ..Timings::default()
    }
}

fn tick_until(manager: &mut LedManager, what: &str, mut done: impl FnMut(&LedManager) -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done(manager) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {:?}",
            manager.status()
        );
        manager.tick();
        thread::sleep(Duration::from_millis(10));
    }
}

/// Waits until the fake worker mirrored at least `count` stdin lines.
fn stdin_lines(worker: &Path, count: usize) -> Vec<String> {
    let path = PathBuf::from(format!("{}.stdin", worker.display()));
    let deadline = Instant::now() + WAIT;
    loop {
        if let Ok(text) = fs::read_to_string(&path) {
            let lines: Vec<String> = text.lines().map(str::to_string).collect();
            if lines.len() >= count {
                return lines;
            }
        }
        assert!(
            Instant::now() < deadline,
            "fake worker never mirrored {count} line(s)"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn decode(line: &str) -> proto::Control {
    proto::decode_control(line.as_bytes()).unwrap()
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only probes existence.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn audio_line(bands: usize, level: f32) -> Vec<u8> {
    encode_audio_frame(&AudioFrame::new(7, vec![level; bands], vec![level; bands]).unwrap())
        .unwrap()
}

#[test]
fn enabled_spawns_with_settings_flags_and_feeds_frame_config_and_pause_without_respawn() {
    let root = temporary_directory("lifecycle");
    let worker = fake_worker(&root, RECORDING_BODY);
    let settings = LedSettings {
        brightness: 60,
        rate_hz: 15,
        disabled_devices: vec!["0123456789abcdef".into()],
        ..LedSettings::default()
    };
    let mut led = manager(&worker, settings.clone());
    led.set_timings_for_test(fast_timings());
    let frame = root.join("frames").join("worker-7.bin");
    led.set_frame(Some(&frame));
    tick_until(&mut led, "spawn", |led| led.running_pid().is_some());
    let pid = led.running_pid().unwrap();

    // Initial settings ride the argv (no startup race) ...
    let argv = {
        let path = PathBuf::from(format!("{}.argv", worker.display()));
        let deadline = Instant::now() + WAIT;
        loop {
            if let Ok(text) = fs::read_to_string(&path)
                && text.contains("--disable-device")
            {
                break text;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
    };
    let args: Vec<&str> = argv.lines().collect();
    assert_eq!(
        args,
        [
            "--mode",
            "ambient",
            "--brightness",
            "60",
            "--rate-hz",
            "15",
            "--disable-device",
            "0123456789abcdef"
        ]
    );
    // ... and the current frame source is the first stdin line.
    let lines = stdin_lines(&worker, 1);
    assert_eq!(
        decode(&lines[0]),
        proto::Control::Frame(Some(frame.clone()))
    );

    // A settings change is one led_config line with just the changed
    // fields; the worker is not respawned.
    led.set_settings(LedSettings {
        mode: Mode::Spectrum,
        brightness: 30,
        ..settings.clone()
    });
    let lines = stdin_lines(&worker, 2);
    assert_eq!(
        decode(&lines[1]),
        proto::Control::Config(ConfigPatch {
            mode: Some(Mode::Spectrum),
            brightness: Some(30),
            ..ConfigPatch::default()
        })
    );
    // No renderer live → led_frame null.
    led.set_frame(None);
    led.set_frame(None);
    let lines = stdin_lines(&worker, 3);
    assert_eq!(decode(&lines[2]), proto::Control::Frame(None));
    // Pause → mode off; resume → the configured mode.
    led.set_paused(true);
    led.set_paused(true);
    led.set_paused(false);
    let lines = stdin_lines(&worker, 5);
    assert_eq!(
        decode(&lines[3]),
        proto::Control::Config(ConfigPatch {
            mode: Some(Mode::Off),
            ..ConfigPatch::default()
        })
    );
    assert_eq!(
        decode(&lines[4]),
        proto::Control::Config(ConfigPatch {
            mode: Some(Mode::Spectrum),
            ..ConfigPatch::default()
        })
    );
    // A mode change while paused is remembered but not sent (stays off).
    led.set_paused(true);
    led.set_settings(LedSettings {
        mode: Mode::AmbientPulse,
        brightness: 30,
        ..settings.clone()
    });
    led.set_paused(false);
    let lines = stdin_lines(&worker, 7);
    assert_eq!(lines.len(), 7, "{lines:?}");
    assert_eq!(
        decode(&lines[6]),
        proto::Control::Config(ConfigPatch {
            mode: Some(Mode::AmbientPulse),
            ..ConfigPatch::default()
        })
    );
    for _ in 0..5 {
        led.tick();
    }
    assert_eq!(led.running_pid(), Some(pid), "never respawned");
    assert_eq!(led.status().worker.restarts, 0);

    // Disable → stdin closed + SIGTERM; reaped on later ticks.
    led.set_settings(LedSettings {
        enabled: false,
        ..settings
    });
    assert_eq!(led.running_pid(), None);
    tick_until(&mut led, "stop", |led| !led.is_stopping());
    assert!(!alive(pid), "worker {pid} must be reaped");
    let status = led.status();
    assert_eq!(status.state, "disabled");
    assert!(!status.worker.running);
    for _ in 0..5 {
        led.tick();
    }
    assert_eq!(led.running_pid(), None, "disabled never spawns");
    fs::remove_dir_all(root).ok();
}

#[test]
fn crash_loop_exhausts_the_budget_then_reprobe_respawns_and_is_rate_limited() {
    let root = temporary_directory("budget");
    let worker = fake_worker(&root, "exit 1");
    let mut led = manager(&worker, LedSettings::default());
    led.set_timings_for_test(fast_timings());
    tick_until(&mut led, "budget", |led| {
        led.status().worker.restart_budget_exhausted
    });
    let status = led.status();
    assert_eq!(status.state, "worker_unavailable");
    assert_eq!(status.reason.as_deref(), Some("restart_budget_exhausted"));
    assert_eq!(status.worker.restarts, MAX_RESTARTS as u64);
    assert_eq!(status.worker.last_exit.as_deref(), Some("exit_code_1"));
    assert!(!status.worker.running);

    let outcome = led.reprobe();
    assert!(outcome.accepted);
    assert_eq!(outcome.action, Some("respawn"));
    let again = led.reprobe();
    assert!(!again.accepted);
    assert_eq!(again.reason, Some("rate_limited"));
    assert!(again.retry_after_ms.is_some_and(|ms| ms <= 2000));
    // The fresh budget is used up again by the same crash loop.
    tick_until(&mut led, "budget again", |led| {
        led.status().worker.restart_budget_exhausted
    });
    fs::remove_dir_all(root).ok();
}

#[test]
fn missing_or_unconfigured_binary_is_a_quiet_unavailable_state() {
    let root = temporary_directory("missing");
    let mut led = manager(&root.join("no-such-worker"), LedSettings::default());
    for _ in 0..5 {
        led.tick();
    }
    let status = led.status();
    assert_eq!(status.state, "worker_unavailable");
    assert_eq!(status.reason.as_deref(), Some("not_installed"));
    assert_eq!(status.worker.restarts, 0, "a missing binary is no strike");
    assert!(!status.worker.available);

    let mut none = LedManager::new(None, LedSettings::default());
    none.tick();
    let status = none.status();
    assert_eq!(status.state, "worker_unavailable");
    assert_eq!(status.reason.as_deref(), Some("not_configured"));
    assert_eq!(none.reprobe().reason, Some("not_configured"));

    let mut off = LedManager::new(
        None,
        LedSettings {
            enabled: false,
            ..LedSettings::default()
        },
    );
    off.tick();
    assert_eq!(off.status().state, "disabled");
    assert_eq!(off.reprobe().reason, Some("disabled"));
    fs::remove_dir_all(root).ok();
}

#[test]
fn a_worker_that_never_reads_stdin_never_blocks_the_audio_tee() {
    let root = temporary_directory("stalled");
    // `sleep` never reads its stdin: the pipe fills and stays full.
    let worker = fake_worker(&root, "exec /bin/sleep 60");
    let mut led = manager(&worker, LedSettings::default());
    led.set_timings_for_test(fast_timings());
    tick_until(&mut led, "spawn", |led| led.running_pid().is_some());
    let line = audio_line(64, 0.0009); // −20 dBFS: the real pipeline scale
    // 4000 lines ≈ 5 MiB: ~80× the pipe's capacity.
    let started = Instant::now();
    for i in 0..4000 {
        led.offer_audio(&line);
        if i % 100 == 0 {
            led.set_frame(Some(Path::new(&format!("/run/kwe/frame-{i}.bin"))));
            led.set_settings(LedSettings {
                brightness: (i / 100) as u8,
                ..LedSettings::default()
            });
            led.tick();
        }
    }
    let elapsed = started.elapsed();
    // Non-blocking writes take microseconds each; a single blocking write
    // would hang here forever (the reader never reads). The bound is two
    // orders of magnitude above the measured time to stay unflaky.
    assert!(elapsed < Duration::from_secs(3), "tee took {elapsed:?}");
    let status = led.status();
    assert!(status.tee.audio_sent > 0, "{:?}", status.tee);
    assert!(status.tee.audio_coalesced > 3000, "{:?}", status.tee);
    assert_eq!(status.tee.write_errors, 0);
    let pid = led.running_pid().unwrap();
    drop(led);
    assert!(!alive(pid), "drop must stop and reap the worker");
    fs::remove_dir_all(root).ok();
}

#[test]
fn a_worker_ignoring_sigterm_is_killed_after_the_grace_and_reaped() {
    let root = temporary_directory("grace");
    // Ignored dispositions survive exec, so `sleep` ignores SIGTERM too.
    // Readiness handshake: the marker appears only after the trap is set.
    let worker = fake_worker(&root, "trap '' TERM\n: > \"$0.ready\"\nexec /bin/sleep 60");
    let mut led = manager(&worker, LedSettings::default());
    led.set_timings_for_test(fast_timings());
    tick_until(&mut led, "spawn", |led| led.running_pid().is_some());
    let pid = led.running_pid().unwrap();
    let ready = PathBuf::from(format!("{}.ready", worker.display()));
    let deadline = Instant::now() + WAIT;
    while !ready.exists() {
        assert!(
            Instant::now() < deadline,
            "fake worker never armed its trap"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let started = Instant::now();
    led.set_settings(LedSettings {
        enabled: false,
        ..LedSettings::default()
    });
    assert!(alive(pid), "SIGTERM is ignored");
    tick_until(&mut led, "forced stop", |led| !led.is_stopping());
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(300), "{elapsed:?}");
    assert!(!alive(pid));
    fs::remove_dir_all(root).ok();
}

#[test]
fn the_default_stop_grace_meets_the_worker_contract() {
    assert!(STOP_GRACE >= Duration::from_secs(4));
    assert!(Timings::default().stop_grace >= Duration::from_secs(4));
}

#[test]
fn status_lines_are_read_latest_wins_bounded_and_staleness_is_reported_then_restarted() {
    let root = temporary_directory("status");
    let big = "x".repeat(40 * 1024);
    let body = format!(
        "printf '%s\\n' '{big}'\nprintf '%s\\n' 'not json'\nprintf '%s\\n' '{}'\nprintf '%s\\n' '{}'\nexec /bin/sleep 60",
        status_json("server_absent", "ambient"),
        status_json("active", "ambient"),
    );
    let worker = fake_worker(&root, &body);
    let mut led = manager(&worker, LedSettings::default());
    led.set_timings_for_test(Timings {
        status_stale: Duration::from_millis(800),
        wedged: Duration::from_millis(1600),
        ..fast_timings()
    });
    tick_until(&mut led, "active", |led| led.status().state == "active");
    let status = led.status();
    assert!(status.worker_status.is_some());
    assert!(status.worker.status_lines_rejected >= 1, "{status:?}");
    let first_pid = led.running_pid().unwrap();
    // No heartbeat: degraded/worker_unresponsive, then restarted.
    tick_until(&mut led, "stale", |led| {
        led.status().reason.as_deref() == Some("worker_unresponsive")
    });
    assert_eq!(led.status().state, "degraded");
    tick_until(&mut led, "wedged restart", |led| {
        led.running_pid().is_some_and(|pid| pid != first_pid)
    });
    assert!(!alive(first_pid));
    assert_eq!(led.status().worker.restarts, 1);
    assert_eq!(led.status().worker.last_exit.as_deref(), Some("wedged"));
    fs::remove_dir_all(root).ok();
}

#[test]
fn status_reports_paused_off_and_degraded_reasons() {
    let root = temporary_directory("states");
    let body = format!(
        "printf '%s\\n' '{}'\nexec /bin/cat > /dev/null",
        status_json("off", "off")
    );
    let worker = fake_worker(&root, &body);
    let mut led = manager(&worker, LedSettings::default());
    led.set_timings_for_test(fast_timings());
    led.set_paused(true);
    tick_until(&mut led, "off", |led| led.status().state == "off");
    let status = led.status();
    assert!(status.paused);
    assert_eq!(status.reason.as_deref(), Some("paused"));
    // Spawned while paused: the argv carried mode off.
    assert_eq!(status.settings.mode, Mode::Ambient, "the setting is kept");
    drop(led);
    fs::remove_dir_all(root).ok();
}

// --------------------------------------------------------------------------
// The real worker against the stub SDK server.
// --------------------------------------------------------------------------

/// Set by `scripts/check.sh` (after building the worker): a missing
/// `kwe-led-worker` is then a test failure, never a silent skip.
pub(crate) const REQUIRE_WORKER_ENV: &str = "KWE_REQUIRE_LED_WORKER";

/// `target/<profile>/kwe-led-worker` beside this test binary's `deps/`
/// directory. `cargo test --workspace` builds it; `cargo test -p
/// kwe-daemon` alone may not: then these tests skip with a line written
/// straight to stderr (not swallowed by the test harness's capture) —
/// unless `KWE_REQUIRE_LED_WORKER=1`, which turns the skip into a failure.
pub(crate) fn real_worker() -> Option<PathBuf> {
    let path = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.join("kwe-led-worker")));
    let required = std::env::var_os(REQUIRE_WORKER_ENV).is_some_and(|value| value == "1");
    match path {
        Some(path) if path.is_file() => Some(path),
        path => {
            let shown = path.map_or_else(|| "<unknown>".to_string(), |p| p.display().to_string());
            assert!(
                !required,
                "{REQUIRE_WORKER_ENV}=1 but the LED worker is not built at {shown}"
            );
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "skipped: worker not built ({shown}; run `cargo build -p kwe-led-worker`)"
            );
            None
        }
    }
}

/// A loopback port nothing listens on, chosen BELOW the kernel's ephemeral
/// range (32768..) so a parallel test's stub `bind(0)` can never be handed
/// the same port between our probe and the worker's connect. Deterministic
/// per (process, slot) like the worker crate's tests; probes are bounded.
pub(crate) fn quiet_port(slot: u32) -> u16 {
    let pid = std::process::id();
    for k in 0..200_u32 {
        let port = 20_000 + (pid.wrapping_mul(37) + slot * 1_013 + k * 7_919) % 12_000;
        let port = u16::try_from(port).expect("below 32000");
        if TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok() {
            return port;
        }
    }
    panic!("no quiet loopback port found");
}

fn count(stub: &StubServer, id: u32) -> usize {
    stub.received().iter().filter(|p| p.id == id).count()
}

fn write_frame(path: &Path) -> SharedFrameWriter {
    let (w, h) = (64, 36);
    let mut writer = SharedFrameWriter::create(path, FrameSpec::new(w, h).unwrap()).unwrap();
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..w * h {
        px.extend_from_slice(&[0, 0, 255, 255]); // BGRA red
    }
    writer.publish(&px).unwrap();
    writer
}

#[test]
fn real_worker_drives_the_stub_pauses_off_and_restores_on_disable() {
    let Some(binary) = real_worker() else {
        return;
    };
    let stub = StubServer::start(StubConfig::default()).unwrap();
    let root = temporary_directory("real");
    let frame = root.join("frame.bin");
    let _writer = write_frame(&frame);
    let mut led = LedManager::new(
        Some(LedWorkerConfig {
            worker_path: binary,
            port: Some(stub.port()),
        }),
        LedSettings::default(),
    );
    led.set_frame(Some(&frame));
    tick_until(&mut led, "active with frame", |led| {
        let status = led.status();
        status.state == "active"
            && status.worker_status.as_ref().is_some_and(|line| {
                line.device_count == 2
                    && line.frame.state == proto::FrameState::Ok
                    && line.server.protocol.is_some()
            })
    });
    let status = led.status();
    let line = status.worker_status.as_ref().unwrap();
    assert_eq!(line.devices.len(), 2);
    assert!(line.devices.iter().all(|d| proto::is_device_id(&d.id)));
    assert!(!line.devices[0].zones.is_empty());
    let deadline = Instant::now() + WAIT;
    while count(&stub, packet_id::UPDATE_LEDS) == 0 {
        assert!(Instant::now() < deadline, "no UpdateLEDs reached the stub");
        led.tick();
        thread::sleep(Duration::from_millis(20));
    }
    assert!(count(&stub, packet_id::SET_CUSTOM_MODE) >= 1);
    // The frame is solid red; ambient smoothing converges within ~1 s.
    let deadline = Instant::now() + WAIT;
    loop {
        led.tick();
        let c = stub.controllers()[0].colors[0];
        if c.r > 200 && c.g < 40 && c.b < 40 {
            break;
        }
        assert!(Instant::now() < deadline, "LEDs never turned red: {c:?}");
        thread::sleep(Duration::from_millis(20));
    }

    // Pause verdict → mode off → devices restored, worker stays.
    let pid = led.running_pid().unwrap();
    let restores_before = count(&stub, packet_id::UPDATE_MODE);
    led.set_paused(true);
    tick_until(&mut led, "off", |led| led.status().state == "off");
    let deadline = Instant::now() + WAIT;
    while count(&stub, packet_id::UPDATE_MODE) < restores_before + 2 {
        assert!(Instant::now() < deadline, "mode off did not restore");
        led.tick();
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(led.status().reason.as_deref(), Some("paused"));
    // Review fixes C1 (R4): the pause path (mode off → back) does not
    // retake the devices sooner than the worker's minimum retake interval
    // (5 s) after the release — a fullscreen app toggling cannot cycle
    // the devices' modes. They are taken again after it (below).
    let takes = count(&stub, packet_id::SET_CUSTOM_MODE);
    let resumed = Instant::now();
    led.set_paused(false);
    tick_until(&mut led, "active again", |led| {
        led.status().state == "active"
    });
    assert_eq!(led.running_pid(), Some(pid));
    while resumed.elapsed() < Duration::from_millis(2500) {
        led.tick();
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        count(&stub, packet_id::SET_CUSTOM_MODE),
        takes,
        "retaken too soon"
    );

    // Config change live: brightness 0 → black LEDs, same process.
    led.set_settings(LedSettings {
        brightness: 0,
        ..LedSettings::default()
    });
    let deadline = Instant::now() + WAIT;
    loop {
        led.tick();
        let c = stub.controllers()[0].colors[0];
        if (c.r, c.g, c.b) == (0, 0, 0) {
            break;
        }
        assert!(Instant::now() < deadline, "brightness 0 never applied");
        thread::sleep(Duration::from_millis(20));
    }
    // The next status line (material change) reports the new cap.
    tick_until(&mut led, "status brightness 0", |led| {
        led.status()
            .worker_status
            .is_some_and(|line| line.brightness == 0)
    });
    assert_eq!(led.running_pid(), Some(pid));

    // Disable → stopped within the grace; every device restored.
    let restores_before = count(&stub, packet_id::UPDATE_MODE);
    let started = Instant::now();
    led.set_settings(LedSettings {
        enabled: false,
        brightness: 0,
        ..LedSettings::default()
    });
    tick_until(&mut led, "stopped", |led| !led.is_stopping());
    assert!(started.elapsed() < STOP_GRACE, "{:?}", started.elapsed());
    assert!(!alive(pid));
    assert_eq!(count(&stub, packet_id::UPDATE_MODE), restores_before + 2);
    for controller in stub.controllers() {
        assert_eq!(
            controller.active_mode, 2,
            "{} back to Rainbow",
            controller.name
        );
    }
    let restore = led.status().worker.last_restore.expect("restore summary");
    assert_eq!((restore.attempted, restore.restored), (2, 2), "{restore:?}");
    assert_eq!(led.status().state, "disabled");
    fs::remove_dir_all(root).ok();
}

#[test]
fn real_worker_reports_server_absent_quietly_and_accepts_reprobe() {
    let Some(binary) = real_worker() else {
        return;
    };
    let mut led = LedManager::new(
        Some(LedWorkerConfig {
            worker_path: binary,
            port: Some(quiet_port(1)),
        }),
        LedSettings::default(),
    );
    tick_until(&mut led, "server_absent", |led| {
        led.status().state == "server_absent"
    });
    let status = led.status();
    assert!(status.reason.is_none());
    assert_eq!(status.worker.restarts, 0, "absence is no strike");
    let outcome = led.reprobe();
    assert!(outcome.accepted, "{outcome:?}");
    assert_eq!(outcome.action, Some("reprobe"));
    for _ in 0..20 {
        led.tick();
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(led.status().state, "server_absent");
    let pid = led.running_pid().unwrap();
    drop(led);
    assert!(!alive(pid));
}

#[test]
fn real_worker_crash_is_restarted_within_the_budget() {
    let Some(binary) = real_worker() else {
        return;
    };
    let mut led = LedManager::new(
        Some(LedWorkerConfig {
            worker_path: binary,
            port: Some(quiet_port(2)),
        }),
        LedSettings::default(),
    );
    led.set_timings_for_test(fast_timings());
    tick_until(&mut led, "server_absent", |led| {
        led.status().state == "server_absent"
    });
    let pid = led.running_pid().unwrap();
    // SAFETY: a crash stand-in for our own child.
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    tick_until(&mut led, "respawn", |led| {
        led.running_pid().is_some_and(|new| new != pid)
    });
    let status = led.status();
    assert_eq!(status.worker.restarts, 1);
    assert_eq!(status.worker.last_exit.as_deref(), Some("signal_9"));
    tick_until(&mut led, "server_absent again", |led| {
        led.status().state == "server_absent"
    });
}

// --------------------------------------------------------------------------
// Review fixes A
// --------------------------------------------------------------------------

#[test]
fn saved_modes_path_rides_the_argv_and_waiting_reason_passes_through() {
    let root = temporary_directory("saved-argv");
    let status = status_json("active", "ambient")
        .replace(r#""reason":null"#, r#""reason":"waiting_for_source""#);
    let body = format!(
        "printf '%s\\n' \"$@\" > \"$0.argv\"\nprintf '%s\\n' '{status}'\nexec /bin/cat > \"$0.stdin\""
    );
    let worker = fake_worker(&root, &body);
    let mut led = manager(&worker, LedSettings::default());
    let state = root.join("state");
    led.set_state_dir(&state);
    tick_until(&mut led, "waiting", |led| {
        led.status().reason.as_deref() == Some("waiting_for_source")
    });
    assert_eq!(led.status().state, "active");
    let argv = fs::read_to_string(format!("{}.argv", worker.display())).unwrap();
    let args: Vec<&str> = argv.lines().collect();
    let at = args.iter().position(|a| *a == "--saved-modes").unwrap();
    assert_eq!(
        Path::new(args[at + 1]),
        state.join(SAVED_MODES_FILE).as_path()
    );
    drop(led);
    fs::remove_dir_all(root).ok();
}

/// MUST-FIX 2 through the daemon: a worker SIGKILLed while driving leaves
/// the devices in Direct; the respawned worker (same state directory)
/// gives the user's original modes back on disable.
#[test]
fn real_worker_killed_mid_drive_is_cleaned_up_by_its_successor() {
    let Some(binary) = real_worker() else {
        return;
    };
    let stub = StubServer::start(StubConfig::default()).unwrap();
    let root = temporary_directory("real-kill");
    let frame = root.join("frame.bin");
    let _writer = write_frame(&frame);
    let mut led = LedManager::new(
        Some(LedWorkerConfig {
            worker_path: binary,
            port: Some(stub.port()),
        }),
        LedSettings::default(),
    );
    led.set_timings_for_test(fast_timings());
    led.set_state_dir(&root);
    led.set_frame(Some(&frame));
    tick_until(&mut led, "driving", |_| {
        stub.controllers().iter().all(|c| c.active_mode == 0)
    });
    assert!(root.join(SAVED_MODES_FILE).is_file());
    let pid = led.running_pid().unwrap();
    // SAFETY: a crash stand-in for our own child.
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    tick_until(&mut led, "respawned and driving", |led| {
        led.running_pid().is_some_and(|p| p != pid)
            && led
                .status()
                .worker_status
                .is_some_and(|s| s.driven_count == 2)
    });
    led.set_settings(LedSettings {
        enabled: false,
        ..LedSettings::default()
    });
    tick_until(&mut led, "stopped", |led| !led.is_stopping());
    assert!(
        stub.controllers().iter().all(|c| c.active_mode == 2),
        "originals, not Direct: {:?}",
        stub.controllers()
            .iter()
            .map(|c| c.active_mode)
            .collect::<Vec<_>>()
    );
    assert!(!root.join(SAVED_MODES_FILE).exists());
    fs::remove_dir_all(root).ok();
}

// --------------------------------------------------------------------------
// Review fixes B
// --------------------------------------------------------------------------

#[test]
fn restart_budget_allows_three_restarts_and_the_fourth_exit_exhausts_it() {
    let root = temporary_directory("budget-count");
    // Count spawns through a side file ($0 survives the cleared env).
    let worker = fake_worker(&root, "echo x >> \"$0.spawns\"\nexit 3");
    let mut led = manager(&worker, LedSettings::default());
    led.set_timings_for_test(fast_timings());
    tick_until(&mut led, "budget", |led| {
        led.status().worker.restart_budget_exhausted
    });
    let spawns = fs::read_to_string(format!("{}.spawns", worker.display()))
        .unwrap()
        .lines()
        .count();
    assert_eq!(
        spawns,
        MAX_RESTARTS + 1,
        "first run + {MAX_RESTARTS} restarts"
    );
    assert_eq!(led.status().worker.restarts, MAX_RESTARTS as u64);
    fs::remove_dir_all(root).ok();
}

#[test]
fn a_non_executable_binary_is_a_quiet_unavailable_state_without_a_strike() {
    let root = temporary_directory("noexec");
    let worker = fake_worker(&root, "exit 0");
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o644)).unwrap();
    let mut led = manager(&worker, LedSettings::default());
    led.set_timings_for_test(fast_timings());
    for _ in 0..10 {
        led.tick();
        thread::sleep(Duration::from_millis(5));
    }
    let status = led.status();
    assert_eq!(status.state, "worker_unavailable");
    assert_eq!(status.reason.as_deref(), Some("not_executable"));
    assert_eq!(status.worker.restarts, 0, "no restart strike");
    assert!(!status.worker.restart_budget_exhausted);
    assert!(!led.wants_audio());
    // Fixing the mode bits + "Check again" respawns it.
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();
    let outcome = led.reprobe();
    assert_eq!(outcome.action, Some("respawn"));
    tick_until(&mut led, "spawned", |led| {
        led.status().reason.as_deref() != Some("not_executable")
    });
    fs::remove_dir_all(root).ok();
}

#[test]
fn reprobe_while_restarting_says_when_to_retry() {
    let root = temporary_directory("retry-after");
    let worker = fake_worker(&root, RECORDING_BODY);
    let mut led = manager(&worker, LedSettings::default());
    led.set_timings_for_test(Timings {
        restart_delay: Duration::from_millis(1500),
        ..fast_timings()
    });
    tick_until(&mut led, "spawn", |led| led.running_pid().is_some());
    let pid = led.running_pid().unwrap();
    // SAFETY: a crash stand-in for our own child.
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    tick_until(&mut led, "exit noticed", |led| led.running_pid().is_none());
    let outcome = led.reprobe();
    assert!(!outcome.accepted);
    assert_eq!(outcome.reason, Some("worker_restarting"));
    let after = outcome
        .retry_after_ms
        .expect("retry_after_ms for worker_restarting");
    assert!((1..=1500).contains(&after), "{after}");
    drop(led);
    fs::remove_dir_all(root).ok();
}

#[test]
fn audio_demand_follows_mode_enable_and_configuration() {
    let root = temporary_directory("demand");
    let worker = fake_worker(&root, RECORDING_BODY);
    let mut led = manager(&worker, LedSettings::default());
    assert!(!led.wants_audio(), "ambient never needs capture");
    for (mode, wants) in [
        (Mode::Spectrum, true),
        (Mode::AmbientPulse, true),
        (Mode::Ambient, false),
        (Mode::Off, false),
    ] {
        led.set_settings(LedSettings {
            mode,
            ..LedSettings::default()
        });
        assert_eq!(led.wants_audio(), wants, "{mode:?}");
    }
    led.set_settings(LedSettings {
        enabled: false,
        mode: Mode::Spectrum,
        ..LedSettings::default()
    });
    assert!(!led.wants_audio(), "disabled");
    let unconfigured = LedManager::new(
        None,
        LedSettings {
            mode: Mode::Spectrum,
            ..LedSettings::default()
        },
    );
    assert!(!unconfigured.wants_audio(), "--no-led-worker");
    // consumes_audio additionally needs a live worker and no pause.
    let mut led = manager(
        &worker,
        LedSettings {
            mode: Mode::Spectrum,
            ..LedSettings::default()
        },
    );
    assert!(!led.consumes_audio(), "not spawned yet");
    tick_until(&mut led, "spawn", |led| led.running_pid().is_some());
    assert!(led.consumes_audio());
    led.set_paused(true);
    assert!(!led.consumes_audio(), "paused: mode off");
    drop(led);
    fs::remove_dir_all(root).ok();
}

/// Review fixes C2 (R1): capture is demanded only while the worker is
/// connected to an OpenRGB server (including `waiting_for_source`, the
/// Spectrum bootstrap) and the pause verdict does not hold; losing either
/// drops the demand only after the hold, so a flap never restarts capture;
/// a settings change drops it at once.
#[test]
fn audio_demand_needs_a_connected_unpaused_worker_and_is_debounced() {
    let root = temporary_directory("demand-live");
    let worker = fake_worker(&root, STATUS_FILE_BODY);
    set_worker_status(&worker, &status_json("server_absent", "spectrum"));
    let mut led = manager(
        &worker,
        LedSettings {
            mode: Mode::Spectrum,
            ..LedSettings::default()
        },
    );
    let hold = Duration::from_millis(600);
    led.set_timings_for_test(Timings {
        audio_hold: hold,
        ..fast_timings()
    });
    // No OpenRGB server: the settings want audio, nobody would consume it.
    tick_until(&mut led, "server_absent", |led| {
        led.status().state == "server_absent"
    });
    assert!(led.wants_audio());
    for _ in 0..20 {
        led.tick();
        assert!(!led.audio_demand(), "no server: no capture");
        thread::sleep(Duration::from_millis(10));
    }

    // Connected, waiting for audio (Spectrum's bootstrap): demand rises.
    set_worker_status(
        &worker,
        &connected_status_json("active", "spectrum", Some("waiting_for_source")),
    );
    tick_until(&mut led, "demand while waiting_for_source", |led| {
        led.audio_demand()
    });

    // A short disconnect flap (well under the hold) never drops it.
    set_worker_status(&worker, &status_json("server_absent", "spectrum"));
    tick_until(&mut led, "flap seen", |led| {
        led.status().state == "server_absent"
    });
    assert!(led.audio_demand(), "held through the flap");
    set_worker_status(&worker, &connected_status_json("active", "spectrum", None));
    tick_until(&mut led, "reconnected", |led| {
        led.status().state == "active"
    });
    assert!(led.audio_demand());

    // A lasting disconnect drops it after the hold, not before.
    set_worker_status(&worker, &status_json("server_absent", "spectrum"));
    tick_until(&mut led, "absent", |led| {
        led.status().state == "server_absent"
    });
    let lost = Instant::now();
    tick_until(&mut led, "demand dropped", |led| !led.audio_demand());
    let waited = lost.elapsed();
    assert!(
        waited + Duration::from_millis(150) >= hold,
        "dropped after {waited:?}"
    );

    // Paused (fullscreen / displays off) while connected: same hold, then
    // off; resuming brings it back at once.
    set_worker_status(&worker, &connected_status_json("active", "spectrum", None));
    tick_until(&mut led, "connected again", |led| led.audio_demand());
    led.set_paused(true);
    led.tick();
    assert!(led.audio_demand(), "a quick pause toggle keeps capture");
    led.set_paused(false);
    led.tick();
    assert!(led.audio_demand());
    led.set_paused(true);
    tick_until(&mut led, "paused drop", |led| !led.audio_demand());
    led.set_paused(false);
    tick_until(&mut led, "resumed", |led| led.audio_demand());

    // A settings change that removes the need drops it on the next tick.
    led.set_settings(LedSettings {
        mode: Mode::Ambient,
        ..LedSettings::default()
    });
    led.tick();
    assert!(!led.audio_demand(), "ambient: no capture, no hold");
    drop(led);
    fs::remove_dir_all(root).ok();
}

// --------------------------------------------------------------------------
// Review fixes D (R-a): the restore-only pass after an unclean death when
// no successor will run.
// --------------------------------------------------------------------------

/// A real-worker manager on `port` with the state dir and a red frame.
fn real_manager(binary: PathBuf, port: u16, root: &Path, frame: &Path) -> LedManager {
    let mut led = LedManager::new(
        Some(LedWorkerConfig {
            worker_path: binary,
            port: Some(port),
        }),
        LedSettings::default(),
    );
    led.set_timings_for_test(fast_timings());
    led.set_state_dir(root);
    led.set_frame(Some(frame));
    led
}

fn modes(stub: &StubServer) -> Vec<i32> {
    stub.controllers().iter().map(|c| c.active_mode).collect()
}

fn kill(pid: u32) {
    // SAFETY: a crash stand-in for our own child.
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

/// R-a: a worker crash-looping while it drives (SIGKILLed mid-drive until
/// the restart budget is gone) no longer leaves the devices frozen on the
/// last frame: the daemon runs one restore pass and they end on their
/// ORIGINAL modes (Rainbow = 2), the file removed, no extra restart.
#[test]
fn budget_exhausted_by_kills_mid_drive_ends_on_the_original_modes() {
    let Some(binary) = real_worker() else {
        return;
    };
    let stub = StubServer::start(StubConfig::default()).unwrap();
    let root = temporary_directory("restore-budget");
    let frame = root.join("frame.bin");
    let _writer = write_frame(&frame);
    let mut led = real_manager(binary, stub.port(), &root, &frame);
    let mut last = None;
    for round in 0..=MAX_RESTARTS {
        tick_until(&mut led, "driving", |led| {
            led.running_pid().is_some_and(|p| Some(p) != last)
                && led
                    .status()
                    .worker_status
                    .is_some_and(|s| s.driven_count == 2)
        });
        assert_eq!(modes(&stub), vec![0, 0], "round {round}: driven");
        let pid = led.running_pid().unwrap();
        kill(pid);
        last = Some(pid);
    }
    tick_until(&mut led, "budget exhausted", |led| {
        led.status().worker.restart_budget_exhausted
    });
    tick_until(&mut led, "restored", |led| {
        !led.is_restoring() && modes(&stub) == vec![2, 2]
    });
    assert!(!root.join(SAVED_MODES_FILE).exists());
    let status = led.status();
    assert_eq!(status.worker.restarts, MAX_RESTARTS as u64, "no strike");
    assert!(status.worker.restart_budget_exhausted && !status.worker.running);
    drop(led);
    fs::remove_dir_all(root).ok();
}

/// R-a: disabling LED sync right after an unclean death (before any
/// successor ran) restores the originals through the pass.
#[test]
fn disable_right_after_an_unclean_death_restores_the_originals() {
    let Some(binary) = real_worker() else {
        return;
    };
    let stub = StubServer::start(StubConfig::default()).unwrap();
    let root = temporary_directory("restore-disable");
    let frame = root.join("frame.bin");
    let _writer = write_frame(&frame);
    let mut led = real_manager(binary, stub.port(), &root, &frame);
    tick_until(&mut led, "driving", |led| {
        led.status()
            .worker_status
            .is_some_and(|s| s.driven_count == 2)
    });
    kill(led.running_pid().unwrap());
    led.set_settings(LedSettings {
        enabled: false,
        ..LedSettings::default()
    });
    tick_until(&mut led, "restored", |led| {
        !led.is_stopping() && !led.is_restoring() && modes(&stub) == vec![2, 2]
    });
    assert!(!root.join(SAVED_MODES_FILE).exists());
    assert_eq!(led.status().worker.restarts, 0);
    assert!(led.running_pid().is_none());
    drop(led);
    fs::remove_dir_all(root).ok();
}

/// R-a: with no saved-modes file there is no pass at all; with a file but
/// no server the pass is a bounded no-op that leaves the file; and the
/// daemon's shutdown runs a pass still owed (within its bound).
#[test]
fn restore_pass_is_quiet_without_a_file_or_a_server_and_runs_at_shutdown() {
    let Some(binary) = real_worker() else {
        return;
    };
    // No file: budget exhausted by kills against an absent server.
    let root = temporary_directory("restore-nofile");
    let frame = root.join("frame.bin");
    let _writer = write_frame(&frame);
    let mut led = real_manager(binary.clone(), quiet_port(21), &root, &frame);
    let mut last = None;
    for _ in 0..=MAX_RESTARTS {
        tick_until(&mut led, "worker up", |led| {
            led.running_pid().is_some_and(|p| Some(p) != last)
        });
        let pid = led.running_pid().unwrap();
        kill(pid);
        last = Some(pid);
    }
    tick_until(&mut led, "budget exhausted", |led| {
        led.status().worker.restart_budget_exhausted
    });
    for _ in 0..20 {
        led.tick();
        assert!(!led.is_restoring(), "no file, no pass");
        thread::sleep(Duration::from_millis(5));
    }
    drop(led);
    fs::remove_dir_all(root).ok();

    // A file, then the server goes away: the pass ends at once, keeps it.
    let stub = StubServer::start(StubConfig::default()).unwrap();
    let root = temporary_directory("restore-noserver");
    let frame = root.join("frame.bin");
    let _writer = write_frame(&frame);
    let mut led = real_manager(binary.clone(), stub.port(), &root, &frame);
    tick_until(&mut led, "driving", |led| {
        led.status()
            .worker_status
            .is_some_and(|s| s.driven_count == 2)
    });
    let port = stub.port();
    drop(stub);
    kill(led.running_pid().unwrap());
    led.set_settings(LedSettings {
        enabled: false,
        ..LedSettings::default()
    });
    tick_until(&mut led, "pass started", |led| led.is_restoring());
    let started = Instant::now();
    tick_until(&mut led, "pass ended", |led| !led.is_restoring());
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(root.join(SAVED_MODES_FILE).is_file(), "kept for later");
    drop(led);

    // Shutdown with a pass owed (worker died, successor not yet started):
    // the server is back; the pass runs inside `finish_shutdown`.
    let stub = start_stub_on(port);
    let mut led = real_manager(binary, port, &root, &frame);
    led.set_timings_for_test(Timings {
        restart_delay: Duration::from_secs(60),
        ..fast_timings()
    });
    tick_until(&mut led, "driving", |led| {
        led.status()
            .worker_status
            .is_some_and(|s| s.driven_count == 2)
    });
    kill(led.running_pid().unwrap());
    tick_until(&mut led, "death seen", |led| led.running_pid().is_none());
    assert_eq!(modes(&stub), vec![0, 0], "frozen until the pass");
    let t = Instant::now();
    led.finish_shutdown();
    assert!(t.elapsed() < RESTORE_PASS_LIMIT + Duration::from_secs(1));
    let end = Instant::now() + WAIT;
    while modes(&stub) != vec![2, 2] && Instant::now() < end {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(modes(&stub), vec![2, 2]);
    assert!(!root.join(SAVED_MODES_FILE).exists());
    drop(led);
    fs::remove_dir_all(root).ok();
}

/// A stub on a fixed port (the one an earlier stub of this test used),
/// retried for a bounded time.
fn start_stub_on(port: u16) -> StubServer {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        match StubServer::start_on(port, StubConfig::default()) {
            Ok(stub) => return stub,
            Err(error) => {
                assert!(Instant::now() < end, "stub on {port}: {error}");
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}
