# The open-wallpaper-engine renderer backend

As of 2026-08-28 this tree is the KDE front end of the
[open-wallpaper-engine](https://github.com/waywallen/open-wallpaper-engine)
monorepo (it lives in `kde/` of that repository). The daemon, manager, CLI,
Plasma package, protocols, and supervision model are unchanged; the scene
and web renderer *workers* are now C++ binaries built from the surrounding
OWE workspace instead of the retired Rust implementations.

## What changed

| Lane | Before | Now |
|---|---|---|
| scene | `crates/kwe-scene-renderer` (Rust, partial WE support) | `../kwe/SceneRenderer` — `owe::SceneWallpaper`, the full OWE Vulkan engine |
| web | `crates/kwe-web-renderer` (bwrap + headless Chromium over CDP) | `../kwe/WebRenderer` — CEF `weweb::BrowserHost`, in-process OSR |
| video | `crates/kwe-video-renderer` (libmpv) | unchanged |
| test | `crates/kwe-test-renderer` | unchanged |

Also removed with them: `kwe-cdp` (only the old web worker used it) and
`kwe-shader-compiler` (only the old scene renderer used it; the daemon still
accepts `--shader-helper` and the workers accept-and-ignore the flag).

## Worker contract (unchanged)

Both new workers speak the exact supervisor contract
(`docs/SUPERVISOR_API_V1.md`):

- argv: `--output <frame> --width W --height H --fps N --scaling
  aspect|fill|stretch --content <path>` plus the kind-specific flags
  (`--assets-dir`/`--shader-helper` for scene; `--web-heartbeat-ms`,
  `--web-heartbeat-max-failures`, `--allow-network`, `--probe` for web) and
  the development fault flags (`--startup-hang`, `--hang-after`,
  `--corrupt-after`, `--exit-after`, `--ignore-term`,
  `--memory-pressure-after`/`--memory-pressure-mib`, `--stderr-lines`).
- frames: shared frame protocol v1 (`docs/FRAME_PROTOCOL_V1.md`), BGRA8888
  premultiplied, two-slot seqlock file, re-published every tick so the
  supervisor's generation watchdog sees liveness on static content.
- input: normalized input protocol v1 on stdin, pointer acks on stdout.
- exit codes: 0 graceful, 70 exit-after fault, 71/72 memory pressure, 73
  backend rejection, 74 no drawable content, 2 usage error.

The C++ side of the frame/input protocols lives in `../kwe/Common`
(`kwe.frame_protocol`, `kwe.input_wire`, `kwe.worker_common`), wire-
compatible with the Rust `kwe-frame-protocol` / `kwe-input-protocol` crates
that the daemon and display client keep using.

## Scene worker

`kwe-scene-renderer` (from `owe-kwe-scene-renderer`):

- Parses the scene up front (`LoadSceneDocumentFromSource`); an unreadable
  or unparseable scene exits 73 before first publish, which the supervisor
  reports as a refusal, never a crash loop.
- Renders fully offscreen: `RenderInitInfo{offscreen, LINEAR tiling,
  offscreen_host_visible}`; the engine's `LocalExSwapchain` exports its
  three slots as DMA-BUFs, the worker mmaps them, waits on the frame sync
  fd (bounded, adaptive), and converts RGBA→BGRA into the frame file.
  `offscreen_host_visible` was plumbed through the OWE engine for this:
  host-visible slots make the CPU readback run at memory speed instead of
  uncached-VRAM speed (measured 1080p30 at full rate on an RTX 3070).
- `--scaling` maps to the engine `FillMode` (aspect→ASPECTFIT,
  fill→ASPECTCROP, stretch→STRETCH), so the GPU does the aspect mapping.
- Pointer wire events feed `mouseInput`/`mouseEnter`/`mouseButton`;
  `media_state` maps onto the scenescript MPRIS constants. `audio_bands`
  is ignored: the engine consumes PCM windows, not analyzed bands, and
  captures its own audio through PipeWire when the session permits.
- Daemon-side changes for this lane: scene workers now inherit
  `XDG_RUNTIME_DIR` (PipeWire) and get a persistent `XDG_CACHE_HOME`
  (`<state>/scene-shader-cache`) so shader compiles survive the per-launch
  throwaway HOME; the scene startup timeout default rose to 25 s for the
  cold-compile case.

## Web worker

`kwe-web-renderer` (from `owe-kwe-web-renderer`):

- Embeds CEF via `weweb::BrowserHost` in software-OSR mode; `OnPaint`
  delivers BGRA frames that are published directly.
- Network isolation without bwrap: absent `--allow-network` the worker
  calls `unshare(CLONE_NEWUSER|CLONE_NEWNET)` before CEF initialises, so
  every CEF subprocess inherits an empty network namespace. Failure to
  isolate exits 73 (fail closed).
- Audio output stays disabled (CEF muted); the daemon's grant-gated
  `audio_bands` stream feeds the page's `wallpaperRegisterAudioListener`
  API through `PushAudioData` (64+64 bands).
- Liveness: the worker kicks `Invalidate()` every tick, making paint
  recency a valid heartbeat; a paint gap over
  `heartbeat-ms × max-failures` exits 73.
- `--probe` boots the real browser against a throwaway animated page and
  prints `{"backend":"cef-owe","browser_version":...}` for `kwe diagnose`.
- Deployment: the CEF runtime must sit beside the real binary (the chrome
  runtime resolves ICU and .pak files relative to `libcef.so`). The
  package stages a flat bundle in `/usr/lib/kde-wallpaper-engine/weweb/`
  with a `/usr/bin/kwe-web-renderer` symlink; `scripts/dev-run.sh`
  flattens the extracted CEF archive for dev-tree runs.

## Render pause (F3)

Both workers honour the `render_pause` input line through the shared
`kwe/Common/InputWire.cppm` dispatcher: the scene worker calls the engine's
`pause()`/`play()` and stops copying frames, the web worker hides the CEF
browser (`SetPaused` → `WasHidden`), stops its invalidate kicks and
suspends the paint heartbeat. Both keep re-publishing the last frame every
500 ms while paused so kwe-daemon's frame watchdog needs no special case.
The detector side lives in `kde/apps/kwe-occlusion-worker` (see
`kde/docs/SUPERVISOR_API_V1.md`, "Pause when covered").

## Capability gate

`kwe-core`'s `SCENE_CAPABILITIES_IMPLEMENTED` now covers every id the
inspector can emit (sound layers and lighting moved from tolerated to
implemented), and `SCENE_CAPABILITIES_LIMITATION_TOLERATED` is empty. The
SR-1c classification stayed parameterized (`classify_required_capabilities`
in `kwe-daemon/src/apply.rs`) so the tolerated machinery remains
unit-tested with synthetic ids.

## Building and testing

```sh
# workers (repo root; lito: https://github.com/litocpp/lito)
lito build --profile release -p owe-kwe-scene-renderer -p owe-kwe-web-renderer

# KDE stack
cd kde && cargo build --workspace

# contract smoke (no GPU or content library needed)
scripts/smoke-owe-workers.sh

# everything wired together
scripts/dev-run.sh
```

`scripts/smoke-scene.sh`, `smoke-web-compromise.sh`, `smoke-cdp.sh`, and
`scene-corpus-byte-identity-sweep.sh` are retired stubs — they tested
internals of the removed Rust workers.

## Verified on 2026-08-28 (RTX 3070, CachyOS)

- Scene: real Workshop `scene.pkg` renders at 1080p30 full rate; daemon
  `renderer.start` → canary → `live`; pointer input acked end to end;
  graceful stop.
- Web: animated page publishes at target fps inside the network namespace;
  daemon promotion to `live`; probe reports the CEF version.
- `cargo test --workspace --exclude kwe-vulkan`: green.
