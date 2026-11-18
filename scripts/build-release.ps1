# Build the Windows release bundle for argent.
#
#   powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1
#
# The CLI/TUI and the native iced window need nothing but Windows itself.

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function New-DeterministicZip {
    param(
        [Parameter(Mandatory = $true)][string]$SourceDir,
        [Parameter(Mandatory = $true)][string]$ZipPath,
        [Parameter(Mandatory = $true)][string]$RootName
    )
    if (Test-Path $ZipPath) {
        Remove-Item -Force $ZipPath
    }
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem

    $files = Get-ChildItem $SourceDir -File -Recurse | Sort-Object {
        $_.FullName.Substring($SourceDir.Length + 1).Replace('\', '/')
    }
    $archive = [System.IO.Compression.ZipFile]::Open(
        $ZipPath,
        [System.IO.Compression.ZipArchiveMode]::Create
    )
    try {
        foreach ($file in $files) {
            $relative = $file.FullName.Substring($SourceDir.Length + 1).Replace('\', '/')
            $entry = $archive.CreateEntry(
                "$RootName/$relative",
                [System.IO.Compression.CompressionLevel]::Optimal
            )
            # Zip's minimum timestamp. Keeping it fixed makes the archive
            # byte-for-byte stable across builds.
            $entry.LastWriteTime = [DateTimeOffset]::Parse("1980-01-01T00:00:00+00:00")
            $input = [System.IO.File]::OpenRead($file.FullName)
            try {
                $output = $entry.Open()
                try {
                    $input.CopyTo($output)
                } finally {
                    $output.Dispose()
                }
            } finally {
                $input.Dispose()
            }
        }
    } finally {
        $archive.Dispose()
    }
}

$version = (Select-String -Path "Cargo.toml" -Pattern '^version = ' |
    Select-Object -First 1).Line -replace '.*"(.*)".*', '$1'
if (-not $version) { $version = "0.2.0" }

Write-Host "building argent $version"
cargo build --release --locked -p argent-cli -p argent-native

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

$hashes = Get-ChildItem $dist -File -Recurse |
    Sort-Object FullName |
    ForEach-Object {
        $hash = (Get-FileHash -Algorithm SHA256 $_.FullName).Hash.ToLower()
        $relative = $_.FullName.Substring($dist.Length + 1).Replace('\', '/')
        "$hash  $relative"
    }
$hashes | Set-Content (Join-Path $dist "SHA256SUMS") -Encoding ASCII

$zip = Join-Path $root "dist\$name.zip"
New-DeterministicZip -SourceDir $dist -ZipPath $zip -RootName $name
Write-Host "wrote $zip (deterministic)"

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