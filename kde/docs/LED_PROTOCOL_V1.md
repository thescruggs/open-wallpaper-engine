# LED worker protocol v1 (`kwe-led-worker`)

Contract between `kwe-daemon` (slice L3, `crates/kwe-daemon/src/led.rs`) and the isolated
OpenRGB LED sync worker (slice L2). Everything an integrator needs is here;
the code is `crates/kwe-led-worker` (binary + library) and the wire types are
`crates/kwe-led-worker/protocol` (package `kwe-led-protocol`). Plan:
`docs/backlog/OPENRGB_LED.md`. SDK client: `docs/OPENRGB_CLIENT.md`.

## Where the types live (and why)

- `kwe-led-protocol` (nested crate, serde only, no I/O, no network code) owns
  the stdin control lines, the stdout status lines, `device_id`, and every
  bound below. The daemon depends on **this crate only**, so no OpenRGB/LED
  code is linked into the daemon (`docs/backlog/OPENRGB_LED.md`
  §Architecture).
- `kwe-input-protocol` is **not** widened: renderers never see LED lines. The
  worker accepts the existing renderer `audio_bands` line byte-for-byte, so
  the daemon can tee the exact line it already writes to the renderer.

## Process model

- One process per daemon, no children, no `unsafe` beyond one SIGTERM/SIGINT
  handler install (same pattern as `kwe-audio-worker`).
- Network: TCP to `127.0.0.1:<port>` only (the client has no address field).
  **Do not run it in a private network namespace** — loopback there is not
  the host's loopback and OpenRGB would look permanently absent.
- Needs no environment variables, no `HOME`, no display; works under
  `env_clear()`.
- Recommended spawn (clone `audio.rs`): `setpgid(0,0)`, `PR_SET_PDEATHSIG =
  SIGTERM` (SIGTERM, not SIGKILL, so devices get restored), parent-pid check,
  `PR_SET_NO_NEW_PRIVS`; stdin = pipe (control), stdout = pipe (status),
  stderr = inherited (bounded diagnostics).

## Command line

| Flag | Default | Bounds | Meaning |
|---|---|---|---|
| `--port N` | 6742 | 1..=65535 | OpenRGB SDK port on 127.0.0.1 |
| `--mode M` | `ambient` | `off \| ambient \| spectrum \| ambient+pulse` | initial mode |
| `--brightness N` | 100 | 0..=100 | initial brightness cap, % |
| `--rate-hz N` | 10 | 1..=20 | initial LED update rate |
| `--frame PATH` | none | absolute, ≤4096 B, no `.`/`..` | initial frame file |
| `--disable-device ID` | none | 16 lowercase hex; repeatable | initially disabled device |
| `--probe` | | | print one `led_probe` JSON line and exit (below) |
| `--timeout-ms N` | 1000 | 1..=30000 | `--probe` I/O timeout |
| `--standalone` | off | | development: stdin EOF does not stop |
| `--synthetic-audio` | off | | development: internal 32-band test audio (2 Hz bass pulse + sweeping peak) |
| `--duration-s N` | none | 1..=3600 | stop (and restore) after N s |
| `--backoff-min-ms N` (hidden) | 1000 | 50..=60000 | first re-probe delay |
| `--backoff-max-ms N` (hidden) | 60000 | ≥ min, ≤60000 | re-probe delay cap |

The daemon should pass the initial settings as flags (no startup race) and
use stdin lines for every later change. Usage errors exit 2 before anything
is opened.

Development run without the daemon (any OpenRGB or the stub):

```bash
cargo run -p kwe-openrgb --features stub --example stub_server   # prints {"port":N}
kwe-led-worker --port N --standalone --synthetic-audio \
    --mode ambient+pulse --frame /path/to/frame.bin --duration-s 10
```

## Exit codes

| Code | When |
|---|---|
| 0 | SIGTERM/SIGINT, `led_stop`, stdin EOF (not `--standalone`), `--duration-s` elapsed — always after the bounded restore |
| 1 | could not start its stdin thread (resource exhaustion) |
| 2 | usage error |
| `--probe`: 0 / 3 / 1 / 2 | reachable / server absent / other error / usage |

An absent, refusing, stalling, or misbehaving OpenRGB server is **never** an
exit: it is a status. A non-zero exit or a signal death is a real crash and
may count toward L3's restart budget; a 0 exit after `stdin_eof` is the
daemon's own doing.

## stdin: control lines (daemon → worker)

NDJSON, one JSON object per line, ≤ **4096 bytes** including the newline
(`kwe_led_protocol::MAX_LINE_BYTES`, the renderer input cap). Encoders:
`encode_config`, `encode_frame`, `encode_status_request`, `encode_reprobe`,
`encode_stop`; the worker decodes with `decode_control`.

```json
{"version":1,"type":"audio_bands","sequence":4,"left":[...16|32|64 floats 0..1],"right":[...]}
{"version":1,"type":"led_config","mode":"ambient+pulse","brightness":60,"rate_hz":15,"disabled_devices":["0123456789abcdef"]}
{"version":1,"type":"led_frame","path":"/run/user/1000/kwe/frames/worker-7.bin"}
{"version":1,"type":"led_frame","path":null}
{"version":1,"type":"led_status_request"}
{"version":1,"type":"led_reprobe"}
{"version":1,"type":"led_stop"}
```

| Type | Semantics |
|---|---|
| `audio_bands` | The renderer line from `kwe-input-protocol`, unchanged and validated by its own decoder. Latest wins; `sequence` is ignored. Fresh for 500 ms, then treated as silence. |
| `led_config` | Every field optional; a present field replaces the current value, absent keeps it. `mode` ∈ `off`, `ambient`, `spectrum`, `ambient+pulse`; `brightness` 0..=100; `rate_hz` 1..=20; `disabled_devices` (≤128 ids) **replaces the whole set** (`[]` enables all). Unknown fields or out-of-range values reject the whole line (nothing applied). |
| `led_frame` | The active renderer's frame file; `null` = no renderer live. Absolute UTF-8 path ≤4096 bytes, no NUL, no `.`/`..` components. Existence is not required (checked per tick). |
| `led_status_request` | Emit a status line as soon as the rate limit allows (≤250 ms). |
| `led_reprobe` | While not connected: probe now and reset the backoff (e.g. the UI's "Retry"). No-op while connected. |
| `led_stop` | Restore devices and exit 0 (same path as SIGTERM). |

Daemon-side rules:

- Write with the renderer discipline: nonblocking, one pending line per
  stream, latest wins. Control lines are idempotent "set" messages, so a
  replaced pending `led_config` must be **merged** (or re-sent whole), never
  dropped; `ConfigPatch::merge` does that. Audio may simply be replaced.
- Lines of other known renderer types (`pointer_position`, `media_state`,
  `render_pause`, ...) and future types with a numeric `version` are
  **ignored** and counted (`stdin_ignored`); they are not errors.
- Rejected lines (oversized, not JSON, invalid UTF-8, wrong `version`, bad
  values, missing `version`) are dropped and counted (`stdin_rejected`); the
  worker never exits on input. An oversized line is skipped up to its newline
  without being buffered (worker memory stays ≤ 4096 + 8192 bytes for
  input).
- Inside the worker everything is latest-wins too: config patches are
  merged, the frame path and audio are replaced, so no queue can build.
- **stdin EOF stops the worker** (restore, exit 0). Closing the pipe is
  therefore a valid way to stop it.

## stdout: status lines (worker → daemon)

Only `led_status` lines are written to stdout (diagnostics go to stderr).
Each line is one JSON object ≤ **32 KiB** including the newline
(`MAX_STATUS_LINE_BYTES`); decode with `kwe_led_protocol::decode_status`.

Emission policy (rate-limited and bounded):

- immediately at start, before the first probe (`state: "connecting"`), so
  the daemon knows within milliseconds that the worker runs; then whenever a
  *material* field changes
  (anything but `seq`, `counters`, `server.next_probe_ms`, `frame.sequence`),
  on `led_status_request`, and at least every **5 s** (heartbeat with fresh
  counters);
- never more than one line per **250 ms** (≤ 4 lines/s);
- written by a dedicated thread through a one-slot latest-wins buffer: if the
  daemon does not read, unread lines are replaced, the worker never blocks
  and never queues;
- a final line with `state: "stopped"` before exit (flush bounded to 300 ms).

Reader guidance for L3 (same shape as the supervisor's ack reader): read
nonblocking, keep at most 32 KiB of an unterminated line (discard beyond),
keep only the **latest** complete line and answer `led.status` from it; a
heartbeat older than ~15 s while the process lives means it is wedged.

```json
{"version":1,"type":"led_status","seq":12,"state":"active","reason":null,
 "mode":"ambient","brightness":100,"rate_hz":10,
 "server":{"port":6742,"reachable":true,"protocol":5,"server_protocol":5,"next_probe_ms":null},
 "frame":{"state":"ok","sequence":4711,"width":1920,"height":1080},
 "audio":{"fresh":true,"bands":64},
 "device_count":2,
 "devices":[{"id":"9d0c2b7e5a1f3c44","name":"Keyboard","vendor":"...","serial":"...","location":"...",
             "type":5,"leds":104,"enabled":true,"controlled":true,
             "zones":[{"name":"Keys","type":"matrix","leds":104,"matrix":[6,22]}],"zones_truncated":false}],
 "devices_truncated":false,
 "counters":{"ticks":130,"updates_sent":256,"update_errors":0,"frames_read":130,"connects":1,
             "disconnects":0,"stdin_lines":3,"stdin_rejected":0,"stdin_ignored":0,"audio_frames":394}}
```

| Field | Meaning |
|---|---|
| `seq` | +1 per emitted line; a gap means the reader missed lines the worker replaced while unread (harmless: every line is a full snapshot) |
| `state` | see below |
| `reason` | `null`, or for `degraded`: `timeout`, `disconnected`, `io_error`, `protocol_error`, `unsupported_version`, `device_error`, `error`; on the final `stopped` line: `signal`, `led_stop`, `stdin_eof`, `duration` |
| `mode`, `brightness`, `rate_hz` | the settings in effect |
| `server.reachable` | connected right now |
| `server.protocol` / `server_protocol` | negotiated version / the server's own highest (while connected) |
| `server.next_probe_ms` | while not connected: time to the next probe |
| `frame.state` | `none` (no path), `ok`, `absent` (no file), `invalid` (bad header, truncated, symlink, not a regular file), `busy` (writer mid-publish on every attempt, or nothing published yet) |
| `frame.sequence/width/height` | of the last accepted snapshot |
| `audio.fresh`, `audio.bands` | an `audio_bands` line arrived within 500 ms; its band count |
| `device_count` | controllers the server reported |
| `devices[]` | ≤32 listed; `devices_truncated` when more exist or the line had to shrink. `id` is the key for `disabled_devices`; display strings are cut to 80 chars; ≤16 zones per device (`zones_truncated`). Zone `type` ∈ `single`, `linear`, `matrix`, `unknown`; `matrix` = `[height, width]` |
| `enabled` | not in `disabled_devices` |
| `controlled` | the worker switched it to its per-LED mode (and will restore it) |
| `restore` | final line only: `{attempted, restored, failed, timed_out}` |

### States

| State | Meaning | UI hint (L4) |
|---|---|---|
| `connecting` | first line only, before the first probe finished | spinner |
| `server_absent` | connection refused on loopback — **normal and quiet** | "OpenRGB SDK server is not running — enable it in OpenRGB (SDK Server tab) or run `openrgb --server`" |
| `active` | connected + enumerated; enabled devices are driven (zero devices is still `active`, `device_count: 0`) | device list |
| `off` | connected, mode `off`: every device restored, no LED packets | "Paused" |
| `degraded` | connection dropped or refused for another reason (`reason`); re-probing | reason text + Retry (`led_reprobe`) |
| `stopped` | final line only | — |

`state` reflects the connection; `off` replaces `active` only while
connected. With mode `off` and no server the state is `server_absent`
(`mode` says `off`). Probing continues in mode `off` so the device
inventory stays available to the UI.

### Device ids

`device_id(name, vendor, serial, location)` = FNV-1a 64 over the four
strings joined by NUL, as 16 lowercase hex digits (pinned by a unit test).
Stable across processes, restarts, and controller re-ordering (OpenRGB
indexes are not stable). Persist these in settings (`led.devices`).

## `--probe`

One line ≤ 64 KiB, read-only (connect, negotiate, enumerate; no device
packets):

```json
{"version":1,"type":"led_probe","port":6742,"client_protocol":{"min":1,"max":5},
 "status":"ok","reachable":true,"server_protocol":5,"negotiated_protocol":5,
 "device_count":2,"devices":[ ...same objects as status devices[]... ]}
```

`status` ∈ `ok` (exit 0), `server_absent` (exit 3, `reachable:false`),
`error` (exit 1, with `reason` and `error` text). Same top-level fields as
the `kwe-openrgb` `probe` example, except the inventory is `devices` in the
status-line shape (with `id`) instead of the example's `controllers`, so a
daemon parses one device shape.

## Behaviour

### Connection (OpenRGB is optional)

- Probe at start, then on failure with a doubling backoff from
  `--backoff-min-ms` (1 s) capped at `--backoff-max-ms` (60 s). Refused
  connections are `server_absent`; logged only on the state *transition*,
  never per probe.
- Per-operation bounds: connect 250 ms, one request/reply 500 ms, whole
  enumeration 1.5 s. Any error on connect/enumerate, and any
  connection-losing error mid-run (timeout, reset, protocol error), drops the
  connection → `degraded` with a truthful `reason` → first re-probe after
  `backoff-min`, then the doubling backoff; once probes are refused it
  settles into quiet `server_absent`.
- The server's device-list-changed notification triggers a re-enumeration;
  per-device errors (`device_error`) are counted and logged (rate-limited)
  without dropping the connection.

### Sink

- All controllers by default; `disabled_devices` excludes by `id`. A device
  is taken into its per-LED mode (`SetCustomMode`) on first use; its previous
  mode is remembered by the client.
- One `UpdateLEDs` (whole device) per enabled device per tick at most.
  Unchanged colours are not re-sent, except a keepalive every 2 s.
- Disabling a controlled device at runtime restores its previous mode at the
  next tick; it then receives no packets at all.
- Mode `off` restores every device at the next tick; the worker stays alive
  (and connected).
- Remembered modes **survive a dropped connection**: when the server returns
  with devices still in the worker's per-LED mode, the originals are restored
  on stop, not the per-LED mode (`Client::adopt_saved_modes`).
- Known limit: after a worker *crash* (no restore ran) a respawned worker
  sees the per-LED mode as the "previous" mode. Nothing is persisted.

### Stop and restore

On SIGTERM, SIGINT, `led_stop`, or stdin EOF: the main loop notices within
50 ms (or after the in-flight OpenRGB operation, ≤ 1.5 s with a stalled
server), restores every remembered mode on a helper thread bounded to
**1.5 s** (with no live connection but remembered modes it makes one bounded
reconnect attempt first), writes the final `stopped` line (flush ≤ 300 ms),
and exits 0. Worst case ≈ 3.3 s by construction; measured SIGTERM → exit
1–51 ms with a healthy server (release build, 3 runs) and ≈ 0.55 s with a
server stalling its replies (integration test, 3 runs; the test bound is
4 s). **L3: use a ≥ 4 s SIGTERM grace before
SIGKILL** (the audio worker's 1 s is too short for the stalled-server case).

### Ambient source

- Read only while the mode is `ambient` or `ambient+pulse`, once per tick
  (never faster than `rate_hz`).
- **Open per tick, no mapping.** Each tick: `open(O_RDONLY|O_NOFOLLOW|
  O_CLOEXEC|O_NONBLOCK)`, `fstat` (regular file, ≥64 B, ≤512 MiB, size must
  equal the header's), seqlock read with `pread`, `close`. Rationale: a held
  descriptor would pin a replaced/quarantined renderer's file (up to
  512 MiB of tmpfs) after the daemon unlinks it and keep sampling a dead
  frame; a mapping would expose the worker to SIGBUS when a renderer
  truncates its file. Measured cost of the whole worker with a 1920×1080
  frame published at 30 fps: **≈0.1 % of one core at 10 Hz** (10 ms CPU in
  10 s), ≈0.3 % at 20 Hz in `ambient+pulse`, RSS ≈4.3 MB (release build,
  `--synthetic-audio` on; see the L2 handoff). An integration test asserts
  no descriptor on the frame directory is held between ticks.
- Seqlock: read the 64-byte header (validated like `kwe-frame-protocol`);
  odd generation → retry; sample; re-read the 8-byte generation; accept only
  equal even values; at most 4 attempts per tick, else `busy`. Generation 0
  (nothing published) is `busy`. An unchanged generation on the same inode
  skips the pixel reads.
- Sampling: a ≤ 64×36 grid (≤ 2304 samples) of point samples at cell
  centres, read as one `pread` per grid row (≤ 36 row reads, ≤ 36 × width × 4
  bytes). BGRA premultiplied → RGB as composited over black.
- Missing / invalid / cleared (`led_frame null`) source: the last colours
  are **held for 2 s**, then **faded to black** (time constant 1 s). A
  frame-path switch keeps the held grid, so a wallpaper switch does not
  blink. A replaced file (new inode) is picked up on the next tick.

### Modes and mapping (pure functions in `src/mapping.rs`, `src/color.rs`)

| Zone type | `ambient` | `spectrum` |
|---|---|---|
| single | chroma-weighted average of the frame (weight `1 + 4·chroma`) | bass pulse: value = bass energy, hue 270°→0° as energy rises |
| linear | LED *i* = chroma-weighted average of its share of the columns, left→right, full height | LEDs spread across the bands, hue blue (bass) → red (treble), value = band level |
| matrix | each cell = mean of its box of the grid; LEDs absent from the map take the frame colour | one bar per column, lit from the bottom, top cell partial |
| other/unknown, or matrix without a usable map | treated as linear | treated as linear |

- Audio bands: `sqrt(mean(left, right))` per band (perceptual lift); bass
  energy = mean of the lowest eighth of the bands (≥ 2).
- `ambient+pulse`: ambient colour × `0.2 + 0.8·bass`; without fresh audio
  the factor is 1 (plain ambient).
- Smoothing (exponential, per LED): ambient τ = 0.25 s; spectrum attack
  30 ms / release 200 ms; pulse attack 30 ms / release 250 ms; fade τ = 1 s.
- Brightness cap applied last (linear scale of the sRGB bytes).
- Ticks are scheduled from the actual tick start (`next = now + 1/rate`),
  so a late tick never causes a catch-up burst. Rate changes apply at once.

## Bounds summary

| Bound | Value |
|---|---|
| stdin line | 4096 B; oversized lines skipped to newline unbuffered |
| status line | 32 KiB; ≤ 4/s; heartbeat 5 s |
| devices listed / zones per device / text | 32 / 16 / 80 chars |
| disabled ids | 128 |
| rate | 1..=20 Hz; ≤ 1 UpdateLEDs per device per tick |
| frame grid | 64×36 samples, ≤ 36 row reads, 4 seqlock attempts |
| OpenRGB I/O | connect 250 ms, request 500 ms, enumerate 1.5 s; client caps in `OPENRGB_CLIENT.md` |
| probe backoff | 1 s doubling to 60 s |
| restore on stop | 1.5 s; final flush 0.3 s |
| stderr | token bucket: burst 20 lines, then 1 per 6 s, `suppressed=N` appended; lines ≤ 512 chars |

## stderr diagnostics

`event=led.<name> key=value ...`, rate-limited as above. Events:
`led.state` (state/reason transitions only), `led.frame` (frame-state
transitions), `led.update_error`, `led.stdin_rejected`, `led.restore` (only
when a restore failed or timed out), `led.fatal`.

## Tests

`cargo test -p kwe-led-worker -p kwe-led-protocol` — unit tests for the
mapping/smoothing/brightness math, the seqlock sampler (stable, unchanged,
absent, replaced, truncated, hostile header, symlink, odd generation), the
bounded stdin reader, the latest-wins writer, the log limiter, and the wire
types; integration tests (`tests/worker.rs`) run the real binary against the
`kwe-openrgb` recording stub server on ephemeral ports with synthetic frame
files: ambient on single/linear/matrix zones, spectrum from audio lines,
rate bound (UpdateLEDs counted over a measured window), disabled device gets
no packets and runtime toggle, restore on SIGTERM / stdin EOF / `led_stop` /
mode off, server absent then appearing (one log line), server dying and
returning (original modes kept), protocol error and stalled server →
`degraded`, frame missing / replaced / truncated / switched / cleared,
garbage and oversized stdin, `--probe`, and a CPU regression guard.
