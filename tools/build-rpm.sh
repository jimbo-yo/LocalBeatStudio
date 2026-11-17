#!/bin/sh
# Builds the Fedora RPM of LocalBeat Studio + the localbeat command from the
# committed code (git archive of HEAD; uncommitted changes are not in it):
#   tools/build-rpm.sh   ->  ~/rpmbuild/RPMS/x86_64/localbeat-studio-<ver>-<rel>.fc<N>.x86_64.rpm
# Needs: sudo dnf install rpm-build rpmdevtools desktop-file-utils, plus the
# build packages (rust cargo cmake gcc gcc-c++ alsa-lib-devel). A release
# build: ~10-13 min the first time. Install with: sudo dnf install <the rpm>
set -e
cd "$(dirname "$0")/.."
ver=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
rel="1.$(git log -1 --format=%cd --date=format:%Y%m%d).git$(git rev-parse --short HEAD)"
top=$(rpm --eval %_topdir)
mkdir -p "$top/SOURCES" "$top/SPECS"
git archive --format=tar.gz --prefix="localbeat-studio-$ver/" -o "$top/SOURCES/localbeat-studio-$ver.tar.gz" HEAD
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "note: uncommitted changes are not in the package (it's built from HEAD)" >&2
fi
rpmbuild -bb --define "ver $ver" --define "rel $rel" packaging/localbeat-studio.spec
echo
ls -1 "$top"/RPMS/*/localbeat-studio-"$ver-$rel".*.rpm
