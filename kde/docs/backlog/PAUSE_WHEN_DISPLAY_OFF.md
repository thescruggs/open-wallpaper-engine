# Feature: pause rendering while the displays are off (F4)

- **Requested:** 2026-09-27 (user: "stop rendering when the screen goes off
  and just display the last frame until the screen comes back on")
- **Status:** IMPLEMENTED on `f4-pause-display-off`; live display-off
  verification pending.

## Why

Renderers draw offscreen into the shared frame file, so they never notice
that nothing is being shown: a scene kept ~30% of a core and the GPU busy
all night behind a sleeping monitor.

## Design

- **Detector.** `apps/kwe-display-power-worker`: windowless Qt GUI client,
  `KScreen::Dpms::modeChanged` per `QScreen`. KWin scripting was ruled out
  (KWin 6.7 exposes no display power property or signal on the scripted
  output object) and so was `/sys/class/drm/*/dpms` (the nvidia connectors
  report `On` for disconnected outputs).
- **Relay.** The occlusion worker's `OcclusionBridge`, parameterized with
  the daemon method and subset key.
- **Daemon.** `pause_when_display_off` setting (global, default off),
  `display.power.report` RPC, a second `OcclusionDetector` instance
  (`with_label("display_power", ..)`), and
  `effective_render_pause = (covered policy) || (display-off policy)`.
- **Renderers.** Unchanged: the existing `render_pause` message.
- **UI.** Manager → Settings → "Pause while the displays are off".

## Known limits

- Pauses only when every output is asleep (one renderer serves all
  displays).
- A monitor that drops off the bus when it powers down leaves zero outputs;
  that does not pause.
- A helper started while the displays are already asleep treats them as
  awake until the next transition.

## Evidence

- Unit: settings round trip + legacy load, asleep policy, RPC round trip and
  rejection, supervisor combination of both pause reasons.
- Qt: bridge test, settings client test.
- Live (KWin 6.7.5, nvidia 615.71): helper started, `supported=1`, relayed
  `{"asleep":[],"outputs":["DP-1"]}` to a stub socket. The asleep
  transition has not been exercised on real hardware yet.
