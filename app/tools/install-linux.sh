#!/bin/sh
# Builds LocalBeat Studio (optimised) and installs it for this user:
#   ~/.local/bin/localbeat (command line), ~/.local/bin/localbeat-studio (the
#   app), and an app menu entry. Removes the pre-rename ripshelf ones.
# Run it again after changes to update the installed copy.
set -e
cd "$(dirname "$0")/.."
cargo build --release -p localbeat -p localbeat-studio
mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications"
# install replaces the files (new inodes), so a running copy isn't disturbed.
install -m 755 target/release/localbeat target/release/localbeat-studio "$HOME/.local/bin/"
cat > "$HOME/.local/share/applications/localbeat-studio.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=LocalBeat Studio
GenericName=Music library, player and CD ripper
Comment=Play your music, rip CDs to verified FLAC, sync to the phone
Exec=$HOME/.local/bin/localbeat-studio
Icon=media-optical-audio
Terminal=false
StartupWMClass=localbeat-studio
Categories=AudioVideo;Audio;Player;
Keywords=music;player;cd;rip;flac;opus;phone;sync;accuraterip;localbeat;ripshelf;
DESKTOP
# Before the rename to LocalBeat Studio (2026-11-11).
rm -f "$HOME/.local/bin/ripshelf" "$HOME/.local/bin/ripshelf-gui" "$HOME/.local/share/applications/ripshelf.desktop"
update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
command -v kbuildsycoca6 >/dev/null && kbuildsycoca6 >/dev/null 2>&1 || true
echo
echo "Installed: localbeat and localbeat-studio in $HOME/.local/bin, and 'LocalBeat Studio' in the app menu."
