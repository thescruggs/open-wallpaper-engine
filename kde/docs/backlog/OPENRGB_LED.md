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
- L5: pending.

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
