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
    let line = audio_line(64, 0.5);
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
    let worker = fake_worker(&root, "trap '' TERM\nexec /bin/sleep 60");
    let mut led = manager(&worker, LedSettings::default());
    led.set_timings_for_test(fast_timings());
    tick_until(&mut led, "spawn", |led| led.running_pid().is_some());
    let pid = led.running_pid().unwrap();
    thread::sleep(Duration::from_millis(100)); // let the trap install
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

/// `target/<profile>/kwe-led-worker` beside this test binary's `deps/`
/// directory. `cargo test --workspace` builds it; `cargo test -p
/// kwe-daemon` alone may not, and then these tests skip loudly.
pub(crate) fn real_worker() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let path = exe.parent()?.parent()?.join("kwe-led-worker");
    if path.is_file() {
        Some(path)
    } else {
        eprintln!(
            "SKIP: {} not built (run `cargo build -p kwe-led-worker` or `cargo test --workspace`)",
            path.display()
        );
        None
    }
}

/// A loopback port nothing listens on (bound and released).
pub(crate) fn quiet_port() -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.local_addr().unwrap().port()
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
    led.set_paused(false);
    tick_until(&mut led, "active again", |led| {
        led.status().state == "active"
    });
    assert_eq!(led.running_pid(), Some(pid));

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
            port: Some(quiet_port()),
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
            port: Some(quiet_port()),
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
