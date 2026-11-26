<#
.SYNOPSIS
  Builds the LocalBeat Studio per-user Windows installer (.exe) using Inno Setup 6.

.DESCRIPTION
  1. Reads the workspace version from Cargo.toml (unless -Version is passed).
  2. Builds release binaries `localbeat-studio.exe` and `localbeat.exe`
     (unless -SkipBuild is passed).
  3. Compiles `packaging\windows\localbeat-studio.iss` with Inno Setup (`iscc.exe`)
     into `dist\LocalBeatStudio-<version>-windows-x64-setup.exe`.

  Install Inno Setup 6 once via:
      winget install JRSoftware.InnoSetup
#>
param(
    [string]$Version,
    [switch]$SkipBuild
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot

if (-not $Version) {
    $cargoToml = Get-Content (Join-Path $repo 'Cargo.toml') -Raw
    if ($cargoToml -match '(?m)^version\s*=\s*"([^"]+)"') {
        $Version = $Matches[1]
    } else {
        throw 'Could not determine version from Cargo.toml; pass -Version X.Y.Z'
    }
}

function Find-Iscc {
    $cmd = Get-Command iscc.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    foreach ($candidate in @(
        (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe'),
        'C:\Program Files (x86)\Inno Setup 6\ISCC.exe',
        'C:\Program Files\Inno Setup 6\ISCC.exe'
    )) {
        if (Test-Path $candidate) { return $candidate }
    }
    return $null
}

$iscc = Find-Iscc
if (-not $iscc) {
    throw "Inno Setup 6 (ISCC.exe) not found. Install it with: winget install JRSoftware.InnoSetup"
}

if (-not $SkipBuild) {
    Write-Host "Building LocalBeat Studio $Version (release)..."
    Push-Location $repo
    try {
        cargo build --release -p localbeat -p localbeat-studio
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
    } finally { Pop-Location }
}

$iss = Join-Path $repo 'packaging\windows\localbeat-studio.iss'
Write-Host "Compiling installer with $iscc..."
& $iscc "/DAppVersion=$Version" $iss
if ($LASTEXITCODE -ne 0) { throw "ISCC failed (exit $LASTEXITCODE)" }

$out = Join-Path $repo "dist\LocalBeatStudio-$Version-windows-x64-setup.exe"
Write-Host ''
Write-Host "Built: $out"
