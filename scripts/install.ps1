# Install argent into ~/.local/bin. Close running argent windows first.
param([string]$Source = "target\release\argent.exe")

$ErrorActionPreference = "Stop"
if (-not (Test-Path $Source)) { throw "missing binary: $Source" }
$dest = Join-Path $env:USERPROFILE ".local\bin\argent.exe"
New-Item -ItemType Directory -Force (Split-Path $dest) | Out-Null
if (Test-Path $dest) {
  $locked = $false
  try { Copy-Item $Source $dest -Force -ErrorAction Stop }
  catch {
    $locked = $true
    Move-Item $dest ($dest + ".prev") -Force
    Copy-Item $Source $dest -Force
  }
} else { Copy-Item $Source $dest -Force }
$hash = (Get-FileHash $dest -Algorithm MD5).Hash.ToLowerInvariant()
Write-Host "installed $dest ($hash)"
Write-Host "quit and reopen argent for the new binary to take effect."
