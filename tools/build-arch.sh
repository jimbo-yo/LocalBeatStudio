#!/bin/sh
# Builds the Arch Linux package of LocalBeat Studio + the localbeat command
# from the committed code (git archive of HEAD):
#   tools/build-arch.sh   ->  dist/localbeat-studio-<ver>-<rel>-x86_64.pkg.tar.zst
# On Arch (or Arch-based): needs base-devel, rust (or rustup), cmake.
# Elsewhere (e.g. Fedora): runs makepkg in an archlinux container with
# podman (sudo dnf install podman). Install the result on Arch with:
#   sudo pacman -U dist/localbeat-studio-*.pkg.tar.zst
set -e
cd "$(dirname "$0")/.."
ver=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
work=$(mktemp -d)
# Files the container made belong to its user: podman unshare can remove them.
trap 'rm -rf "$work" 2>/dev/null || podman unshare rm -rf "$work"' EXIT
cp packaging/arch/PKGBUILD "$work/"
sed -i "s/^pkgver=.*/pkgver=$ver/" "$work/PKGBUILD"
git archive --format=tar.gz --prefix="localbeat-studio-$ver/" -o "$work/localbeat-studio-$ver.tar.gz" HEAD
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "note: uncommitted changes are not in the package (it's built from HEAD)" >&2
fi
mkdir -p dist
if command -v makepkg >/dev/null; then
    (cd "$work" && makepkg -sf --noconfirm)
else
    command -v podman >/dev/null || { echo "not Arch, and no podman to run an archlinux container (sudo dnf install podman)" >&2; exit 1; }
    # makepkg won't run as root: build as a throwaway user inside the container.
    podman run --rm -v "$work:/build:Z" docker.io/library/archlinux:latest bash -euc '
        pacman -Syu --noconfirm --needed base-devel rust cmake alsa-lib hicolor-icon-theme >/dev/null
        useradd -m builder && chown -R builder /build
        su builder -c "cd /build && makepkg -f --noconfirm"
    '
fi
# From the container, the package belongs to its user: copy it out through podman unshare.
cp "$work"/localbeat-studio-*.pkg.tar.zst dist/ 2>/dev/null \
    || podman unshare sh -c "cp \"$work\"/localbeat-studio-*.pkg.tar.zst dist/"
ls -1 dist/localbeat-studio-*.pkg.tar.zst
