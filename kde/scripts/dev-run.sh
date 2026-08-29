#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
runtime_root="${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR must be set}/kwe-alpha"
socket_path="$runtime_root/daemon-v1.sock"

cd "$project_root"
cargo build --workspace
cmake -S . -B build/cmake -G Ninja -DCMAKE_BUILD_TYPE=Debug
cmake --build build/cmake --parallel
install -d -m 700 "$runtime_root"

# The scene and web workers are open-wallpaper-engine C++ binaries built by
# the surrounding monorepo's lito workspace (../kwe). Build them when lito
# is available, and hand the daemon their paths; without them the daemon
# still runs, but scene/web applies fail closed with launch_failed.
owe_root="$(cd -- "$project_root/.." && pwd)"
owe_bin_dir="${KWE_OWE_BIN_DIR:-$owe_root/build/release/bin}"
if command -v lito >/dev/null 2>&1; then
    (cd "$owe_root" && lito build --profile release \
        -p owe-kwe-scene-renderer -p owe-kwe-web-renderer)
fi
# CEF's chrome runtime resolves ICU and the .pak resources beside the
# loaded libcef.so; the minimal archive keeps Release/ and Resources/
# separate, so flatten them for dev runs (the installed package stages a
# flat bundle via lito install instead).
for cef_release in "$owe_root"/build/release/sources/archives/*/extracted/cef_binary_*/Release; do
    [[ -d "$cef_release" ]] || continue
    ln -sf "$(readlink -f "$cef_release/../Resources")"/* "$cef_release"/ 2>/dev/null || true
done
renderer_flags=()
scene_worker="$owe_bin_dir/owe-kwe-scene-renderer/kwe-scene-renderer"
web_worker="$owe_bin_dir/owe-kwe-web-renderer/kwe-web-renderer"
if [[ -x "$scene_worker" ]]; then
    renderer_flags+=(--renderer-scene "$scene_worker")
else
    echo "dev-run: OWE scene worker not built ($scene_worker); scene applies will fail" >&2
fi
if [[ -x "$web_worker" ]]; then
    renderer_flags+=(--renderer-web "$web_worker")
else
    echo "dev-run: OWE web worker not built ($web_worker); web applies will fail" >&2
fi

target/debug/kwe-daemon --socket "$socket_path" "${renderer_flags[@]}" &
daemon_pid=$!
cleanup() {
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

for _attempt in {1..50}; do
    [[ -S "$socket_path" ]] && break
    kill -0 "$daemon_pid" 2>/dev/null || {
        wait "$daemon_pid"
        exit 1
    }
    sleep 0.05
done

build/cmake/apps/kwe-manager/kwe-manager --socket "$socket_path"

