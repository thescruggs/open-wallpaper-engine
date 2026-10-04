# KWE Terminal — lock screen

The `kwe-terminal` login look (see `sddm/README.md`) for the Plasma lock
screen: a terminal/CRT unlock prompt over the **live** wallpaper.

## How it works

`kscreenlocker_greet` draws the lock-screen wallpaper plugin
(`kscreenlockerrc [Greeter] WallpaperPlugin`; set it to
`org.kde.kwe.wallpaper` in System Settings → Screen Locking → Appearance)
and loads its UI from a Plasma/Shell package. `org.kde.kwe.lockscreen` is a
shell package that contains only `contents/lockscreen/`. It renders nothing
and parses no wallpaper content; it only adds the dim, vignette and
scanline layers over the wallpaper.

The greeter picks the package from `plasmashellrc [Shell] ShellPackage`,
then `$PLASMA_DEFAULT_SHELL`. Both also choose plasmashell's desktop shell, so
`kwe-lockscreen enable` sets the variable only for KWin, which starts the
greeter, through a user drop-in:

```
~/.config/systemd/user/plasma-kwin_wayland.service.d/kwe-lockscreen.conf
```

plasmashell keeps `org.kde.plasma.desktop`. If the package is missing or
fails to load, the greeter falls back to its built-in lock screen, so unlock
keeps working.

## Setup

```sh
kwe-lockscreen preview   # test mode now; unlock with your password
kwe-lockscreen enable    # from the next login
kwe-lockscreen disable   # back to the Plasma lock screen, next login
kwe-lockscreen status
```

## Behavior

- Idle: a large clock and "press any key". Typing goes straight into the
  password field.
- Awake (key, click or mouse move): boot log, password prompt, power row.
  It goes back to idle after 10 s unless something is typed.
- A wrong password shakes the terminal and shows a glitched
  `ACCESS DENIED`, then input is held for 3 s.
- Escape blanks the screen (kscreenlocker) and clears the prompt.
- Tab moves to `suspend` / `hibernate` / `switch-user`.

## Options

`kscreenlockerrc`, group `[Greeter][LnF][General]` (schema in
`contents/lockscreen/config.xml`): `accent`, `secondary`, `danger`, `font`,
`dim`, `idleDim`, `scanlines`, `hostname`, `poolDir`. The greeter does not
provide a host name: `enable` writes `hostname`; otherwise the theme
reads the `<host>.host` marker that `kwe-sddm-sync` writes to its pool.
