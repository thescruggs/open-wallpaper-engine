#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# OpenRGB L5: LED sync end to end against the RECORDING STUB SDK server.
# A daemon built from this tree runs the real kwe-led-worker aimed at an
# ephemeral loopback port (never 6742: the maintainer's real OpenRGB) with
# private state/socket/runtime directories. The stub is driven over two
# FIFOs (`examples/led_smoke_stub.rs`), the wallpaper is a solid-colour
# stand-in renderer (`examples/led_smoke_renderer.rs`). Never selects an
# audio LED mode, and points --audio-worker at a missing path, so system
# audio is never captured. Opt-in from check.sh with KWE_RUN_LED_SMOKE=1.
#
# Cases: server absent (quiet) → server appears with no wallpaper (no
# takeover) → solid wallpaper (colour, driven_count) → rate bound → live
# settings (disable one device, then enabled:false restores all) → opt-in
# gating across a daemon restart → worker SIGKILLed mid-drive (successor
# restores the ORIGINAL modes) → daemon SIGTERM (restored, no strays).
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
target_dir="${CARGO_TARGET_DIR:-$project_root/target}"
smoke_root="$(mktemp -d -t kwe-led-smoke.XXXXXX)"
socket="$smoke_root/daemon.sock"
runtime_dir="$smoke_root/runtime"
state_dir="$smoke_root/state"
daemon_log="$smoke_root/daemon.log"
daemon_pid=""
stub_pid=""
started_at=$SECONDS
# The SDK port the stub serves (and the daemon's worker dials). Below the
# kernel's ephemeral range, so nothing else is handed it by bind(0).
sdk_port=""
readonly REAL_SDK_PORT=6742
readonly SOLID_RGB=(0 160 255)
readonly TOLERANCE=12
# Bounded wait for every condition (seconds).
readonly WAIT_S=15

stray_pids() {
    # Every process this smoke could have left behind, matched by its
    # private paths / port (never the user's own daemon or OpenRGB).
    pgrep -f -- "$smoke_root" || true
    if [[ -n "$sdk_port" ]]; then
        pgrep -f -- "kwe-led-worker .*--port $sdk_port( |\$)" || true
    fi
}

cleanup() {
    local status=$?
    if [[ -n "$daemon_pid" ]] && kill -0 "$daemon_pid" 2>/dev/null; then
        kill -TERM "$daemon_pid" 2>/dev/null || true
        for _ in {1..100}; do kill -0 "$daemon_pid" 2>/dev/null || break; sleep 0.05; done
        kill -KILL "$daemon_pid" 2>/dev/null || true
    fi
    exec 7>&- 8<&- 2>/dev/null || true
    if [[ -n "$stub_pid" ]]; then
        kill "$stub_pid" 2>/dev/null || true
        wait "$stub_pid" 2>/dev/null || true
    fi
    local leftover
    leftover="$(stray_pids)"
    if [[ -n "$leftover" ]]; then
        echo "smoke-led: killing strays: $leftover" >&2
        # shellcheck disable=SC2086
        kill -KILL $leftover 2>/dev/null || true
    fi
    if (( status != 0 )) && [[ -f "$daemon_log" ]]; then
        echo "---- daemon log (tail) ----" >&2
        tail -n 80 "$daemon_log" >&2 || true
    fi
    rm -rf -- "$smoke_root"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

fail() {
    echo "smoke-led: FAILED: $*" >&2
    exit 1
}

call_daemon() {
    local method="$1" params="{}"
    if (( $# >= 2 )); then
        params="$2"
    fi
    "$target_dir/debug/kwe" daemon-call --socket "$socket" --method "$method" --params "$params"
}

led_status() { call_daemon led.status; }

# wait_for <description> <bash condition string>
wait_for() {
    local what="$1" condition="$2" deadline=$((SECONDS + WAIT_S))
    until eval "$condition"; do
        (( SECONDS < deadline )) || fail "timed out waiting for $what"
        sleep 0.1
    done
}

# The stub is a background child on two FIFOs held open on fds 7/8 (bash
# coprocess fds are not visible inside $(...), plain fds are).
stub() {
    printf '%s\n' "$1" >&7
    local reply=""
    IFS= read -r -t 5 reply <&8 || fail "stub did not answer '$1'"
    printf '%s\n' "$reply"
}

count_packets() { # <snapshot json> <packet id>
    jq -r --arg id "$2" '.counts[$id] // 0' <<<"$1"
}

start_daemon() {
    "$target_dir/debug/kwe-daemon" \
        --socket "$socket" \
        --led-worker "$target_dir/debug/kwe-led-worker" \
        --led-sdk-port "$sdk_port" \
        --renderer "$target_dir/debug/examples/led_smoke_renderer" \
        --renderer-runtime-dir "$runtime_dir" \
        --state-dir "$state_dir" \
        --renderer-canary-ms 150 \
        --renderer-handoff-timeout-ms 1000 \
        --audio-worker "$smoke_root/no-audio-worker" \
        --no-occlusion-worker \
        --no-display-power-worker >>"$daemon_log" 2>&1 &
    daemon_pid=$!
    for _ in {1..200}; do
        [[ -S "$socket" ]] && return
        kill -0 "$daemon_pid" 2>/dev/null || fail "daemon exited during startup"
        sleep 0.02
    done
    fail "daemon socket did not appear"
}

stop_daemon() {
    kill -TERM "$daemon_pid"
    local deadline=$((SECONDS + 10))
    while kill -0 "$daemon_pid" 2>/dev/null; do
        (( SECONDS < deadline )) || fail "daemon did not stop within 10 s"
        sleep 0.05
    done
    wait "$daemon_pid" 2>/dev/null || true
    daemon_pid=""
    rm -f -- "$socket"
}

# The stand-in renderer draws a solid colour at an even fps and a fast
# colour cycle at an odd fps (unchanged colours are not re-sent, so the rate
# case needs a changing picture).
apply_wallpaper() { # solid|animated
    local id="led-$1" fps=30
    [[ "$1" == animated ]] && fps=31
    call_daemon renderer.start \
        "{\"wallpaper_id\":\"$id\",\"content_hash\":\"hash-$id\",\"width\":160,\"height\":90,\"fps\":$fps}" >/dev/null
    wait_for "renderer $id live" \
        '[[ "$(call_daemon renderer.status | jq -r "[.result.phase, .result.wallpaper_id] | join(\" \")")" == "live '"$id"'" ]]'
}

all_modes() { # <snapshot> → "2 2"
    jq -r '[.controllers[].active_mode] | map(tostring) | join(" ")' <<<"$1"
}

command -v jq >/dev/null || fail "jq is required"
cd "$project_root"
cargo build -p kwe-daemon -p kwe-cli -p kwe-led-worker --examples --bins >/dev/null 2>&1 \
    || cargo build -p kwe-daemon -p kwe-cli -p kwe-led-worker --examples --bins
for binary in kwe-daemon kwe kwe-led-worker examples/led_smoke_stub examples/led_smoke_renderer; do
    [[ -x "$target_dir/debug/$binary" ]] || fail "$target_dir/debug/$binary not built"
done

# A loopback port nothing listens on, in 20000..31999 (below ephemeral).
sdk_port="$(python3 -I - <<'PY'
import os, random, socket
rng = random.Random(os.getpid())
for _ in range(200):
    port = rng.randrange(20000, 32000)
    with socket.socket() as s:
        try:
            s.bind(("127.0.0.1", port))
        except OSError:
            continue
    print(port)
    break
PY
)"
[[ "$sdk_port" =~ ^[0-9]+$ ]] || fail "no free loopback port"
(( sdk_port != REAL_SDK_PORT )) || fail "refusing to use the real OpenRGB SDK port $REAL_SDK_PORT"
echo "smoke-led: stub SDK port $sdk_port (state $smoke_root)"

mkfifo "$smoke_root/stub.in" "$smoke_root/stub.out"
"$target_dir/debug/examples/led_smoke_stub" <"$smoke_root/stub.in" >"$smoke_root/stub.out" &
stub_pid=$!
exec 7>"$smoke_root/stub.in" 8<"$smoke_root/stub.out"
mkdir -p "$runtime_dir" "$state_dir"

# --- 1. No SDK server: quiet server_absent, no strikes, worker alive -------
start_daemon
wait_for "server_absent" '[[ "$(led_status | jq -r .result.state)" == server_absent ]]'
sleep 2 # a few bounded re-probes
status="$(led_status)"
[[ "$(jq -r .result.state <<<"$status")" == server_absent ]] || fail "not server_absent: $status"
[[ "$(jq -r .result.reason <<<"$status")" == null ]] || fail "server_absent carries a reason"
[[ "$(jq -r .result.worker.restarts <<<"$status")" == 0 ]] || fail "absence cost a restart strike"
worker_pid="$(jq -r .result.worker.pid <<<"$status")"
kill -0 "$worker_pid" 2>/dev/null || fail "LED worker $worker_pid not alive"
if grep -Eiq 'panick|wait_error|spawn_failed|forced_kill|led\.worker\.(exited|wedged)|level=error' "$daemon_log"; then
    grep -Ei 'panick|wait_error|spawn_failed|forced_kill|led\.worker\.(exited|wedged)|level=error' "$daemon_log" >&2
    fail "error-level lines while the server is absent"
fi
echo "led smoke passed: no SDK server → server_absent, success-shaped, 0 strikes, worker $worker_pid alive"

# --- 2. Server appears, no wallpaper: connects, takes over nothing ---------
[[ "$(stub "listen $sdk_port" | jq -r .listening)" == "$sdk_port" ]] || fail "stub could not listen"
call_daemon led.reprobe >/dev/null
wait_for "connected, waiting_for_source" \
    '[[ "$(led_status | jq -r "[.result.state, .result.reason] | join(\"/\")")" == active/waiting_for_source ]]'
sleep 1.5
snapshot="$(stub snapshot)"
(( $(jq -r .connections <<<"$snapshot") >= 1 )) || fail "worker never connected"
for id in 1050 1051 1100 1101; do
    [[ "$(count_packets "$snapshot" "$id")" == 0 ]] || fail "packet $id without a source: $snapshot"
done
[[ "$(all_modes "$snapshot")" == "2 2" ]] || fail "device modes changed without a source"
[[ "$(led_status | jq -r .result.worker_status.driven_count)" == 0 ]] || fail "driven_count != 0"
echo "led smoke passed: server appears with no wallpaper → waiting_for_source, no takeover/update packets"

# --- 3. Solid wallpaper: devices driven with its colour -------------------
apply_wallpaper solid
colour_ok() {
    local snap
    snap="$(stub snapshot)"
    jq -e --argjson r "${SOLID_RGB[0]}" --argjson g "${SOLID_RGB[1]}" --argjson b "${SOLID_RGB[2]}" \
        --argjson t "$TOLERANCE" \
        '[.controllers[].colors[]] | length > 0 and all(
            ((.[0] - $r) | fabs) <= $t and ((.[1] - $g) | fabs) <= $t and ((.[2] - $b) | fabs) <= $t)' \
        <<<"$snap" >/dev/null
}
wait_for "solid colour on every LED" colour_ok
status="$(led_status)"
[[ "$(jq -r .result.state <<<"$status")" == active ]] || fail "not active: $status"
[[ "$(jq -r .result.reason <<<"$status")" == null ]] || fail "active with a reason while driving"
[[ "$(jq -r .result.worker_status.driven_count <<<"$status")" == 2 ]] || fail "driven_count != 2"
[[ "$(all_modes "$(stub snapshot)")" == "0 0" ]] || fail "devices not in Direct while driven"
echo "led smoke passed: solid wallpaper → every LED ≈ rgb(${SOLID_RGB[*]}), driven_count 2"

# --- 4. Rate bound (changing picture: one UpdateLEDs per device and tick) --
apply_wallpaper animated
rate_hz=8
window_s=3
call_daemon settings.set "{\"led\":{\"rate_hz\":$rate_hz}}" >/dev/null
wait_for "rate in status" '[[ "$(led_status | jq -r .result.worker_status.rate_hz)" == '"$rate_hz"' ]]'
sleep 1
before="$(stub snapshot | jq -c .update_leds_by_device)"
sleep "$window_s"
after="$(stub snapshot | jq -c .update_leds_by_device)"
max_allowed=$((rate_hz * window_s + 2))
min_expected=$((rate_hz * window_s * 6 / 10))
for device in 0 1; do
    sent=$(( $(jq -r ".[$device]" <<<"$after") - $(jq -r ".[$device]" <<<"$before") ))
    (( sent <= max_allowed )) || fail "device $device got $sent UpdateLEDs in ${window_s}s at ${rate_hz} Hz (max $max_allowed)"
    (( sent >= min_expected )) || fail "device $device got only $sent UpdateLEDs in ${window_s}s at ${rate_hz} Hz"
    echo "led smoke: device $device: $sent UpdateLEDs in ${window_s}s at rate_hz $rate_hz (bound $max_allowed)"
done
echo "led smoke passed: update rate bounded by rate_hz"

# --- 5. Live settings: disable one device, then enabled:false -------------
strip_id="$(led_status | jq -r '.result.worker_status.devices[] | select(.name == "Stub Strip") | .id')"
[[ "$strip_id" =~ ^[0-9a-f]{16}$ ]] || fail "no device id for Stub Strip"
call_daemon settings.set "{\"led\":{\"disabled_devices\":[\"$strip_id\"]}}" >/dev/null
wait_for "Stub Strip restored" '[[ "$(stub snapshot | jq -r .controllers[1].active_mode)" == 2 ]]'
sleep 0.5
frozen="$(stub snapshot | jq -r '.update_leds_by_device[1]')"
keyboard_before="$(stub snapshot | jq -r '.update_leds_by_device[0]')"
sleep 1.5
snapshot="$(stub snapshot)"
[[ "$(jq -r '.update_leds_by_device[1]' <<<"$snapshot")" == "$frozen" ]] || fail "disabled device still updated"
(( $(jq -r '.update_leds_by_device[0]' <<<"$snapshot") > keyboard_before )) || fail "enabled device stopped updating"
[[ "$(jq -r .controllers[0].active_mode <<<"$snapshot")" == 0 ]] || fail "keyboard not driven"
[[ "$(led_status | jq -r .result.worker_status.driven_count)" == 1 ]] || fail "driven_count != 1"
echo "led smoke passed: disabling one device restores it and stops its updates; the other keeps going"

worker_pid="$(led_status | jq -r .result.worker.pid)"
call_daemon settings.set '{"led":{"enabled":false}}' >/dev/null
wait_for "worker stopped" '! kill -0 '"$worker_pid"' 2>/dev/null'
wait_for "disabled state" '[[ "$(led_status | jq -r .result.state)" == disabled ]]'
[[ "$(all_modes "$(stub snapshot)")" == "2 2" ]] || fail "not every device restored after enabled:false"
[[ "$(led_status | jq -r .result.worker.last_restore.attempted)" -ge 1 ]] || fail "no restore summary"
echo "led smoke passed: enabled:false stops the worker and restores every device"

# --- 6. Opt-in gating: led.enabled=false at start → no worker at all -------
stop_daemon
connections_before="$(stub snapshot | jq -r .connections)"
start_daemon
sleep 2
status="$(led_status)"
[[ "$(jq -r .result.state <<<"$status")" == disabled ]] || fail "not disabled after restart: $status"
[[ "$(jq -r .result.worker.running <<<"$status")" == false ]] || fail "a worker was spawned"
[[ "$(stub snapshot | jq -r .connections)" == "$connections_before" ]] || fail "the stub saw a connection"
if pgrep -f -- "kwe-led-worker .*--port $sdk_port( |\$)" >/dev/null; then
    fail "an LED worker process exists while disabled"
fi
echo "led smoke passed: led.enabled=false at daemon start → no worker spawned, no SDK connection"

# --- 7. Worker SIGKILLed mid-drive → successor restores the ORIGINAL modes -
call_daemon settings.set '{"led":{"enabled":true,"disabled_devices":[]}}' >/dev/null
apply_wallpaper solid
wait_for "both devices driven" '[[ "$(all_modes "$(stub snapshot)")" == "0 0" ]]'
[[ -f "$state_dir/led-saved-modes.json" ]] || fail "no saved-modes file while driving"
victim="$(led_status | jq -r .result.worker.pid)"
kill -KILL "$victim"
replacement_driving() {
    local status
    status="$(led_status)"
    [[ "$(jq -r .result.worker.pid <<<"$status")" =~ ^[0-9]+$ ]] \
        && [[ "$(jq -r .result.worker.pid <<<"$status")" != "$victim" ]] \
        && [[ "$(jq -r .result.worker_status.driven_count <<<"$status")" == 2 ]]
}
wait_for "replacement worker driving" replacement_driving
status="$(led_status)"
[[ "$(jq -r .result.worker.restarts <<<"$status")" == 1 ]] || fail "SIGKILL was not exactly one restart"
[[ "$(jq -r .result.worker.last_exit <<<"$status")" == signal_9 ]] || fail "last_exit not signal_9"
replacement="$(jq -r .result.worker.pid <<<"$status")"
call_daemon settings.set '{"led":{"enabled":false}}' >/dev/null
wait_for "replacement stopped" '! kill -0 '"$replacement"' 2>/dev/null'
[[ "$(all_modes "$(stub snapshot)")" == "2 2" ]] \
    || fail "successor did not restore the ORIGINAL modes: $(all_modes "$(stub snapshot)")"
[[ ! -e "$state_dir/led-saved-modes.json" ]] || fail "saved-modes file left after a full restore"
echo "led smoke passed: worker SIGKILLed mid-drive → replacement restores the original modes (Rainbow), file removed"

# --- 8. Daemon SIGTERM while driving → restored, nothing left behind -------
call_daemon settings.set '{"led":{"enabled":true}}' >/dev/null
wait_for "driving again" '[[ "$(all_modes "$(stub snapshot)")" == "0 0" ]]'
last_worker="$(led_status | jq -r .result.worker.pid)"
stop_daemon
[[ "$(all_modes "$(stub snapshot)")" == "2 2" ]] || fail "daemon stop left devices in Direct"
kill -0 "$last_worker" 2>/dev/null && fail "LED worker $last_worker survived the daemon"
leftover="$(stray_pids | grep -v "^$stub_pid\$" || true)"
[[ -z "$leftover" ]] || fail "stray processes after daemon stop: $leftover"
echo "led smoke passed: daemon SIGTERM → every device restored, no stray processes"

if grep -q 'panicked' "$daemon_log"; then
    fail "a panic in the daemon log"
fi
snapshot="$(stub snapshot)"
(( $(jq -r .total <<<"$snapshot") < 10000 )) || fail "stub recording cap reached; counts unreliable"
stub quit >/dev/null 2>&1 || true
wait "$stub_pid" 2>/dev/null || true
stub_pid=""
echo "led smoke passed: all cases in $((SECONDS - started_at)) s"
