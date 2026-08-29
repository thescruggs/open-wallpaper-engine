#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Contract smoke for the open-wallpaper-engine renderer workers
# (kwe-scene-renderer, kwe-web-renderer built from ../../kwe): argv contract,
# pre-publish refusal exit codes, and frame-file discipline — everything
# assertable without a GPU or a Wallpaper Engine content library. GPU-backed
# pixel smoke stays a manual lane (run a real scene and watch
# `kwe renderer-status`).
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
owe_root="$(cd -- "${project_root}/.." && pwd)"
bin_dir="${KWE_OWE_BIN_DIR:-${owe_root}/build/release/bin}"
scene_worker="${bin_dir}/owe-kwe-scene-renderer/kwe-scene-renderer"
web_worker="${bin_dir}/owe-kwe-web-renderer/kwe-web-renderer"

smoke_root="$(mktemp -d "${TMPDIR:-/tmp}/kwe-owe-worker-smoke.XXXXXX")"
trap 'rm -rf "${smoke_root}"' EXIT

failures=0
check() {
    local label="$1"
    shift
    if "$@"; then
        echo "ok: ${label}"
    else
        echo "FAIL: ${label}" >&2
        failures=$((failures + 1))
    fi
}

expect_exit() {
    local expected="$1"
    shift
    local actual=0
    "$@" >/dev/null 2>&1 || actual=$?
    [[ "${actual}" == "${expected}" ]]
}

if [[ -x "${scene_worker}" ]]; then
    check "scene: unknown flag is a usage error (2)" \
        expect_exit 2 "${scene_worker}" --output "${smoke_root}/f1.bin" \
        --content /nonexistent --no-such-flag
    check "scene: missing --output is a usage error (2)" \
        expect_exit 2 "${scene_worker}" --content /nonexistent
    check "scene: unparseable content refuses with backend_reject (73)" \
        expect_exit 73 timeout 30 "${scene_worker}" \
        --output "${smoke_root}/f2.bin" --width 64 --height 64 --fps 5 \
        --content /nonexistent/scene.pkg
    check "scene: refusal still initialized the frame mapping header" \
        test -s "${smoke_root}/f2.bin"
    check "scene: existing frame path is refused" \
        expect_exit 1 timeout 30 "${scene_worker}" \
        --output "${smoke_root}/f2.bin" --width 64 --height 64 --fps 5 \
        --content /nonexistent/scene.pkg
else
    echo "SKIPPED scene lanes: ${scene_worker} not built" >&2
fi

if [[ -x "${web_worker}" ]]; then
    check "web: unknown flag is a usage error (2)" \
        expect_exit 2 "${web_worker}" --output "${smoke_root}/w1.bin" \
        --content "${smoke_root}" --no-such-flag
    mkdir -p "${smoke_root}/empty-web"
    check "web: content without an entry html is no_drawable_content (74)" \
        expect_exit 74 timeout 30 "${web_worker}" \
        --output "${smoke_root}/w2.bin" --width 64 --height 64 --fps 5 \
        --content "${smoke_root}/empty-web"
else
    echo "SKIPPED web lanes: ${web_worker} not built" >&2
fi

if [[ "${failures}" -gt 0 ]]; then
    echo "smoke-owe-workers: ${failures} failure(s)" >&2
    exit 1
fi
echo "smoke-owe-workers: all lanes passed"
