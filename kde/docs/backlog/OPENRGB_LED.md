# OpenRGB LED sync with audio response — plan (in progress since 2026-11-05)

## Maintainer decisions and plan changes (2026-11-05)

- Maintainer, 2026-11-05: "Make openrgb optional but utilize if it is
  running." This replaces the "global opt-in, default off" policy below:
  - OpenRGB is never a hard dependency (`optdepends`, decision 4 = yes).
  - `led.enabled` defaults to **on** and means "use OpenRGB when its SDK
    server answers on loopback". The SDK server is off by default in
    OpenRGB, so turning it on is the opt-in.
  - No reachable SDK server is a normal quiet state (`led.status` =
    `server_absent`, bounded slow re-probe), never an error, a restart
    strike, or a log storm. The Settings switch still turns LED sync off.
- Decisions 1, 2, 3, 5: the orchestrator took the recommended option for
  the overnight run; the maintainer may revise.
  1. In-repo minimal sync client (`kwe-openrgb`), no new dependency.
  2. Default mode `ambient`.
  3. All controllers, with per-device disable.
  5. Restore the previous per-device mode on stop (best effort).
- L0 is folded into L1: the SDK server was off when work started, so the
  client is built against a stub server and ships a probe for the live
  check.

## Slice status

- **L0+L1 — done (uncommitted, 2026-11-05), live probe pending.**
  `crates/kwe-openrgb` (contract: `docs/OPENRGB_CLIENT.md`; provenance:
  `THIRD_PARTY.yml` `openrgb-sdk-protocol`). Protocol 1..=5, loopback-only
  config, bounded everything, `ServerAbsent` as a distinct quiet outcome,
  recording stub server (feature `stub`), `probe` and `stub_server`
  examples. 30 unit tests against the stub.
  - Plan corrections found: installed OpenRGB is now `1.1-1.1` (not
    1.0rc3); still SDK server off (6742 refused at 2026-11-05). The probe
    lives in L1 as an example, so L2's `--probe` can reuse the same
    output shape. Byte order / colour layout / `data_size` semantics are
    not in the published doc — the first live probe must confirm them.
  - Live check to run when the maintainer enables the SDK server:
    `cargo run -p kwe-openrgb --example probe` (read-only), then once
    `-- --set-color FF0000 --hold-ms 1500`.
  - Maintainer gate still open: dependency/provenance approval (here: no
    new dependency; protocol-compatible ledger entry).
- **L2 — done (uncommitted, 2026-11-05), live check pending.**
  `crates/kwe-led-worker` (binary + lib) and nested wire crate
  `crates/kwe-led-worker/protocol` (`kwe-led-protocol`, serde only — the
  daemon depends on this, never on the client). Contract:
  `docs/LED_PROTOCOL_V1.md` (CLI, stdin control lines, stdout `led_status`
  lines, states, exit codes, bounds, restore). Unit + stub-server
  integration tests. Measured ≈0.1 % of one core at 10 Hz on a 1080p frame.
  - Plan corrections found: (1) the stop grace must be ≥ 4 s, not the audio
    worker's 1 s (bounded OpenRGB op + 1.5 s restore); (2) the worker must
    NOT get a private network namespace (loopback would not be the host's);
    (3) the client's reply wait gave up after 64 queued ACKs — fixed in
    `kwe-openrgb` (it was the intermittent failure of an existing test);
    (4) remembered modes must survive a dropped connection, otherwise a
    reconnect re-saves the per-LED mode — `adopt_saved_modes` added;
    (5) "Worker runs only while … a renderer is live" (L3 below) is not
    needed for correctness: the worker holds then fades on `led_frame null`
    and idles cheaply; L3 may still choose it.
  - Live check: SDK server not listening at 2026-11-05 (`ss -ltn` shows no
    6742) → `kwe-led-worker --probe` and the ≤10 s ambient run are pending.
- **L3 — done (uncommitted, 2026-11-06), live check pending.**
  `crates/kwe-daemon/src/led.rs` (lifecycle manager driven from the
  supervisor tick), `led` record in `settings-v1.json`, `led.status` /
  `led.reprobe` RPCs, audio tee, frame-path sync, pause → mode `off`,
  daemon flags `--led-worker` / `--no-led-worker` / hidden `--led-sdk-port`,
  `kwe diagnose` LED line, PKGBUILD installs `kwe-led-worker` +
  `optdepends=('openrgb: ...')` (no pkgrel bump). Contract:
  `docs/SUPERVISOR_API_V1.md` §LED sync.
  - Design choices where the plan was open: (1) the worker runs whenever
    `led.enabled` is on, **not** only while a renderer is live (L2
    correction 5: the inventory and `server_absent` state stay visible to
    the UI; with no renderer it gets `led_frame null` and fades); (2) the
    manager lives inside the supervisor thread (like the F3/F4 detectors),
    so frame path and pause verdict are read from the supervisor's own
    state every tick — level-triggered, no hooks in the apply / playlist /
    rollback / pause paths; nothing in it waits (async SIGTERM→grace→SIGKILL
    reaping; only daemon shutdown waits, in parallel with renderer stop);
    (3) **pause → `mode: off`** while `(pause_when_covered AND covered) OR
    (pause_when_display_off AND asleep)`, even with no renderer live —
    orchestrator choice, flagged for maintainer review; (4) the audio tee
    is independent of the wallpaper's `audio` grant; (5) restart budget 3 per
    10 min, 1 s apart, reset by toggling `enabled` or by `led.reprobe`; a
    worker silent 30 s is restarted (one strike), silent 15 s reports
    `degraded`/`worker_unresponsive`; (6) `led.reprobe` is rate-limited to one
    accepted call per 2 s and doubles as "try the worker again" when it is
    `not_installed` or out of restarts; (7) settings key `disabled_devices`
    (the plan's `devices[]`), stored sorted+unique, ≤ 128.
  - Plan corrections found: (1) audio only reaches the LED worker while the
    daemon runs with `--audio-capture`, which the packaged unit does **not**
    pass (default off) — `spectrum` / `ambient+pulse` get no audio on a
    stock install; L4 must surface this or a later slice must start capture
    while an audio-driven LED mode is selected; (2) audio frames are also
    generation-gated (a renderer must have been promoted once), so spectrum
    mode with no wallpaper ever applied stays dark; (3) smoke scripts that
    start a daemon from `target/debug` will now also start the LED worker
    against 127.0.0.1:6742 by default — harmless while no SDK server runs,
    but such a smoke would drive the maintainer's real devices for its
    duration (and restore them) if OpenRGB's server is on; those scripts
    should pass `--no-led-worker` (scripts were outside L3's file scope);
    (4) `TimeoutStopSec=5s` in the unit is enough: systemd SIGTERMs the
    whole cgroup at once, and the worker's worst case is ≈ 3.3 s.
  - Live check: 6742 not listening at 2026-11-06 (`ss -ltn`) → the real
    `--probe` and ≤ 10 s run are still pending (same as L1/L2).
- **L4 — done (uncommitted, 2026-11-06), live check pending.** Manager
  Settings → "LED sync" (`apps/kwe-manager/qml/LedSyncSection.qml`,
  `src/ledsyncclient.{h,cpp}`, context property `ledSync`; states and
  wording in `docs/UX_DESIGN.md` §LED sync). Consumes `led.status` /
  `led.reprobe` / `settings.set {led}` exactly as documented; C++ client
  tests against a fake daemon (`tests/ledsyncclienttest.cpp`); offscreen
  screenshots against a daemon from this tree with the stub SDK server.
  - Design choices where the plan was open: (1) the switch covers "off", so
    the mode picker lists only Ambient / Spectrum / Ambient + pulse; a
    stored `off` shows "Off" in the picker; (2) brightness/rate edits are
    merged and debounced 300 ms into one `settings.set`, discrete edits
    (switch, mode, device) are sent at once, all with only the changed
    `led` fields; (3) a device checkbox writes the whole
    `disabled_devices` list from the effective list, so ids of devices
    not connected right now stay disabled; (4) a write that fails to reach
    the daemon is not retried (unlike the audio switch): the control
    returns to the last confirmed value and the page shows the daemon as
    unreachable; (5) "Check again" is disabled for 2 s after an accepted
    call (the daemon's limit) and for `retry_after_ms` after
    `rate_limited`; (6) the audio hint shows only in `active`/`degraded`
    with an audio mode and `worker_status.audio.fresh == false`; (7) new
    `kwe-manager --start-page installed|workshop|settings|led` for
    screenshots and deep links.
  - Contract notes for later slices: `led.reprobe` documents
    `retry_after_ms` only for `rate_limited` (the UI uses a 1 s cooldown
    for `worker_restarting`); the stock unit still runs without
    `--audio-capture`, so on a stock install audio modes show the
    no-audio hint (L3 correction 1).
- **Review fixes A — done (uncommitted, 2026-11-06), live check pending.**
  Adversarial review of L1–L4; worker + client + minimal daemon wiring.
  Bar: the user's own lighting is never lost or blanked by LED sync. Each
  regression test below was run against pre-fix HEAD `1ae2185` (scratch
  worktree; tests reduced to the old API) and **failed** there.
  - MUST-FIX 1 — CLOSED. No device is taken over until the mode has a real
    source (valid frame ≤ 2 s for the ambient modes, audio for spectrum);
    when it goes away: 2 s hold, then release (restore, as for `off`), no
    fade to black; status `active` + reason `waiting_for_source`, new
    `driven_count`. Daemon passes the reason through; manager row + exact
    count. Tests `no_source_never_takes_or_blanks_devices`,
    `source_appearing_is_driven_and_counted`,
    `spectrum_reacts_to_audio_lines_and_releases_without_them`.
  - MUST-FIX 2 — CLOSED. `kwe-led-worker --saved-modes <ABS>` (atomic,
    0600, ≤ 8 MiB, versioned JSON) written before each take-over, removed
    after a full restore, adopted on start; stale entries (device left the
    recorded per-LED mode) forgotten; corrupt file → one log line, removed.
    Daemon passes `<state_dir>/led-saved-modes.json` (one line in
    `supervisor.rs`: `led.set_state_dir(&config.state_dir)`). Direct-origin
    devices remembered as Direct + colours. Tests
    `unclean_death_then_next_worker_restores_the_originals`,
    `saved_modes_file_is_adopted_without_a_source_and_corrupt_is_ignored`,
    daemon `real_worker_killed_mid_drive_is_cleaned_up_by_its_successor`,
    `saved::tests::*`.
  - MUST-FIX 3 — CLOSED. Restore keeps the entry unless device/mode gone;
    one refresh + retry on a list change; off/release/disable paths keep
    failures and carry them across connections and into the file. Tests
    `restore_all_keeps_modes_when_the_connection_is_lost`,
    `restore_all_under_a_notification_storm_never_drops_a_mode`,
    `restore_one_keeps_mode_on_error`.
  - R1 — CLOSED. Stop checked between connect and enumerate, between
    devices (0.5 s per-tick budget, round-robin), before restore; no driving
    in the connecting tick; signals via a `sigwait` thread. Documented worst
    case 3.35 s. Test `sigterm_during_slow_take_over_of_many_devices_is_bounded`
    (old: 9.66 s, new: 0.45 s).
  - R3 — CLOSED. No frame reads / no polling while disconnected; 4 Hz idle
    while waiting, 1 Hz in `off`. Release build, 10 s, all threads:
    disconnected 201 → 6, mode off 201 → 14, connected no source 201 → 44.
    Test `idle_without_openrgb_is_quiet_and_still_stops_promptly`.
  - R4 — CLOSED. `MAX_ENUMERATION_BYTES` 16 MiB (estimated retained size),
    `LimitExceeded {what: "enumeration bytes"}`. Test
    `enumeration_total_bytes_are_budgeted`.
  - NIT log injection — CLOSED (`{:?}` names + control chars escaped in
    `log.rs`; test `server_strings_cannot_inject_log_lines`).
  - NIT `--probe` budget — CLOSED (enumeration ≤ 2 s → ≈ 3.3 s worst case).
  - NIT ids — PARTIAL. Identical `DeviceKey`s saved/restored separately
    (occurrence); settings id drops `location` when a serial is present
    (**id change** for such devices); serial-less devices still change id
    when their location changes (documented limit).
  - Not in this slice (second agent): smoke scripts, `check.sh`, settings
    quarantine, audio-capture policy, restart-budget docs, daemon test
    skipping.
- **Review fixes B — done (uncommitted, 2026-11-06), live check pending.**
  Second review slice: scripts, settings isolation, audio for the audio
  modes, daemon NITs.
  - MUST-FIX 4 — CLOSED. Every daemon launch under `scripts/` passes
    `--no-led-worker` (smoke-supervisor/-audio/-apply ×2/-input-preview/
    -plasma-display/-workshop-cache/-playlist-restart/-video/-web/
    -live-apply/-ui ×2); `smoke-led.sh` uses a stub `--led-sdk-port`;
    `dev-run.sh` defaults to `--no-led-worker`, opt-in `KWE_DEV_LED=1`
    (real devices, marked `# led-guard:`) or `KWE_DEV_LED_SDK_PORT=<port>`
    (6742 refused). `check.sh` guard (awk over backslash-joined lines) fails
    on any launch without `--no-led-worker` / `--led-sdk-port` / a
    `# led-guard:` marker; verified it flags 8 launches in mutated copies
    and passes the tree.
  - R2 — CLOSED. An invalid `led` record resets only `led` (one
    `event=settings.led_reset` line); `audio_output` and both pause
    policies survive; unknown fields inside `led` are ignored; bad JSON /
    top-level shape / schema still quarantines the whole file. Not
    surfaced as a separate API field (no existing way; `settings.get` /
    `led.status` show the defaults actually in effect). Downgrade below
    L3 quarantines a file with `led` (documented in SUPERVISOR_API_V1.md;
    a package install script cannot warn on downgrade — the old package's
    script runs — so no `.install` line). Test
    `settings::tests::led_bounds_are_enforced_on_patch_and_a_bad_led_record_resets_only_led`.
  - R5 — CLOSED. `check.sh` and PKGBUILD `check()` build `kwe-led-worker`
    first and set `KWE_REQUIRE_LED_WORKER=1` (missing binary = test
    failure); a bare `cargo test -p kwe-daemon` without the worker prints
    `skipped: worker not built (...)` straight to stderr (visible, not
    captured). Verified both by moving the binary aside.
  - Audio for the audio modes — CLOSED (with one out-of-scope file, see
    below). Capture runs while `--audio-capture` OR LED sync wants audio
    (enabled, usable worker, mode spectrum / ambient+pulse), following
    `settings.set` live (`audio.rs`: shared demand flag published by the
    supervisor every tick; start/stop on the 100 ms runtime tick); default
    `ambient` never captures. `audio.status` gains additive `active` +
    `reasons` (`audio_capture_flag`, `led_sync`). Renderer-less LED audio:
    the capture worker now pushes `generation: 0` frames while nothing was
    ever promoted; the daemon gives them to the LED worker only (running,
    audio mode, not paused), else rejects them and the worker backs off to
    one push / 500 ms. Renderer gates unchanged. **Scope note:** this needed
    a ~20-line change in `crates/kwe-audio-worker` (it never pushed at
    generation 0), outside the listed file scope. Tests
    `audio::tests::led_demand_starts_and_stops_capture_without_the_flag`,
    `audio::tests::the_flag_keeps_capture_running_whatever_led_wants`,
    `supervisor::tests::led_only_audio_never_reaches_an_ungranted_wallpaper`,
    `led::tests::audio_demand_follows_mode_enable_and_configuration`,
    kwe-audio-worker `unpromoted_frames_are_pushed_with_generation_zero_when_accepted`,
    `rejected_unpromoted_frames_back_off_to_the_poll_interval`. Manager hint
    reworded (no "audio capture may be off / no wallpaper running").
  - NIT restart budget — CLOSED. Doc and code comment now say "at most 3
    restarts per 10 min; the 4th unexpected exit exhausts it"; test
    `restart_budget_allows_three_restarts_and_the_fourth_exit_exhausts_it`
    counts 4 spawns.
  - NIT non-executable binary — CLOSED. `not_executable` reason (mode bits,
    or EACCES at exec), no strike, `led.reprobe` respawns; manager shows
    its own text (one QML branch — outside the "hint + retry" scope, needed
    so the fallback "started with --no-led-worker" is not shown). Test
    `a_non_executable_binary_is_a_quiet_unavailable_state_without_a_strike`.
  - NIT shutdown bounds — CLOSED by reasoning (an unreapable child cannot
    be made in a unit test): a SIGKILLed worker not reaped 2 s later is
    dropped with one `event=led.worker.unreaped` line, in ticks and in
    `finish_shutdown` (plus a hard backstop); a `try_wait` error is logged
    once per worker and no longer blocks the SIGKILL escalation.
  - NIT timing-fragile tests — CLOSED. Trap test waits for a `.ready`
    marker written after `trap`; `quiet_port(slot)` picks deterministic
    ports in 20000..31999 (below the ephemeral range, so no parallel
    `bind(0)` can take them), like the worker crate's tests.
  - NIT `led.reprobe` `retry_after_ms` for `worker_restarting` — CLOSED
    (time to next spawn or remaining stop grace); manager uses it, 1 s
    fallback for older daemons. Tests `reprobe_while_restarting_says_when_to_retry`,
    `LedSyncClientTest::reprobeWhileRestartingHonoursRetryAfter`.
  - NIT credits — CLOSED in `kde/README.md` (OpenRGB SDK protocol line,
    pointing at THIRD_PARTY.yml `openrgb-sdk-protocol`). OPEN:
    `docs/PROVENANCE.md`'s ledger table has no OpenRGB row (file outside
    this slice's scope).
- **Review fixes C1 — done (uncommitted, 2026-11-06), live check pending.**
  Worker/client slice of the third review (C2 — daemon settings, capture
  demand, scripts guard, manager, Workshop — follows separately). Each
  regression test below was confirmed to FAIL with its fix reverted in
  place (the pre-fix logic restored, everything else unchanged).
  - N1 silence — CLOSED. A frame whose loudest perceptual band is < 0.05 is
    silence; `spectrum` takes over only on sound and releases after 3 s of
    silence (gaps shorter than that keep driving); `ambient+pulse` returns
    to steady ambient after 3 s of silence instead of the 0.2 floor (it was
    dimmed to ≈ 20 % — confirmed, fixed). Status: additive `audio.silent`
    (`fresh:true, silent:true` = nothing playing; `fresh:false` = no
    frames); reason stays `waiting_for_source`, so no new manager string.
    Tests `engine::tests::spectrum_silence_releases_instead_of_painting_black`,
    `ambient_pulse_is_steady_ambient_in_silence`.
  - N2 stale saved-modes file — CLOSED with a documented limit. The file
    carries `boot_id`; its mtime is touched every 5 s while it exists; a
    successor adopts only same-boot entries whose writer was alive within
    60 s, else forgets the whole file (one line) and leaves the devices
    alone. Limit (doc): inside the 60 s window brand-new Direct lighting
    cannot be told from our leftovers. Tests
    `engine::tests::untrusted_leftovers_never_overwrite_new_direct_lighting`,
    `saved::tests::provenance_is_judged_by_boot_and_liveness`,
    `touch_refreshes_mtime_without_rewriting_and_only_while_the_file_exists`,
    integration `stale_or_foreign_boot_leftovers_leave_new_direct_lighting_alone`.
  - N3 device missing at restore — CLOSED. `DeviceGone` keeps the entry
    (in memory, in the file, across a successor); restored at the next tick
    once listed again in the recorded control mode (forgotten if it comes
    back in another mode); logged once; absent devices are no longer
    `controlled`. Limit (doc): missing at a stop with no worker running
    within 60 s of its return → stays frozen. Tests
    `engine::tests::a_device_missing_at_release_is_restored_when_it_returns`,
    `a_device_missing_at_stop_is_restored_by_the_successor`, client
    `device_missing_at_restore_is_kept_and_restored_when_back`, integration
    `a_device_missing_when_the_successor_starts_is_restored_when_back`.
  - R4 hysteresis — CLOSED. Takeover dwell 1 s; ≥ 5 s from a release to the
    next takeover; a mode change between non-off modes or enabling a device
    skips it once (off → on, the pause path, does not). Reproducer counts
    (virtual clock, same scenario): flap 6 → 3 takeovers in 20 s, pause
    toggling every 3 s 7 → 4. Tests `engine::tests::flapping_source_cycles_are_bounded`,
    `pause_toggling_is_bounded_by_the_minimum_retake`,
    `a_user_mode_switch_skips_the_minimum_retake_once`, integration
    `flapping_audio_is_bounded_end_to_end`, daemon
    `real_worker_drives_the_stub_pauses_off_and_restores_on_disable` (no
    retake within 2.5 s of resume).
  - R5 per-tick encode — CLOSED. Persist only when the remembered set
    changed (client `saved_revision` + engine epoch); linear encoder (worst
    case 12 ms release / 470 ms debug, was ≈ 1.2 s release); a tick
    interrupted by a stop no longer persists (the restore path writes the
    file), so the stop bound stays 3.35 s. Tests
    `engine::tests::persist_encodes_only_when_the_set_changes`,
    `saved::tests::encoding_is_bounded_dropping_direct_colours_first` (time).
  - R6 all-black Direct — CLOSED as far as the protocol allows (PARTIAL in
    effect): zeros are never re-sent; such a device is still taken over and
    after release shows the last wallpaper colours, frozen (no SDK way back
    to firmware control). Documented in LED_PROTOCOL_V1.md and UX_DESIGN.md.
    Test `all_black_direct_origin_is_not_repainted_black`.
  - Hidden worker flags `--takeover-dwell-ms`, `--min-retake-ms`,
    `--silence-release-ms` (tests only); the worker integration suite runs
    them scaled, the daemon tests and `smoke-led.sh` run the defaults
    (smoke 15.9 s → 22 s).
- **L5 — done (uncommitted, 2026-11-06), live check pending.**
  `scripts/smoke-led.sh` (opt-in `KWE_RUN_LED_SMOKE=1` in `check.sh`), with
  test-only helpers `crates/kwe-daemon/examples/led_smoke_stub.rs` (the
  recording stub driven over FIFOs: late `listen`, `snapshot`) and
  `led_smoke_renderer.rs` (solid RGB 0,160,255 at even fps, fast colour
  cycle at odd fps — `kwe-test-renderer` has no solid mode, and the worker
  does not re-send unchanged colours, so the rate case needs motion).
  Private state/socket/runtime dirs, ports in 20000..31999 (6742 refused),
  `--audio-worker` pointed at a missing path, trap cleanup of daemon /
  stub / every process carrying the smoke's paths or port. Cases, all
  CLOSED: server absent (`server_absent`, reason null, 0 strikes, no
  error-level lines, worker alive); server appears without a wallpaper
  (`active`/`waiting_for_source`, no 1050/1051/1100/1101 packets, modes
  untouched, `driven_count` 0); solid wallpaper (every LED within ±12 of
  the colour, `driven_count` 2, Direct); rate (8 Hz → 24 UpdateLEDs per
  device in 3 s, bound 26); disable one device (restored to Rainbow, no
  further updates, other device continues, `driven_count` 1);
  `enabled:false` (worker reaped, every device restored, restore summary);
  opt-in gating across a daemon restart (no worker, no new connection, no
  process); worker SIGKILLed mid-drive (1 restart, `signal_9`, successor
  restores Rainbow, saved-modes file removed); daemon SIGTERM (restored, no
  strays). Wall time 15.7–15.9 s; passed 3/3 consecutive runs.
  - Not covered: audio modes end to end (would need a null-sink capture in
    the LED smoke); live devices (6742 still not listening).

Requested 2026-08-29: drive OpenRGB-controlled devices (keyboard, case
LEDs, strips) from the running wallpaper and/or the captured system
audio — the analog of Wallpaper Engine's Windows "LED integration",
built on the daemon's existing isolation and bounds.

## What already exists (measured on the target machine)

- **Audio**: `kwe-audio-worker` already captures system audio via
  `pw-record` and produces normalized 16/32/64-band stereo `AudioFrame`s
  (`kwe_core::audio::analyze_stereo`), delivered to the daemon over
  `audio.forward` and forwarded latest-wins to the active renderer. The
  LED path can tee this exact stream — no new capture code.
- **Frames**: every renderer publishes BGRA frames into a two-slot
  seqlock file (`kwe-frame-protocol`); the daemon knows the active
  worker's `frame_file` path. A read-only consumer can sample it at its
  own (low) rate without touching any renderer.
- **Settings surface**: `settings-v1.json` + `settings.get/set` RPCs +
  the manager's Settings page (added for the wallpaper-audio switch) are
  the natural home for LED configuration.
- **OpenRGB on the target**: `openrgb 1.0rc3` installed, GUI running
  (`--startminimized`) but the SDK server is OFF (port 6742 refused).
  The integration requires the SDK server (GUI toggle, `--server`, or
  `openrgb.service`); the UI must surface this state with the fix hint.

## Architecture

One new isolated worker, `kwe-led-worker`, owned by the daemon exactly
like `kwe-audio-worker` (spawn, bounded restarts, SIGTERM+grace,
status): no LED or network code in renderers, the daemon, or
plasmashell. Data flows IN over stdin (NDJSON, same wire crate as
renderer input); LED output goes to the OpenRGB SDK on 127.0.0.1:6742.

Sources → mapping → sink, all inside the worker:

- **Ambient source**: open the active wallpaper's frame file read-only,
  take a stable seqlock snapshot at ≤10 Hz, downsample edge regions /
  dominant color per configured zone (ambilight-style; cheap: a few
  thousand pixel reads per tick).
- **Audio source**: the daemon tees each accepted `audio.forward` frame
  to the worker's stdin (latest-wins, exactly the renderer discipline).
  Bands → per-zone hue/brightness (spectrum across a keyboard, bass
  pulse for single-zone devices).
- **Modes**: `off | ambient | spectrum | ambient+pulse` (ambient color,
  audio-modulated brightness). Global brightness cap. Update rate
  bounded (default 10 Hz, max 20 — cheap RGB controllers stall on USB
  storms) with latest-wins coalescing.
- **Sink**: OpenRGB SDK client. On connect: negotiate protocol version,
  enumerate controllers/zones, honor a device allowlist. `UpdateLeds`
  per tick. Disconnect/absent server → bounded reconnect backoff and a
  truthful `led.status`; on worker stop, restore devices to their prior
  mode (best effort) so the user's static lighting comes back.

Policy (superseded 2026-11-05, see the top of this file): LED control is
outward-facing device control → ~~global opt-in, default off~~. The SDK protocol has no
authentication; the client must only ever dial loopback.

## Slices (issue-sized, in order)

- **L0 — spike + dependency decision (small).** Hand-rolled probe
  against the live SDK server (enumerate, set colors, restore). Decide:
  minimal in-repo sync client (~300 lines, protocol ≥3; fits the
  repo's bounded-deps posture) vs the async `openrgb` crate (pulls
  tokio into a threaded daemon — likely rejected). **Maintainer gate:
  dependency/provenance approval before any dependent code merges.**
- **L1 — `kwe-openrgb` client crate (medium).** Sync, bounded
  (timeouts, max packet size, max controllers/leds), typed packets for
  the needed subset only. Unit tests against an in-process stub server.
- **L2 — `kwe-led-worker` (large).** Sources, mapping modes, sink,
  rate bounds, `--probe` (JSON: server reachable, controller inventory)
  for diagnostics, standalone CLI mode for development. Unit tests for
  the mapping math; stub-server integration test.
- **L3 — daemon integration (medium).** `led.rs` lifecycle manager
  (clone the `audio.rs` pattern: restart budget, status); settings
  extension (`led: {enabled, mode, brightness, rate_hz, devices[]}`)
  with the existing quarantine-on-corrupt posture; `led.status` RPC;
  hand the worker the active frame path on renderer transitions and
  tee audio frames. ~~Worker runs only while enabled AND a renderer is
  live.~~ (L3: runs while enabled; see slice status.)
- **L4 — manager UI (medium).** Settings page section: enable switch,
  mode picker, brightness/rate, detected-device list with checkboxes,
  and the actionable degraded states ("OpenRGB SDK server is not
  running — enable it in OpenRGB's settings or `openrgb --server`").
- **L5 — verification (small).** `scripts/smoke-led.sh` against a fake
  SDK server that records packets (asserts: opt-in gating, rate bound,
  restore-on-stop); live verify on the real devices.
- **L6 (defer) — per-wallpaper LED profiles.** WE project.json LED
  projection config compatibility; research scope first — global modes
  cover the request.

Rough order: L0 half a day; L1+L2 the bulk (2–3 days together); L3+L4
a day; L5 half a day.

## Open decisions for the maintainer

1. Dependency: in-repo minimal client (recommended) vs `openrgb` crate.
2. Default mode once enabled (suggest `ambient`).
3. Device selection default: all controllers vs explicit allowlist
   (suggest all, with per-device disable).
4. Whether the package should `optdepends=('openrgb: LED sync')`.
5. Restore-on-stop semantics: previous per-device mode vs a configured
   "idle" color.

## Risks

- OpenRGB 1.x SDK protocol versioning (negotiate, refuse unknown).
- Slow/flaky USB controllers: keep per-device update budgets; a stalled
  device must not stall the tick (per-device latest-wins).
- Frame-file lifecycle races on wallpaper switch (reopen on ENOENT /
  generation change; the seqlock read discipline already handles the
  writer side).
- The worker must never hold the frame file open across renderer
  quarantine/replace in a way that blocks cleanup (open per tick or
  reopen on invalidation).
