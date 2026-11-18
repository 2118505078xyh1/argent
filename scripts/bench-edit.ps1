# Compare hashline vs exact-text edit on a local OpenAI-compatible model.
#
#   powershell -File scripts\bench-edit.ps1
#   powershell -File scripts\bench-edit.ps1 -Trials 3
#
# Requires a local llama-server exposing /v1 (see scripts/local-4b.ps1).
# The script uses a throwaway workspace and reports success, wall time and
# token counts. It is intentionally small: it measures the model's edit
# behavior, not the provider.

param(
    [string]$BaseUrl = "http://127.0.0.1:8080/v1",
    [string]$Model = "spark-4b",
    [string]$Argent = "",
    [int]$Trials = 1
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
if (-not $Argent) {
    $Argent = Join-Path $root "target\debug\argent.exe"
}
if (-not (Test-Path $Argent)) {
    throw "argent binary not found: $Argent"
}
try {
    Invoke-WebRequest -Uri "$BaseUrl/models" -UseBasicParsing -TimeoutSec 5 | Out-Null
} catch {
    throw "local provider is not ready at $BaseUrl; start scripts/local-4b.ps1 first"
}

$work = Join-Path $env:TEMP ("argent-edit-bench-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $work | Out-Null
$file = Join-Path $work "bench.txt"
$seed = "a`nb`nc`nd`ne`nf`ng`nh`n"

function Invoke-Case {
    param([string]$Mode, [string]$Prompt)
    Set-Content -Path $file -Value $seed -NoNewline -Encoding ASCII
    $env:ARGENT_EDIT_MODE = $Mode
    $log = Join-Path $work "$Mode.log"
    $started = Get-Date
    $output = & $Argent --provider local --base-url $BaseUrl -m $Model `
        --thinking off --max-turns 8 --approval never $Prompt 2>&1
    $elapsed = ((Get-Date) - $started).TotalSeconds
    $output | Set-Content -Path $log -Encoding UTF8
    $lines = @(Get-Content -Path $file)
    $passed = $lines.Count -eq 8 -and $lines[5] -eq "F" -and $lines[6] -eq "g"
    $tokens = ($output | Select-String -Pattern '([0-9]+) token\(s\)' |
        Select-Object -Last 1).Matches.Groups[1].Value
    [pscustomobject]@{
        mode = $Mode
        passed = $passed
        seconds = [math]::Round($elapsed, 1)
        tokens = if ($tokens) { [int]$tokens } else { 0 }
        log = $log
    }
}

$results = @()
for ($i = 1; $i -le $Trials; $i++) {
    $results += Invoke-Case "hashline" `
        "Read bench.txt with hash=true, then replace line 6 (f) with F. Use only read and edit; do not use bash. Reply DONE when finished."
    $results += Invoke-Case "replace" `
        "Read bench.txt, then use edit with oldText f and newText F exactly once. Do not use bash. Reply DONE when finished."
}
$results | Format-Table -AutoSize

$summary = $results | Group-Object mode | ForEach-Object {
    [pscustomobject]@{
        mode = $_.Name
        runs = $_.Count
        passed = ($_.Group | Where-Object { $_.passed }).Count
        avg_seconds = [math]::Round(($_.Group | Measure-Object seconds -Average).Average, 1)
        avg_tokens = [math]::Round(($_.Group | Measure-Object tokens -Average).Average, 0)
    }
}
Write-Host ""
$summary | Format-Table -AutoSize
Write-Host "logs: $work"
Remove-Item Env:ARGENT_EDIT_MODE -ErrorAction SilentlyContinue
