# KWE Terminal — SDDM login theme

A terminal/CRT login screen that plays a random Wallpaper Engine wallpaper
from your library behind it.

## How it works

The greeter runs as the `sddm` user before any session exists. That means
`kwe-daemon` is not running, and the renderers cannot run there safely:
a renderer fault must never cost you the login screen. So the work happens
in two stages:

1. **`kwe-sddm-sync`** runs as you, inside your session, from a user timer
   (every 6 h, plus 10 min after login). It keeps a cache of loop-ready
   H.264 clips in `~/.cache/kwe/sddm-clips`:
   - **video** wallpapers are transcoded: crop-to-fill, silent, capped length;
   - **scene/web** wallpapers are recorded with `kwe bake-clip`, which runs
     the same out-of-process renderer the daemon uses (web stays
     network-isolated). It discards a warm-up and records about 20 s;
   - each clip's tail is crossfaded into its head so it loops seamlessly.

   It then copies a random subset into `/var/lib/kwe-sddm/pool` as
   `<kind>-<id>--<Title>.mp4`, with a `.jpg` poster for each.
2. **The greeter** (`kwe-terminal`) lists the pool, picks one clip at
   random on every start, and plays it muted in a loop under the terminal.
   It parses nothing except a directory listing and a file name.

Only wallpapers rated `Everyone` (from `contentrating` in `project.json`)
are used by default. Wallpapers the renderer refuses are remembered and
skipped on later runs.

## Setup

```sh
sudo kwe-sddm-sync --setup "$USER"      # creates /var/lib/kwe-sddm/pool, owned by you
kwe-sddm-sync                           # first fill (a few minutes; renders 4 clips)
systemctl --user enable --now kwe-sddm-sync.timer
```

Select the theme in System Settings → Colors & Themes → Login Screen (SDDM),
or set it in `/etc/sddm.conf.d/`:

```ini
[Theme]
Current=kwe-terminal
```

Preview it without logging out:

```sh
sddm-greeter-qt6 --test-mode --theme /usr/share/sddm/themes/kwe-terminal
```

## Tuning

`kwe-sddm-sync --help` lists the options. The useful ones:

- `--pool-size N`: how many clips go into the pool (default 8).
- `--max-bake N`: new clips rendered per run (default 4). This bounds GPU time.
- `--size WxH`: clip size. By default it uses your primary output's mode,
  scaled down to `--max-width` (default 2560).
- `--kinds scene,video,web` and `--ratings Everyone,Questionable`.

Theme options live in `theme.conf`: colors, font, dim level, scanlines,
whether to show the wallpaper title, and `poolDir`.

## Keys

| Key | Action |
| --- | --- |
| Enter | authenticate (from the login field, moves to password) |
| Tab / Shift+Tab | next / previous field |
| ↑ ↓ (login field) | cycle users |
| ← → / Space / click (session) | cycle sessions |
| F10 | focus the power buttons |

## Recovery

If the pool is empty or unreadable, the theme falls back to an animated grid.
Login keeps working either way. To go back to another theme, change
`Current=` as shown above, or from a TTY run
`sudo rm /etc/sddm.conf.d/<file>` for the file that sets it.
