# One-shot verification for the Tauri migration prototype.
# Run from anywhere: powershell -File scripts/verify-tauri.ps1

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$tauri = Join-Path $root "crates\tauri-app"
$ui = Join-Path $tauri "ui"
$bun = "C:\Users\xyh\.bun\bin\bun.exe"
$python = "C:\Users\xyh\AppData\Local\Programs\Python\Python313\python.exe"
if (-not (Test-Path $python)) { $python = "python" }

function Step([string]$name, [scriptblock]$body) {
    Write-Host "`n== $name ==" -ForegroundColor Cyan
    & $body
    if ($LASTEXITCODE -ne 0) {
        throw "$name failed with exit code $LASTEXITCODE"
    }
}

Step "version consistency" { & $python (Join-Path $root "scripts\check-tauri-version.py") }

Push-Location $ui
try {
    Step "ui typecheck" { & $bun run typecheck }
    Step "ui unit tests" { & $bun run test }
    Step "ui build" { & $bun run build }
}
finally { Pop-Location }

Push-Location $tauri
try {
    Step "rust fmt" { cargo fmt --all -- --check }
    Step "rust tests" { cargo test --bin argent-tauri }
    Step "rust clippy" { cargo clippy --all-targets -- -D warnings }
    Step "rust build" { cargo build }
}
finally { Pop-Location }

Push-Location $root
try {
    Step "CDP end-to-end smoke" { & $python (Join-Path $root "scripts\tauri-smoke.py") }
}
finally { Pop-Location }

Write-Host "`nAll Tauri checks passed." -ForegroundColor Green
