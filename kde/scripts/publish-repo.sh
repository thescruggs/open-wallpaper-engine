#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Publish the built package into the pacman repository "kwe", which is the
# asset list of the fixed GitHub release "repo" on the fork. Clients add
#
#   [kwe]
#   SigLevel = Optional TrustAll
#   Server = https://github.com/thescruggs/open-wallpaper-engine/releases/download/repo
#
# to /etc/pacman.conf and receive new builds through `pacman -Syu`.
# The database only lists the current build; packages are not signed.
#
# Usage: scripts/publish-repo.sh [--dry-run]
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

repo="${KWE_RELEASE_REPO:-thescruggs/open-wallpaper-engine}"
repo_name="kwe"
repo_tag="repo"

dry_run=0
for arg in "$@"; do
    case "$arg" in
        --dry-run) dry_run=1 ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done

run() {
    if (( dry_run )); then
        printf 'dry-run:'; printf ' %q' "$@"; printf '\n'
    else
        "$@"
    fi
}

pkgver="$(sed -n 's/^pkgver=//p' packaging/PKGBUILD)"
pkgrel="$(sed -n 's/^pkgrel=//p' packaging/PKGBUILD)"
archive="packaging/kde-wallpaper-engine-${pkgver}-${pkgrel}-x86_64.pkg.tar.zst"
if [[ ! -f "$archive" ]]; then
    echo "FAILED: $archive is missing; build it first (cd packaging && makepkg -Ccf)" >&2
    exit 1
fi

stage="$(mktemp -d)"
trap 'rm -rf -- "$stage"' EXIT
cp -- "$archive" "$stage/"
(
    cd "$stage"
    repo-add -q "${repo_name}.db.tar.gz" "$(basename "$archive")"
    # Release assets cannot be symlinks; pacman asks for the bare names.
    cp --remove-destination "$(readlink -f "${repo_name}.db")" "${repo_name}.db"
    cp --remove-destination "$(readlink -f "${repo_name}.files")" "${repo_name}.files"
)

if ! gh release view "$repo_tag" --repo "$repo" >/dev/null 2>&1; then
    run gh release create "$repo_tag" \
        --repo "$repo" \
        --target kde-owe-backend \
        --title "pacman repository [${repo_name}]" \
        --prerelease \
        --notes "Rolling pacman repository; see kde/scripts/publish-repo.sh for the pacman.conf entry."
fi
run gh release upload "$repo_tag" --repo "$repo" --clobber \
    "$stage/${repo_name}.db" "$stage/${repo_name}.files" "$stage/$(basename "$archive")"
