#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# RETIRED with the open-wallpaper-engine backend switch: this suite tested
# internals of the removed Rust scene/web renderer workers (kwe-scene-renderer,
# kwe-web-renderer, kwe-cdp). The scene and web lanes are now rendered by the
# OWE C++ workers built from ../../kwe in the open-wallpaper-engine workspace;
# their contract-level smoke lives in scripts/smoke-owe-workers.sh.
echo "RETIRED: this smoke targeted the removed Rust renderer workers." >&2
echo "See scripts/smoke-owe-workers.sh for the OWE worker contract smoke." >&2
exit 2
