# Build the Windows release bundle for argent.
#
#   powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1
#
# The CLI/TUI and the native iced window need nothing but Windows itself.

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$version = (Select-String -Path "Cargo.toml" -Pattern '^version = ' |
    Select-Object -First 1).Line -replace '.*"(.*)".*', '$1'
if (-not $version) { $version = "0.2.0" }

Write-Host "building argent $version"
cargo build --release -p argent-cli -p argent-native

$name = "argent-$version-windows-x64"
$dist = Join-Path $root "dist\$name"
if (Test-Path $dist) { Remove-Item -Recurse -Force $dist }
New-Item -ItemType Directory -Force -Path $dist | Out-Null

Copy-Item "target\release\argent.exe" $dist
Copy-Item "target\release\argent-native.exe" $dist
Copy-Item "README.md" $dist
Copy-Item "LICENSE" $dist
Copy-Item "docs" $dist -Recurse
Copy-Item "examples" $dist -Recurse

$hashes = Get-ChildItem $dist -File -Recurse | ForEach-Object {
    $hash = (Get-FileHash -Algorithm SHA256 $_.FullName).Hash.ToLower()
    $relative = $_.FullName.Substring($dist.Length + 1).Replace('\', '/')
    "$hash  $relative"
}
$hashes | Set-Content (Join-Path $dist "SHA256SUMS") -Encoding ASCII

$zip = Join-Path $root "dist\$name.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $dist -DestinationPath $zip
Write-Host "wrote $zip"

# Optional signed update manifest. Publish the .json beside the zip.
$manifestPath = Join-Path $root "dist\$name.json"
$zipHash = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
$updateUrl = if ($env:ARGENT_UPDATE_URL) { $env:ARGENT_UPDATE_URL } else { "https://example.invalid/$name.zip" }
$notes = if ($env:ARGENT_UPDATE_NOTES) { $env:ARGENT_UPDATE_NOTES } else { "argent $version" }
$manifest = [ordered]@{
  version = $version
  channel = "stable"
  url = $updateUrl
  sha256 = $zipHash
  notes = $notes
  signature = ""
}
$utf8 = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json), $utf8)
if ($env:ARGENT_UPDATE_KEY) {
  & (Join-Path $dist "argent.exe") update sign --key $env:ARGENT_UPDATE_KEY --manifest $manifestPath
  Write-Host "signed update manifest: $manifestPath"
} else {
  Write-Host "unsigned update manifest: $manifestPath (pass ARGENT_UPDATE_KEY to sign)"
}