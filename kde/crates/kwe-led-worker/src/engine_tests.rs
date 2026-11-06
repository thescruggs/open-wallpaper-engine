// SPDX-License-Identifier: GPL-3.0-or-later
//! Engine tests on a virtual clock against the in-process recording stub
//! (ephemeral loopback port). `Engine::tick` takes `now`, so the real
//! default timings (1 s dwell, 5 s minimum retake, 3 s silence, 60 s
//! freshness) run here in milliseconds of wall time. OpenRGB I/O is real.
//! Review fixes C1: N1, N2, N3, R4, R5 regressions.

use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kwe_frame_protocol::{FrameSpec, SharedFrameWriter};
use kwe_led_protocol::{Mode, State};
use kwe_openrgb::stub::{StubConfig, StubServer};
use kwe_openrgb::wire::packet_id;
use kwe_openrgb::{Client, Rgb};

use super::*;
use crate::log::LogLimiter;
use crate::saved::{Store, load};

const STEP: Duration = Duration::from_millis(50);

fn temp_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "kwe-led-engine-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

fn solid_frame(path: &std::path::Path, rgb: [u8; 3]) -> SharedFrameWriter {
    let (w, h) = (32, 18);
    let mut writer = SharedFrameWriter::create(path, FrameSpec::new(w, h).unwrap()).unwrap();
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..w * h {
        px.extend_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
    }
    writer.publish(&px).unwrap();
    writer
}

struct Rig {
    stub: StubServer,
    e: Engine,
    t0: Instant,
    now: Instant,
}

impl Rig {
    fn new(mode: Mode, frame: Option<PathBuf>) -> Self {
        Self::with_stub(
            StubServer::start(StubConfig::default()).unwrap(),
            mode,
            frame,
        )
    }

    fn with_stub(stub: StubServer, mode: Mode, frame: Option<PathBuf>) -> Self {
        let t0 = Instant::now();
        let mut cfg = EngineConfig::with_port(stub.port());
        cfg.backoff_min = Duration::from_millis(50);
        let settings = Settings {
            mode,
            rate_hz: 20,
            ..Settings::default()
        };
        let e = Engine::new(cfg, settings, frame, t0, LogLimiter::new(t0));
        Self {
            stub,
            e,
            t0,
            now: t0,
        }
    }

    /// Attaches a saved-modes store at `path` (boot id "b", trusted).
    fn adopt(mut self, path: &std::path::Path) -> Self {
        let (store, loaded) = Store::open_with_boot_id(path.to_path_buf(), Some("b".into()));
        let t0 = self.t0;
        self.e = self.e.adopt_store(store, loaded, t0, SystemTime::now());
        self
    }

    fn secs(&self) -> f32 {
        self.now.duration_since(self.t0).as_secs_f32()
    }

    /// Advances the virtual clock in 50 ms ticks up to `until` seconds,
    /// calling `feed` before each tick.
    fn run_until(&mut self, until: f32, mut feed: impl FnMut(&mut Engine, Instant, f32)) {
        while self.secs() < until {
            self.now += STEP;
            let (now, s) = (self.now, self.secs());
            feed(&mut self.e, now, s);
            self.e.tick(now, &|| false);
        }
    }

    fn count(&self, dev: u32, id: u32) -> usize {
        self.stub
            .received()
            .iter()
            .filter(|p| p.dev_id == dev && p.id == id)
            .count()
    }

    fn modes(&self) -> Vec<i32> {
        self.stub
            .controllers()
            .iter()
            .map(|c| c.active_mode)
            .collect()
    }

    /// Fire-and-forget packets reach the stub asynchronously.
    fn wait_modes(&self, want: &[i32]) -> bool {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if self.modes() == want {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn settled_modes(&self) -> Vec<i32> {
        thread::sleep(Duration::from_millis(100));
        self.modes()
    }
}

fn loud(e: &mut Engine, now: Instant) {
    e.set_audio(now, &[0.8; 16], &[0.8; 16]);
}

fn quiet(e: &mut Engine, now: Instant) {
    e.set_audio(now, &[0.0; 16], &[0.0; 16]);
}

/// N1: silent audio frames (capture runs continuously while an audio mode
/// is selected) are not a source. Spectrum releases after 3 s of silence
/// instead of painting every device black, a gap shorter than that keeps
/// driving, and status tells silence from "no frames".
#[test]
fn spectrum_silence_releases_instead_of_painting_black() {
    let mut r = Rig::new(Mode::Spectrum, None);
    r.run_until(3.0, |e, now, _| loud(e, now));
    assert!(r.wait_modes(&[0, 0]), "driven: {:?}", r.modes());
    assert_eq!(r.e.status(r.now, 1).driven_count, 2);
    // A 2.5 s gap (between songs): still driven, no release.
    r.run_until(5.5, |e, now, _| quiet(e, now));
    assert_eq!(r.settled_modes(), vec![0, 0], "brief gap must not release");
    assert_eq!(r.count(0, packet_id::UPDATE_MODE), 0);
    // Sustained silence: released to the user's own mode, not black.
    r.run_until(7.0, |e, now, _| quiet(e, now));
    assert!(r.wait_modes(&[2, 2]), "released: {:?}", r.modes());
    let st = r.e.status(r.now, 2);
    assert_eq!(st.state, State::Active);
    assert_eq!(st.reason.as_deref(), Some(WAITING_FOR_SOURCE));
    assert!(st.audio.fresh && st.audio.silent, "{:?}", st.audio);
    assert_eq!(st.driven_count, 0);
    assert_eq!(r.count(0, packet_id::UPDATE_MODE), 1);
    // Silence goes on: nothing more is sent.
    let packets = r.stub.received().len();
    r.run_until(9.0, |e, now, _| quiet(e, now));
    thread::sleep(Duration::from_millis(100));
    assert_eq!(r.stub.received().len(), packets, "silence is quiet");
    // Sound returns: taken again once the minimum retake interval since
    // the release (≈ 6.05 s) has passed and the dwell is met.
    r.run_until(10.5, |e, now, _| loud(e, now));
    assert_eq!(r.settled_modes(), vec![2, 2], "inside the retake interval");
    r.run_until(12.0, |e, now, _| loud(e, now));
    assert!(r.wait_modes(&[0, 0]), "taken again: {:?}", r.modes());
    // No frames at all: fresh=false, silent=false.
    r.run_until(16.0, |_, _, _| {});
    let st = r.e.status(r.now, 3);
    assert!(!st.audio.fresh && !st.audio.silent);
    assert_eq!(st.reason.as_deref(), Some(WAITING_FOR_SOURCE));
}

/// N1 (ambient+pulse): in sustained silence the colours are steady
/// ambient, not dimmed to the 20 % pulse floor.
#[test]
fn ambient_pulse_is_steady_ambient_in_silence() {
    let dir = temp_dir("pulse");
    let frame = dir.join("f.bin");
    let _w = solid_frame(&frame, [255, 0, 0]);
    let mut r = Rig::new(Mode::AmbientPulse, Some(frame));
    r.run_until(8.0, |e, now, _| quiet(e, now));
    thread::sleep(Duration::from_millis(100));
    let c = r.stub.controllers()[0].colors.clone();
    assert!(
        c.iter().all(|c| c.r >= 240 && c.g < 10),
        "steady red in silence, got {c:?}"
    );
    // Loud bass pulses to full, and a quiet moment inside music dims.
    r.run_until(9.0, |e, now, _| loud(e, now));
    r.run_until(10.0, |e, now, _| quiet(e, now));
    thread::sleep(Duration::from_millis(100));
    let c = r.stub.controllers()[0].colors[0];
    assert!(
        c.r < 120,
        "a quiet moment in music still pulses down: {c:?}"
    );
    fs::remove_dir_all(dir).ok();
}

/// R4: a flapping source (0.6 s bursts every 3.6 s, the review's
/// reproducer) costs a bounded number of take/release cycles.
#[test]
fn flapping_source_cycles_are_bounded() {
    let mut r = Rig::new(Mode::Spectrum, None);
    r.run_until(20.0, |e, now, s| {
        if s % 3.6 < 0.6 {
            loud(e, now);
        }
    });
    thread::sleep(Duration::from_millis(100));
    let takes = r.count(0, packet_id::SET_CUSTOM_MODE);
    let gives = r.count(0, packet_id::UPDATE_MODE);
    eprintln!("flap 20 s: SetCustomMode {takes}, UpdateMode {gives}");
    // Every retake waits MIN_RETAKE after a release, and a release comes
    // ≥ FRAME_HOLD after the source left: ≤ 1 + 20 / (5 + 2) cycles. The
    // unhysteresised worker made 6 here (one per burst).
    let bound = 1 + (20.0 / (MIN_RETAKE + FRAME_HOLD).as_secs_f32()) as usize;
    assert!(takes >= 1 && takes <= bound, "{takes} takeovers > {bound}");
    assert!(gives <= takes);
}

/// R4: the daemon's pause path (mode off on every fullscreen toggle, then
/// back) is bounded too: off releases at once, the next takeover waits.
#[test]
fn pause_toggling_is_bounded_by_the_minimum_retake() {
    let mut r = Rig::new(Mode::Spectrum, None);
    let mut last = Mode::Spectrum;
    r.run_until(20.0, |e, now, s| {
        loud(e, now);
        // 2 s playing, 1 s "fullscreen app" (mode off), repeated.
        let want = if s % 3.0 >= 2.0 {
            Mode::Off
        } else {
            Mode::Spectrum
        };
        if want != last {
            e.apply_config(ConfigPatch {
                mode: Some(want),
                ..ConfigPatch::default()
            });
            last = want;
        }
    });
    thread::sleep(Duration::from_millis(100));
    let takes = r.count(0, packet_id::SET_CUSTOM_MODE);
    eprintln!("pause toggling 20 s: SetCustomMode {takes}");
    // Without hysteresis: one takeover per 3 s period (7).
    let bound = 1 + (20.0 / MIN_RETAKE.as_secs_f32()) as usize;
    assert!(takes >= 1 && takes <= bound, "{takes} takeovers > {bound}");
}

/// R4: a user's mode change between two non-off modes skips the minimum
/// retake interval once (dwell still applies); off → on does not.
#[test]
fn a_user_mode_switch_skips_the_minimum_retake_once() {
    let dir = temp_dir("bypass");
    let frame = dir.join("f.bin");
    let _w = solid_frame(&frame, [0, 255, 0]);
    let mut r = Rig::new(Mode::Spectrum, Some(frame));
    r.run_until(3.0, |e, now, _| loud(e, now));
    assert!(r.wait_modes(&[0, 0]));
    // Audio stops: released at ≈ 5.55 s.
    r.run_until(6.0, |_, _, _| {});
    assert!(r.wait_modes(&[2, 2]));
    let mode = |m| ConfigPatch {
        mode: Some(m),
        ..ConfigPatch::default()
    };
    // The user picks ambient (a frame is showing): taken after the 1 s
    // dwell, not at release + 5 s.
    r.e.apply_config(mode(Mode::Ambient));
    r.run_until(7.5, |_, _, _| {});
    assert!(
        r.wait_modes(&[0, 0]),
        "ambient taken after the dwell: {:?}",
        r.modes()
    );
    // Off → ambient: the pause path, no bypass.
    r.e.apply_config(mode(Mode::Off));
    r.run_until(8.0, |_, _, _| {});
    assert!(r.wait_modes(&[2, 2]));
    r.e.apply_config(mode(Mode::Ambient));
    r.run_until(12.3, |_, _, _| {});
    assert_eq!(r.settled_modes(), vec![2, 2], "inside the retake interval");
    r.run_until(13.5, |_, _, _| {});
    assert!(r.wait_modes(&[0, 0]));
    fs::remove_dir_all(dir).ok();
}

/// N3: a device missing from the list when the devices are released (an
/// OpenRGB rescan / USB replug) keeps its remembered mode — also in the
/// saved-modes file — and is restored as soon as it is listed again.
#[test]
fn a_device_missing_at_release_is_restored_when_it_returns() {
    let dir = temp_dir("gone");
    let path = dir.join("s.json");
    let mut r = Rig::new(Mode::Spectrum, None).adopt(&path);
    r.run_until(3.0, |e, now, _| loud(e, now));
    assert!(r.wait_modes(&[0, 0]));
    let full = r.stub.controllers();
    r.stub.set_controllers(vec![full[0].clone()]);
    r.stub.notify_device_list_updated();
    thread::sleep(Duration::from_millis(50));
    // Source gone while the strip is missing.
    r.run_until(7.0, |_, _, _| {});
    assert!(r.wait_modes(&[2]), "{:?}", r.modes());
    assert_eq!(r.e.status(r.now, 1).driven_count, 0);
    let held = load(&path).entries;
    assert_eq!(held.len(), 1, "the missing device stays remembered on disk");
    assert_eq!(held[0].key.name, "Stub Strip");
    // Back, still in Direct as OpenRGB left it: restored promptly.
    let mut back = r.stub.controllers();
    back.push(full[1].clone());
    r.stub.set_controllers(back);
    r.stub.notify_device_list_updated();
    thread::sleep(Duration::from_millis(50));
    r.run_until(7.5, |_, _, _| {});
    assert!(r.wait_modes(&[2, 2]), "{:?}", r.modes());
    r.run_until(7.6, |_, _, _| {});
    assert!(!path.exists(), "nothing left to remember");
    fs::remove_dir_all(dir).ok();
}

/// N3: a stop while a device is missing keeps it in the file; a successor
/// (fresh file, same boot) restores it when it is listed again.
#[test]
fn a_device_missing_at_stop_is_restored_by_the_successor() {
    let dir = temp_dir("gone-stop");
    let path = dir.join("s.json");
    let mut r = Rig::new(Mode::Spectrum, None).adopt(&path);
    r.run_until(3.0, |e, now, _| loud(e, now));
    assert!(r.wait_modes(&[0, 0]));
    let full = r.stub.controllers();
    r.stub.set_controllers(vec![full[0].clone()]);
    r.stub.notify_device_list_updated();
    thread::sleep(Duration::from_millis(50));
    let port = r.stub.port();
    let Rig { stub, e, .. } = r;
    let outcome = e.into_restore_parts().restore();
    assert_eq!((outcome.restored, outcome.failed), (1, 1), "{outcome:?}");
    assert_eq!(load(&path).entries.len(), 1, "kept for the next worker");
    let mut back = stub.controllers();
    back.push(full[1].clone());
    stub.set_controllers(back);
    // Successor: no source, same boot, file fresh.
    let t1 = Instant::now();
    let (store, loaded) = Store::open_with_boot_id(path.clone(), Some("b".into()));
    let mut cfg = EngineConfig::with_port(port);
    cfg.backoff_min = Duration::from_millis(50);
    let mut next = Engine::new(cfg, Settings::default(), None, t1, LogLimiter::new(t1))
        .adopt_store(store, loaded, t1, SystemTime::now());
    for k in 1..10 {
        next.tick(t1 + STEP * k, &|| false);
    }
    let end = Instant::now() + Duration::from_secs(5);
    while stub.controllers().iter().any(|c| c.active_mode != 2) && Instant::now() < end {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(stub.controllers().iter().all(|c| c.active_mode == 2));
    assert!(!path.exists());
    fs::remove_dir_all(dir).ok();
}

/// Leaves a saved-modes file as a SIGKILLed worker would: both devices in
/// Direct, the file naming their original Rainbow; then the user applies
/// their own Direct lighting (magenta), as an OpenRGB profile would.
fn leftovers_then_user_direct(stub: &StubServer, path: &std::path::Path, boot: &str) {
    let mut c = Client::connect(&kwe_openrgb::ClientConfig::with_port(stub.port())).unwrap();
    c.refresh().unwrap();
    let ids: Vec<_> = c.controllers().iter().map(|x| x.id).collect();
    for id in ids {
        c.take_control(id).unwrap();
    }
    let (mut store, _) = Store::open_with_boot_id(path.to_path_buf(), Some(boot.into()));
    store.write(c.saved_modes()).unwrap();
    drop(c);
    let mut list = stub.controllers();
    for x in &mut list {
        x.active_mode = 0;
        x.colors.fill(Rgb::new(255, 0, 255));
    }
    stub.set_controllers(list);
}

fn successor(stub: &StubServer, path: &std::path::Path, boot: &str, wall: SystemTime) -> Engine {
    let t = Instant::now();
    let (store, loaded) = Store::open_with_boot_id(path.to_path_buf(), Some(boot.into()));
    let mut cfg = EngineConfig::with_port(stub.port());
    cfg.backoff_min = Duration::from_millis(50);
    let settings = Settings {
        mode: Mode::Spectrum,
        ..Settings::default()
    };
    let mut e =
        Engine::new(cfg, settings, None, t, LogLimiter::new(t)).adopt_store(store, loaded, t, wall);
    for k in 1..20 {
        e.tick(t + STEP * k, &|| false);
    }
    e
}

/// N2: a leftover file from an earlier boot, or whose writer has not been
/// alive for longer than the freshness window, never overwrites the user's
/// new lighting — even Direct lighting that looks like ours.
#[test]
fn untrusted_leftovers_never_overwrite_new_direct_lighting() {
    let magenta = |stub: &StubServer| {
        stub.controllers()
            .iter()
            .all(|c| c.active_mode == 0 && c.colors.iter().all(|k| *k == Rgb::new(255, 0, 255)))
    };
    for case in ["earlier_boot", "stale", "fresh"] {
        let stub = StubServer::start(StubConfig::default()).unwrap();
        let dir = temp_dir(case);
        let path = dir.join("s.json");
        leftovers_then_user_direct(&stub, &path, "boot-1");
        let before = stub.received().len();
        let now = SystemTime::now();
        let (boot, wall) = match case {
            "earlier_boot" => ("boot-2", now),
            "stale" => ("boot-1", now + SAVED_FRESH + Duration::from_secs(1)),
            _ => ("boot-1", now + SAVED_FRESH - Duration::from_secs(5)),
        };
        let e = successor(&stub, &path, boot, wall);
        thread::sleep(Duration::from_millis(150));
        let sent: Vec<u32> = stub.received()[before..]
            .iter()
            .filter(|p| p.id == packet_id::UPDATE_MODE || p.id == packet_id::UPDATE_LEDS)
            .map(|p| p.id)
            .collect();
        if case == "fresh" {
            // Crash → respawn inside the window: the originals come back.
            let end = Instant::now() + Duration::from_secs(5);
            while stub.controllers().iter().any(|c| c.active_mode != 2) && Instant::now() < end {
                thread::sleep(Duration::from_millis(10));
            }
            assert!(stub.controllers().iter().all(|c| c.active_mode == 2));
        } else {
            assert!(sent.is_empty(), "{case}: sent {sent:?}");
            assert!(magenta(&stub), "{case}: user's lighting kept");
        }
        assert!(!path.exists(), "{case}: file settled");
        drop(e);
        fs::remove_dir_all(dir).ok();
    }
}

/// R5: the saved set is encoded only when it changes, not once per tick.
#[test]
fn persist_encodes_only_when_the_set_changes() {
    let dir = temp_dir("persist");
    let path = dir.join("s.json");
    let mut r = Rig::new(Mode::Spectrum, None).adopt(&path);
    r.run_until(2.0, |e, now, _| loud(e, now));
    assert!(r.wait_modes(&[0, 0]));
    let after_take = r.e.store().unwrap().encodes();
    // Two devices: one pre-switch write each + the recorded control mode.
    assert!(after_take <= 6, "{after_take} encodes for 2 takeovers");
    r.run_until(12.0, |e, now, _| loud(e, now));
    assert_eq!(
        r.e.store().unwrap().encodes(),
        after_take,
        "no encode in 200 steady ticks"
    );
    fs::remove_dir_all(dir).ok();
}
