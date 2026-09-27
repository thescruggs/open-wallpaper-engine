#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Publish the package built by kde/packaging/PKGBUILD:
#   1. tag the current commit kwe-v<pkgver>-<pkgrel> and push it to the fork,
#   2. create the GitHub release with the package archive attached,
#   3. bring the AUR -bin PKGBUILD in step (pkgver, pkgrel, checksum) and,
#      with --aur, push it to the AUR.
#
# Usage: scripts/release.sh [--aur] [--dry-run]
#
# Requires a clean tree on kde-owe-backend, the built archive in
# kde/packaging/, gh (authenticated), and for --aur an SSH key registered
# with the AUR account.
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

remote="${KWE_RELEASE_REMOTE:-fork}"
repo="${KWE_RELEASE_REPO:-thescruggs/open-wallpaper-engine}"
aur_name="kde-wallpaper-engine-bin"
aur_clone="${KWE_AUR_CLONE:-${XDG_CACHE_HOME:-$HOME/.cache}/kwe/aur/${aur_name}}"

push_aur=0
dry_run=0
for arg in "$@"; do
    case "$arg" in
        --aur) push_aur=1 ;;
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
tag="kwe-v${pkgver}-${pkgrel}"
archive="packaging/kde-wallpaper-engine-${pkgver}-${pkgrel}-x86_64.pkg.tar.zst"

if [[ ! -f "$archive" ]]; then
    echo "FAILED: $archive is missing; build it first (cd packaging && makepkg -Ccf)" >&2
    exit 1
fi
if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
    echo "FAILED: the working tree has uncommitted changes" >&2
    exit 1
fi
# The archive must come from a commit that is part of what gets tagged.
built_from="$(bsdtar -xOf "$archive" .BUILDINFO | sed -n 's/^pkgbuild_sha256sum = //p')"
if [[ "$built_from" != "$(sha256sum packaging/PKGBUILD | cut -d' ' -f1)" ]]; then
    echo "FAILED: $archive was not built from the current packaging/PKGBUILD" >&2
    exit 1
fi

sha256="$(sha256sum "$archive" | cut -d' ' -f1)"
echo "release $tag  sha256 $sha256"

# AUR recipe first, so the tagged commit carries the matching checksum.
aur_pkgbuild="packaging/aur/${aur_name}/PKGBUILD"
run sed -i \
    -e "s/^pkgver=.*/pkgver=${pkgver}/" \
    -e "s/^pkgrel=.*/pkgrel=${pkgrel}/" \
    -e "s/^sha256sums=.*/sha256sums=('${sha256}')/" \
    "$aur_pkgbuild"
if ! git diff --quiet -- "$aur_pkgbuild"; then
    run git commit -m "AUR: ${aur_name} ${pkgver}-${pkgrel}" -- "$aur_pkgbuild"
fi

run git tag -a "$tag" -m "KDE Wallpaper Engine ${pkgver}-${pkgrel}"
run git push "$remote" HEAD:kde-owe-backend "$tag"
run gh release create "$tag" "$archive" \
    --repo "$repo" \
    --title "KDE Wallpaper Engine ${pkgver}-${pkgrel}" \
    --prerelease \
    --notes "Install: \`sudo pacman -U $(basename "$archive")\`, then \`systemctl --user restart kwe-daemon.service\`.

sha256: \`${sha256}\`"

# Rolling pacman repository on the fork (works without an AUR account).
if (( dry_run )); then
    scripts/publish-repo.sh --dry-run
else
    scripts/publish-repo.sh
fi

if (( push_aur )); then
    if [[ ! -d "$aur_clone/.git" ]]; then
        run git clone "ssh://aur@aur.archlinux.org/${aur_name}.git" "$aur_clone"
    fi
    if (( ! dry_run )); then
        cp "$aur_pkgbuild" packaging/kde-wallpaper-engine.install "$aur_clone/"
        (cd "$aur_clone" && makepkg --printsrcinfo > .SRCINFO)
    fi
    run git -C "$aur_clone" add PKGBUILD .SRCINFO kde-wallpaper-engine.install
    run git -C "$aur_clone" commit -m "${pkgver}-${pkgrel}"
    run git -C "$aur_clone" push origin HEAD:master
fi
