#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

# The half of BETA B1 that no runtime test covers: the unit must stay part of
# the graphical session, or the daemon goes back to starting before any
# display exists (docs/bugs/OUTPUTS_EMPTY_AFTER_REBOOT.md).
unit_file="packaging/systemd/kwe-daemon.service"
for directive in "PartOf=graphical-session.target" \
                 "After=graphical-session.target" \
                 "WantedBy=graphical-session.target"; do
    if ! grep -qx -- "$directive" "$unit_file"; then
        echo "FAILED: $unit_file is missing '$directive'" >&2
        echo "        (BETA B1: the daemon must start with the graphical session)" >&2
        exit 1
    fi
done
if grep -qx -- "WantedBy=default.target" "$unit_file"; then
    echo "FAILED: $unit_file installs into default.target again (BETA B1)" >&2
    exit 1
fi

# MUST-FIX 4 (OpenRGB review fixes B): LED sync defaults on, so a script
# that starts kwe-daemon without --no-led-worker would drive the
# maintainer's REAL devices through OpenRGB's SDK server (127.0.0.1:6742).
# Every daemon launch under scripts/ must pass --no-led-worker or point the
# worker at a stub with --led-sdk-port. A launch is a (backslash-joined)
# command line that executes target/*/kwe-daemon, "$daemon_bin" or
# "${KWE_DAEMON_BIN...}"; comments, assignments and systemctl/pgrep lines
# are not launches. A deliberate real-device launch (dev-run.sh's
# KWE_DEV_LED=1) must say so with a trailing `# led-guard: <why>` comment.
led_guard_failures="$(
    for script in scripts/*.sh; do
        awk -v file="$script" '
            { line = $0 }
            /\\[[:space:]]*$/ { sub(/\\[[:space:]]*$/, "", line); joined = joined line " "; if (!start) start = FNR; next }
            {
                joined = joined line
                if (!start) start = FNR
                text = joined
                sub(/^[[:space:]]+/, "", text)
                launches = text !~ /^#/ \
                    && text !~ /^[A-Za-z_][A-Za-z_0-9]*=["\x27]?[^[:space:]]*kwe-daemon["\x27]?[[:space:]]*$/ \
                    && text !~ /(systemctl|pgrep|pkill|journalctl|basename|readlink)/ \
                    && (text ~ /(^|[[:space:]"\x27\/])(target|debug)\/[^[:space:]]*kwe-daemon["\x27]?([[:space:]]|$)/ \
                        || text ~ /^"?\$\{?daemon_bin\}?"?[[:space:]]/ \
                        || text ~ /\$\{KWE_DAEMON_BIN[^}]*\}"?[[:space:]]/)
                if (launches && text !~ /--no-led-worker/ && text !~ /--led-sdk-port/ \
                    && text !~ /# led-guard: [^[:space:]]/)
                    printf "%s:%d: kwe-daemon launched without --no-led-worker or --led-sdk-port\n", file, start
                joined = ""; start = 0
            }
        ' "$script"
    done
)"
if [[ -n "$led_guard_failures" ]]; then
    echo "FAILED: LED guard (a script would drive real OpenRGB devices):" >&2
    printf '%s\n' "$led_guard_failures" >&2
    exit 1
fi
echo "LED guard: every kwe-daemon launch under scripts/ passes --no-led-worker or a stub --led-sdk-port"

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
# R5: the daemon's real-worker LED tests must run against a FRESH worker and
# must not skip silently: build it first, then make a missing binary fail.
cargo build -p kwe-led-worker
KWE_REQUIRE_LED_WORKER=1 cargo test --workspace --all-targets
cargo build --workspace
cmake -S . -B build/cmake -G Ninja -DCMAKE_BUILD_TYPE=Debug
cmake --build build/cmake --parallel
# Resolves the Qt 6 qmllint itself (a bare `qmllint` is Qt 5's on this distro
# and passes everything), asserts the QML module really registers its C++
# types, and fails on any unresolved type.
scripts/qml-typecheck.sh
target/debug/kwe diagnose
target/debug/kwe-vulkan --json

if [[ "${KWE_RUN_UI_SMOKE:-0}" == "1" ]]; then
    scripts/smoke-ui.sh
fi
if [[ "${KWE_RUN_FRAME_SMOKE:-0}" == "1" ]]; then
    scripts/smoke-frame-transport.sh
fi
if [[ "${KWE_RUN_SUPERVISOR_SMOKE:-0}" == "1" ]]; then
    scripts/smoke-supervisor.sh
fi
if [[ "${KWE_RUN_LED_SMOKE:-0}" == "1" ]]; then
    scripts/smoke-led.sh
fi
if [[ "${KWE_RUN_INPUT_PREVIEW_SMOKE:-0}" == "1" ]]; then
    scripts/smoke-input-preview.sh
fi
if [[ "${KWE_RUN_PLASMA_DISPLAY_SMOKE:-0}" == "1" ]]; then
    scripts/smoke-plasma-display.sh
fi
if [[ "${KWE_RUN_PLAYLIST_SMOKE:-0}" == "1" ]]; then
    scripts/smoke-playlist-restart.sh
fi
if [[ "${KWE_RUN_WORKSHOP_CACHE_SMOKE:-0}" == "1" ]]; then
    scripts/smoke-workshop-cache.sh
fi
if [[ "${KWE_RUN_SCENE_CORPUS_SMOKE:-0}" == "1" ]]; then
    scripts/smoke-scene-corpus.sh
fi
# Destructive live-session smoke (BETA_M4d): authorized on this machine only.
if [[ "${KWE_RUN_LIVE_APPLY_SMOKE:-0}" == "1" ]]; then
    scripts/smoke-live-apply.sh
fi
