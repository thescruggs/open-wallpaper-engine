# `kwe-openrgb` — OpenRGB SDK client contract (L0+L1)

Minimal synchronous client for the OpenRGB SDK network protocol, used by the
planned `kwe-led-worker` (L2). Plan: `docs/backlog/OPENRGB_LED.md`.
Provenance: `THIRD_PARTY.yml` entry `openrgb-sdk-protocol` (independent
implementation from the published protocol description; no OpenRGB or
third-party client code copied or adapted). Std-only networking, no new
dependencies, `#![forbid(unsafe_code)]`.

## Reach

- Loopback only. `ClientConfig` has a `port` and **no address**; the client
  always dials `127.0.0.1:<port>` (default 6742). The SDK protocol has no
  authentication, so a remote address must be impossible to express.
- Must run only in a separate worker process, never in the daemon's
  request path or in `plasmashell`.

## Protocol versions

- Client range `PROTOCOL_MIN=1` ..= `PROTOCOL_MAX=5` (OpenRGB 0.5 through
  1.0rc1-format). Negotiated = `min(server, 5)`; a server reporting 0, or
  not answering the version request within `io_timeout` (the published
  rule: silence means protocol 0), is refused with `UnsupportedVersion`.
- Protocol 6 (unique controller ids, mode/LED `value` removed, per-zone
  modes) is not spoken; a v6 server is expected to answer at the
  negotiated version. Packets newer servers send unprompted (server
  name/flags, ACKs, update signals) are drained and ignored.
- Target: `openrgb 1.1-1.1` (CachyOS, build 2026-10-17) is installed on the
  development machine (the plan said 1.0rc3). Its SDK server was off, so
  the live check is **pending** — run the probe below once it is on.

## Packet subset

| Id | Name | Direction | Use |
|---|---|---|---|
| 40 | REQUEST_PROTOCOL_VERSION | req/reply | negotiation |
| 50 | SET_CLIENT_NAME | client only | `client_name` (≤64 bytes) |
| 0 | REQUEST_CONTROLLER_COUNT | req/reply | enumeration (`u32` count; extra bytes ignored) |
| 1 | REQUEST_CONTROLLER_DATA | req/reply | name, vendor, description, version, serial, location, type, modes + active mode, zones (type, LED counts, matrix map, segments v4+, flags v5+), LEDs, colours, controller flags v5+ |
| 100 | DEVICE_LIST_UPDATED | server push | marks ids stale |
| 1100 | SETCUSTOMMODE | client only | switch to per-LED mode (server picks direct/custom) |
| 1050 | UPDATELEDS | client only | whole-device colours |
| 1051 | UPDATEZONELEDS | client only | one zone's colours |
| 1101 | UPDATEMODE | client only | restore the remembered mode (echoes the server's mode block, incl. `value`) |

Wire facts assumed (not stated in the published text; **unverified until
the live probe**): little-endian integers; colour = R, G, B, pad;
`data_size` fields count themselves (the parser only requires
`data_size <= payload`, so a server that excludes the field still parses).

## Public API (what the LED worker calls)

- `Client::connect(&ClientConfig) -> Result<Client>` (negotiate + name)
- `client.refresh() -> Result<&[Controller]>`; `controllers()`,
  `controller(id)`, `needs_refresh()`, `is_connection_lost()`,
  `protocol_version()`, `server_protocol_version()`
- `client.poll_events() -> Result<PollOutcome>` — non-blocking drain; call
  each tick/idle
- `client.take_control(id)` — remember current mode once, `SetCustomMode`,
  re-read the controller
- `client.update_leds(id, &[Rgb])`, `client.update_zone_leds(id, zone, &[Rgb])`
  — count must match exactly
- `client.update_mode(id, index, &Mode)`, `client.restore(id) -> Result<bool>`,
  `client.restore_all() -> Vec<RestoreReport>` (best effort; matches devices
  by `DeviceKey {name, vendor, serial, location}`, re-enumerates first if
  the list changed)
- Types: `Controller`, `ControllerId`, `DeviceKey`, `Zone`, `zone_type`,
  `MatrixMap`, `Segment`, `Led`, `Mode`, `Rgb` (`parse_hex`, `Display`)
- Every device-targeting call first drains pending server packets, so a
  queued device-list update is seen before a stale index is used.
- Restore is explicit; `Drop` does no I/O (it would block on a dead peer).

## Bounds

| Bound | Default / cap |
|---|---|
| connect timeout | 250 ms |
| io timeout (one request/reply, one write) | 1000 ms; per-packet deadline, so a byte-dripping server still times out |
| whole `refresh` | 5 s budget |
| configurable timeouts | 1 ms ..= 30 s |
| packet payload | 4 MiB (checked from the header before reading; buffer grows in 64 KiB steps as bytes arrive) |
| controllers | 128 |
| zones / controller | 64 |
| LEDs, colours, LED display names / controller | 8192 |
| modes / controller | 128 |
| colours / mode | 256 |
| segments / zone | 64 |
| matrix cells / zone | 16384 (`height*width` must equal the payload) |
| string length | 4096 bytes (NUL incl.); invalid UTF-8 replaced |
| unrelated packets skipped per reply / drained per poll | 64 |
| remembered modes | 128 |

Every count is also checked against the bytes left in the payload before
anything is allocated.

## Error taxonomy (`kwe_openrgb::Error`)

| Variant | Meaning | Connection after |
|---|---|---|
| `ServerAbsent {port}` | connection refused — the normal state; report quietly, re-probe slowly | n/a |
| `Timeout {op}` | deadline expired (connect/read/write/enumerate) | lost |
| `Disconnected` | peer closed/reset, or used after loss | lost |
| `Io {op, kind}` | other socket error | lost |
| `BadMagic` | header without `ORGB` | lost |
| `PacketTooLarge {size, max}` | header size over cap | lost |
| `TooManyUnsolicited {max}` | reply never arrived among other packets | lost |
| `Malformed {what}` | framed packet whose contents do not parse | usable |
| `LimitExceeded {what, value, max}` | a count/length over its cap | usable |
| `UnsupportedVersion {server, min, max}` | negotiation refused | n/a (connect fails) |
| `DeviceListChanged` | ids stale; call `refresh()` | usable |
| `UnknownController(i)` / `DeviceGone` / `InvalidArgument(..)` | caller-side | usable |

`Error::is_server_absent()` and `Error::is_connection_lost()` classify.
No path panics on server input (mutation and truncation tests).

## Test helper and diagnostics

- `kwe_openrgb::stub` (feature `stub`; always on in the crate's own tests):
  `StubServer::start(StubConfig)` on an ephemeral loopback port; records
  every packet (`received()`), keeps per-controller state (active mode,
  colours), `notify_device_list_updated()`, `set_controllers()`, and
  `Fault` injection (no version reply, bad magic, oversized header,
  truncated-then-close, inner length overflow, garbage, huge controller
  count, stall, drip-feed). `sample_controller()` /
  `sample_matrix_controller()` build synthetic devices.
- `cargo run -p kwe-openrgb --example probe -- [--port N] [--timeout-ms N]
  [--set-color RRGGBB --hold-ms N]` — one JSON line (≤64 KiB): `status`
  (`ok` / `server_absent` / `error`), `reachable`, `server_protocol`,
  `negotiated_protocol`, controller inventory; with `--set-color`, paints
  every device, holds (≤10 s), restores the previous modes, reports each.
  Exit 0 ok, 3 server absent, 1 error, 2 usage.
- `cargo run -p kwe-openrgb --features stub --example stub_server -- [--port N]`
  — prints `{"port":N}`, runs until stdin EOF, then prints the received
  packet ids, client names, and final active modes (for smoke scripts).
