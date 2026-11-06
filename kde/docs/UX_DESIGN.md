# User experience design

## Product standard

The application should feel native to Plasma rather than like a Windows or web
application transplanted onto KDE. Follow the KDE Human Interface Guidelines,
Kirigami conventions, Breeze styling, the active color scheme, the system font,
the user's icon theme, localization, accessibility settings, and reduced-motion
preference.

KDE's HIG summarizes the desired balance as simple by default and powerful when
needed: <https://develop.kde.org/hig/>. Kirigami supplies responsive pages,
cards, actions, form layouts, and settings components:
<https://develop.kde.org/docs/getting-started/kirigami/>.

Do not visually clone Wallpaper Engine. Preserve familiar concepts and
workflows while expressing them with native KDE controls and terminology.

## Information architecture

Primary destinations:

1. **Library** — installed Workshop and local wallpapers.
2. **Workshop** — browse, filter, subscribe, and monitor downloads.
3. **Playlists** — create, reorder, schedule, and assign collections.
4. **Displays** — per-output wallpaper, span/clone/group, and saved profiles.
5. **Activity** — downloads, renderer events, compatibility tests, and recent
   automatic recoveries.
6. **Settings** — playback, quality, audio/input permissions, storage,
   backends, shortcuts, and diagnostics.

Diagnostics should be easy to reach but should not dominate healthy everyday
use. Safe mode must be available from both the UI and CLI.

## Core workflows

### First run

1. Welcome and explain ownership/content boundaries in one short page.
2. Auto-detect Steam and all libraries; let the user correct paths.
3. Run GPU, renderer, PipeWire, and Plasma integration checks.
4. Install/enable the thin Plasma package only with explicit confirmation.
5. Render a synthetic canary, verify fallback recovery, then show the Library.

Each check shows `Ready`, `Limited`, or `Action needed`, with a short reason and
one primary action. Advanced logs stay collapsed.

### Library and Workshop

- Responsive thumbnail grid on wide windows; compact list alternative.
- Persistent search with chips for type, installed/subscribed, compatibility,
  audio, interaction, resolution/aspect, tags, and backend.
- Stable sorting and selection when metadata or downloads update.
- Cards show title, preview, type, installation state, and a text/icon
  compatibility badge. Hover is supplementary; every action works by keyboard.
- Installed and Workshop items use the same details page and property editor.
- Subscription and downloads expose progress, pause/cancel when supported, and
  an offline state without blocking access to installed content.

### Wallpaper details

Use a responsive master/detail layout:

- large preview with play/pause, mute, interaction test, and safe canary action;
- title, author, Workshop link, type, tags, content/update state;
- primary **Apply** action with display/profile target next to it;
- generated native editors for user properties, with Reset, Save preset, and
  unsaved-change indication;
- clear compatibility summary followed by expandable feature-level details;
- quarantine/recovery history with **Test again** and **Export report** actions.

Applying an untested or partially compatible wallpaper invokes a canary, not a
frightening generic confirmation dialog. Only permissions or known material
risk require confirmation.

### Playlists and displays

- Playlist editing supports drag-and-drop plus keyboard Move up/down actions.
- Show total duration, transition, shuffle/repeat behavior, and unavailable or
  quarantined entries before save.
- Display overview mirrors the physical layout reported by Plasma and identifies
  outputs by stable connector/EDID-derived identity, friendly name, scale, and
  orientation.
- Saved profiles are snapshots of per-display wallpaper/playlist assignments.
- Hotplug does not silently overwrite a saved profile; show the proposed mapping.

### Recovery

When a wallpaper fails, keep the desktop usable and show a passive notification:

`“Seascape was stopped after the renderer failed. The previous wallpaper was restored.”`

Offer **Details** and **Test again**; never show raw signals or stack traces as
the primary message. Repeated failures add a `Quarantined` badge and explain
what will permit another automatic test (content, renderer, driver, or settings
change).

### LED sync (Settings, OpenRGB slice L4)

OpenRGB is optional (maintainer, 2026-11-05): LED sync is on by default and
used whenever OpenRGB's SDK server answers on loopback. Without it the
section is calm information, never an error. The section has an enable
switch, a mode picker (Ambient / Spectrum / Ambient + pulse, each with a
one-line description; the switch covers "off", so Off is not a picker entry
— a daemon whose stored mode is `off` shows "Off" until another mode is
chosen), a brightness slider (0–100 %, steps of 5), an update-rate spin box
(1–20 Hz) and one checkbox per detected device (name, vendor, LED count).
`led.status` is polled every 3 s only while Settings is visible and its
window is not minimised (review fixes C2).

Every state has an icon and text:

| Daemon state | Icon | Text | Action |
|---|---|---|---|
| `disabled` | stop | LED sync is off — turn it on to light up OpenRGB devices | — |
| `worker_unavailable` / `not_installed` | warning | The LED sync helper is missing; reinstalling restores it | Check again |
| `worker_unavailable` / `not_executable` | warning | The LED sync helper cannot be started (not executable); reinstalling restores it | Check again |
| `worker_unavailable` / `restart_budget_exhausted` | warning | The helper kept stopping and was paused | Check again |
| `worker_unavailable` / `not_configured` | information | Not available in this session (`--no-led-worker`) | — |
| `starting`, `connecting` | spinner | Starting LED sync… / Restarting… / Restoring your lighting… / Looking for OpenRGB… | — |
| `server_absent` (normal) | information | OpenRGB's SDK server is not running; LED sync is optional; OpenRGB → SDK Server → Start Server, or `openrgb --server`; connects by itself | Check again |
| `active` / `waiting_for_source` | information | Connected to OpenRGB — waiting for a wallpaper (ambient modes) / waiting for audio (spectrum; also shown while nothing is playing — silent audio is not a source); devices keep their own lighting until then; M found | — |
| `active` | success | Syncing N devices with OpenRGB (N = worker `driven_count`; estimated from the list for an older worker); M found; SDK protocol version | — |
| `off` / `paused` | pause | Paused while a fullscreen app covers the desktop or the displays are off | — |
| `off` / none | stop | Mode is Off; devices keep their own lighting (also while a pause verdict holds: the row follows `reason`, not the raw `paused` flag — review fixes C2) | — |
| `degraded` | warning | LED sync is having trouble: reason in plain words; keeps retrying | Check again |

Giving lighting back (review fixes C1): devices return to the lighting they
had before LED sync took them over. That needs lighting OpenRGB knows about —
a non-Direct mode (Static, Rainbow, …) or a loaded OpenRGB profile. A device
that was only showing its firmware effect while OpenRGB reported Direct with
no colours (OpenRGB just started, no profile) cannot be handed back to the
firmware over OpenRGB's SDK: it keeps the last wallpaper colours until the
user picks lighting in OpenRGB. Devices are not taken over again sooner than
5 s after being given back, and a source must be present for 1 s first, so a
flickering source or fullscreen toggling cannot make them blink.

"Check again" calls `led.reprobe`; while the daemon's rate limit runs (and
for 2 s after an accepted call) the button is disabled, never an error; while
the helper restarts it is disabled for the daemon's `retry_after_ms` (1 s
when an older daemon sends none).
Audio hint (review fixes C2): system audio capture starts by itself for the
audio modes while LED sync is connected to OpenRGB and not paused (not for an
audio mode with no SDK server, and it stops 10 s after a pause or a
disconnect). While connected (`active`) in `spectrum` or `ambient+pulse`, an
information message explains missing sound in two ways, from the worker's
`audio` block: fresh but silent frames → "Nothing is playing right now"
(Spectrum lights up as soon as sound plays / Ambient + pulse shows steady
wallpaper colours); no fresh frames → "No audio is reaching LED sync right
now" (capture may still be starting, or could not capture the audio output;
until audio arrives Spectrum stays dark and Ambient + pulse shows steady
wallpaper colours). No wallpaper is needed. Not shown in other states.
An older daemon without `led.*` shows "LED sync needs
a newer wallpaper service"; an unreachable daemon shows that the state is
unknown and is re-checked while the page is open. A rejected write shows a
warning with the daemon's reason and the controls return to the daemon's
values.

## Compatibility language

Use consistent states everywhere:

- **Compatible** — exercised automatically on this backend/hardware.
- **Expected to work** — required features are implemented but this exact item
  has not completed a canary here.
- **Partial** — usable with named missing or altered behavior.
- **Incompatible** — a required capability is unavailable.
- **Quarantined** — this content hash repeatedly failed on this system.
- **Unknown** — insufficient metadata or no matching capability evidence.

These states require an icon and text, never color alone. A details popover
shows backend, last test, hardware context, and missing capability IDs.

## Native KDE integration

- Use `KConfig`-compatible settings exposure where practical and KDE standard
  shortcuts/actions.
- Integrate notifications through the desktop notification service and open the
  relevant Activity/details page from the notification.
- Expose playback and active wallpaper state over D-Bus for KRunner, scripts,
  and future Plasma widgets.
- Use portals or KDE file dialogs for user-selected files and directories.
- Respect power profiles, session lock, idle state, color scheme, locale,
  fractional scale, output rotation, and reduced motion.
- Never hide or replace Plasma desktop icons; the wallpaper stays beneath the
  containment.

## Accessibility and quality gates

- Full operation with keyboard only; predictable focus order and visible focus.
- Accessible names/descriptions for icon-only actions, previews, badges, and
  progress indicators.
- Screen-reader announcements for download completion, apply/rollback, and
  quarantine without announcing every frame or progress tick.
- No information encoded only by color, animation, hover, or sound.
- Test Breeze Light/Dark, high contrast, custom accent colors, 100/125/150/200/
  250% scale, long translations, RTL layout, touch, and reduced motion.
- Avoid modal-dialog chains. Preserve user selection and scroll position after
  updates, errors, navigation, and window resize.
- UI remains responsive while scanning the 92-item local baseline and a
  10,000-item synthetic catalog; image decode and metadata work never block the
  GUI thread.
- Visual regression tests cover the six primary destinations and every standard
  state: loading, empty, offline, degraded, permission-required, and failure.

