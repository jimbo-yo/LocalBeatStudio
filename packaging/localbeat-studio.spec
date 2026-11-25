# LocalBeat Studio (the app) and localbeat (the command line) for Fedora.
# Built by tools/build-rpm.sh from a snapshot of this repo (git archive):
#   tools/build-rpm.sh   ->  ~/rpmbuild/RPMS/x86_64/localbeat-studio-*.rpm
# Installs what tools/install-linux.sh puts in ~/.local, system-wide.

# Version and release come from build-rpm.sh (workspace version; date + commit).
%{!?ver: %global ver 0.1.0}
%{!?rel: %global rel 1}

Name:           localbeat-studio
Version:        %{ver}
Release:        %{rel}%{?dist}
Summary:        LocalBeat Studio: music library, player, CD ripper and phone sync
License:        MIT
URL:            https://github.com/jimbo-yo/LocalBeatStudio
Source0:        %{name}-%{version}.tar.gz

# Rust release builds: no separate debuginfo package.
%global debug_package %{nil}

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  cmake
BuildRequires:  gcc
BuildRequires:  gcc-c++
BuildRequires:  alsa-lib-devel
BuildRequires:  desktop-file-utils

Requires:       hicolor-icon-theme
# Sending rips to the music server (rsync over SSH, the host key, the writer key).
Recommends:     rsync
Recommends:     openssh-clients
# The phone over USB or wireless debugging; URI handler registration.
Recommends:     android-tools
Recommends:     xdg-utils
# Wi-Fi to the phone (KDE Connect's mount), and the Apple SuperDrive's wake-up.
Suggests:       kde-connect
Suggests:       sg3_utils

%description
LocalBeat Studio rips CDs to verified FLAC (AccurateRip, CUETools DB repair),
tags them from MusicBrainz with cover art, plays the library (and the
LocalBeat music server's), fixes tags on older files, keeps playlists on
the music server, and syncs Opus copies to an Android phone. Includes the
localbeat command line (rip, sync, playlist, match, mcp for AI tools,
player for the phone).

%prep
%autosetup

%build
# Built like a developer build (tools/install-linux.sh): Fedora's default
# compiler flags (-flto=auto…) clash with the opus libraries' own LTO set-up
# in .cargo/config.toml.
unset CFLAGS CXXFLAGS LDFLAGS RUSTFLAGS
cargo build --release --locked -p localbeat -p localbeat-studio

%install
install -Dm755 target/release/localbeat %{buildroot}%{_bindir}/localbeat
install -Dm755 target/release/localbeat-studio %{buildroot}%{_bindir}/localbeat-studio
install -Dm644 gui/assets/localbeat.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/localbeat-studio.svg
install -Dm644 gui/assets/localbeat-256.png %{buildroot}%{_datadir}/icons/hicolor/256x256/apps/localbeat-studio.png
desktop-file-install --dir=%{buildroot}%{_datadir}/applications packaging/localbeat-studio.desktop

%check
desktop-file-validate %{buildroot}%{_datadir}/applications/localbeat-studio.desktop

%files
%license LICENSE
%{_bindir}/localbeat
%{_bindir}/localbeat-studio
%{_datadir}/applications/localbeat-studio.desktop
%{_datadir}/icons/hicolor/scalable/apps/localbeat-studio.svg
%{_datadir}/icons/hicolor/256x256/apps/localbeat-studio.png

%changelog
* Tue Nov 17 2026 Jim Baird - 0.1.0-1
- First package: LocalBeat Studio and the localbeat command.
