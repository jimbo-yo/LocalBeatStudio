<#
.SYNOPSIS
  Builds LocalBeat Studio and installs it for this Windows user.

.DESCRIPTION
  The Windows counterpart of tools/install-linux.sh. Run from anywhere:

      pwsh -File tools\install-windows.ps1

  1. Checks Rust (MSVC) and installs CMake if it's missing (winget).
  2. Installs the programs Studio uses, unless -NoTools:
     - rsync (sending albums to the server): MSYS2's rsync + ssh, in
       C:\msys64. Windows' own OpenSSH can't be used with rsync, and the
       server's key only lets rsync in.
     - adb (phone sync over USB or Wi-Fi): Google.PlatformTools.
  3. Release build (~10-13 min the first time).
  4. Copies localbeat-studio.exe and localbeat.exe to
     %LOCALAPPDATA%\Programs\LocalBeat Studio, adds that folder to your PATH
     (for the localbeat command) and makes a Start menu entry.

  A running Studio is fine: its exe is renamed aside, not overwritten.
#>
param(
    # Don't install rsync (MSYS2) or adb.
    [switch]$NoTools
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $env:LOCALAPPDATA 'Programs\LocalBeat Studio'

function Have($name) { [bool](Get-Command $name -ErrorAction SilentlyContinue) }

function Install-Winget($id) {
    Write-Host "Installing $id (winget)..."
    winget install --id $id -e --silent --accept-source-agreements --accept-package-agreements
    if ($LASTEXITCODE -ne 0) { throw "winget couldn't install $id (exit $LASTEXITCODE)" }
}

# --- 1. Build tools -----------------------------------------------------------
if (-not (Have cargo)) {
    throw "Rust isn't installed: winget install Rustlang.Rustup, plus Visual Studio Build Tools with 'Desktop development with C++', then open a new terminal"
}
$cmakeDir = 'C:\Program Files\CMake\bin'
if (-not (Have cmake)) {
    if (-not (Test-Path "$cmakeDir\cmake.exe")) { Install-Winget Kitware.CMake }
    $env:Path = "$cmakeDir;$env:Path"
}

# --- 2. Programs Studio runs -------------------------------------------------
if (-not $NoTools) {
    $msysBin = 'C:\msys64\usr\bin'
    if (-not (Test-Path "$msysBin\rsync.exe") -or -not (Test-Path "$msysBin\ssh.exe")) {
        if (-not (Test-Path "$msysBin\bash.exe")) { Install-Winget MSYS2.MSYS2 }
        Write-Host 'Installing rsync and ssh inside MSYS2 (pacman)...'
        # Twice: the first update can replace pacman/the runtime and stop early.
        & "$msysBin\bash.exe" -lc 'pacman -Syu --noconfirm' | Out-Host
        & "$msysBin\bash.exe" -lc 'pacman -Syu --noconfirm' | Out-Host
        & "$msysBin\bash.exe" -lc 'pacman -S --needed --noconfirm rsync openssh' | Out-Host
        if (-not (Test-Path "$msysBin\rsync.exe")) { throw "rsync didn't install in MSYS2" }
    }
    Write-Host "rsync: $msysBin\rsync.exe"

    $sdkAdb = Join-Path $env:LOCALAPPDATA 'Android\Sdk\platform-tools\adb.exe'
    if (-not (Have adb) -and -not (Test-Path $sdkAdb)) { Install-Winget Google.PlatformTools }
    Write-Host 'adb: installed'
}

# --- 3. Build ----------------------------------------------------------------
Write-Host 'Building (release)...'
Push-Location $repo
try {
    cargo build --release -p localbeat -p localbeat-studio
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
} finally { Pop-Location }

# --- 4. Install --------------------------------------------------------------
New-Item -ItemType Directory -Force $dest | Out-Null
# Left over from earlier installs, once nothing runs them any more.
Get-ChildItem $dest -Filter '*.old*' -ErrorAction SilentlyContinue |
    ForEach-Object { try { Remove-Item $_.FullName -Force -ErrorAction Stop } catch {} }
foreach ($exe in 'localbeat-studio.exe', 'localbeat.exe') {
    $target = Join-Path $dest $exe
    if (Test-Path $target) {
        try { Remove-Item $target -Force -ErrorAction Stop }
        catch {
            # Running: Windows won't delete it but lets it be renamed.
            Rename-Item $target "$exe.old-$(Get-Date -Format yyyyMMddHHmmss)"
        }
    }
    Copy-Item (Join-Path $repo "target\release\$exe") $target
}

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not (($userPath -split ';') -contains $dest)) {
    [Environment]::SetEnvironmentVariable('Path', ($userPath.TrimEnd(';') + ";$dest").TrimStart(';'), 'User')
    Write-Host "Added $dest to your PATH (new terminals)"
}

$lnk = Join-Path ([Environment]::GetFolderPath('Programs')) 'LocalBeat Studio.lnk'
$shell = New-Object -ComObject WScript.Shell
$s = $shell.CreateShortcut($lnk)
$s.TargetPath = Join-Path $dest 'localbeat-studio.exe'
$s.WorkingDirectory = $dest
$s.Description = 'Music library, player, CD ripper and phone sync'
$s.IconLocation = "$(Join-Path $dest 'localbeat-studio.exe'),0"
$s.Save()

Write-Host ''
Write-Host "Installed to $dest"
Write-Host 'Start menu: LocalBeat Studio. Command line (new terminal): localbeat --help'
