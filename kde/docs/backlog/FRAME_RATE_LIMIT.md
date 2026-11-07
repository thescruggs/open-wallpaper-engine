# Feature: per-wallpaper frame-rate limit (F2)

- **Requested:** 2026-08-22 (user: "add an option to limit frame rate")
- **Status:** IMPLEMENTED 2026-08-22 (manager option; the daemon side already
  existed).

## What exists

`wallpaper.apply` has carried `fps` (1..=240, default 30) since M4a; the
renderer paces its publishes to it (`--fps`: libmpv frame delivery, the web
screencast pacing, the scene render loop) and the assignment persists it.

## Added

- Manager: a "Frame rate limit" selector (15 / 24 / 30 default / 60) beside
  the scaling selector; `applyWallpaper(..., scaling, fps)`; only a
  non-default value travels on the wire; the retry replays it; out-of-range
  values never reach the daemon (`frameRateLimitTravelsOnlyWhenNotDefault`).
- Playlist lane: a playlist advance keeps the output's stored `fps` (like
  `scaling`) instead of resetting to 30.
  - **Superseded 2026-11-07:** the daemon now owns per-kind defaults
    (`settings.renderer_defaults`, edited on the manager's Settings page);
    a playlist advance and any `wallpaper.apply` that omits `fps`/`scaling`
    use them. The one-off values of a manual Apply no longer pin later
    playlist entries (a 60 fps Apply used to make every later entry 60 fps
    while the Settings page said otherwise). The manager's old QSettings
    values migrate into the daemon record once.

## Not done / notes

- Lower limits reduce CPU/GPU proportionally (JPEG decode, libmpv software
  render, Vulkan composite all scale with the publish rate). The renderer's
  own minimum work (decode at the media's native rate for video) is not
  reduced by the publish limit.
- A "pause when occluded" mode is F3 (separate design).
