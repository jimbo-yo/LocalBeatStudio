#!/bin/sh
# Builds ripshelf (optimised) and installs it for this user:
#   ~/.local/bin/ripshelf, ~/.local/bin/ripshelf-gui, and an app menu entry.
# Run it again after changes to update the installed copy.
set -e
cd "$(dirname "$0")/.."
cargo build --release -p ripshelf -p ripshelf-gui
mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications"
# install replaces the files (new inodes), so a running copy isn't disturbed.
install -m 755 target/release/ripshelf target/release/ripshelf-gui "$HOME/.local/bin/"
cat > "$HOME/.local/share/applications/ripshelf.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=ripshelf
GenericName=CD ripper and phone sync
Comment=Rip CDs to verified FLAC and sync music to the phone
Exec=$HOME/.local/bin/ripshelf-gui
Icon=media-optical-audio
Terminal=false
Categories=AudioVideo;Audio;
Keywords=cd;rip;flac;opus;phone;sync;accuraterip;
DESKTOP
update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
command -v kbuildsycoca6 >/dev/null && kbuildsycoca6 >/dev/null 2>&1 || true
echo
echo "Installed: ripshelf and ripshelf-gui in $HOME/.local/bin, and 'ripshelf' in the app menu."
