<#
.SYNOPSIS
  Joins this Windows computer to the LocalBeat server: tunnel, music login
  and, with -Admin, sending rips. The Windows counterpart of
  tools/import-tunnel.py.

.DESCRIPTION
      pwsh -File tools\import-tunnel.ps1 [NAME] [-Admin]

  Runs `localbeat join` (the same as Studio's Settings page): from the home
  network it sends a request with this computer's own keys and shows a code;
  approve it on the server's admin page (https://<server>:8443) if the
  code matches; then WireGuard's tunnel is installed (one Windows permission
  prompt) and Studio's music server login is saved. Ctrl+C while waiting and
  run it again to carry on; -Cancel withdraws the request.

  Needs WireGuard for Windows (tools\install-windows.ps1 installs it; or
  Install on Studio's Settings page).
  Close LocalBeat Studio first: it would write its old settings back.
#>
param(
    [Parameter(Position = 0)][string]$Name,
    [switch]$Admin,
    [Parameter(Mandatory = $true)][string]$Server,
    [switch]$Cancel
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$exe = @(
    (Join-Path $env:LOCALAPPDATA 'Programs\LocalBeat Studio\localbeat.exe'),
    (Join-Path $repo 'target\release\localbeat.exe'),
    (Join-Path $repo 'target\debug\localbeat.exe')
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $exe) { throw 'localbeat.exe not found: run tools\install-windows.ps1 first' }
if (Get-Process localbeat-studio -ErrorAction SilentlyContinue) {
    throw 'Close LocalBeat Studio first (it would write its old settings back)'
}
$a = @('join')
if ($Name) { $a += $Name }
if ($Admin) { $a += '--admin' }
if ($Cancel) { $a += '--cancel' }
$a += @('--server', $Server)
& $exe @a
exit $LASTEXITCODE
