# Quick local benchmarks: startup, binary size, first token with a 4B model.
# Usage: powershell -File scripts/bench.ps1 [-Binary path\to\argent.exe]
param([string]$Binary = "target\release\argent.exe")

$ErrorActionPreference = "Stop"
if (-not (Test-Path $Binary)) { throw "missing binary: $Binary" }
$item = Get-Item $Binary
Write-Host ("binary:        {0:N1} MB" -f ($item.Length / 1MB))

$rounds = 10
$sw = [System.Diagnostics.Stopwatch]::StartNew()
for ($i = 0; $i -lt $rounds; $i++) {
  & $Binary --version | Out-Null
}
$sw.Stop()
Write-Host ("startup:       {0:N1} ms avg ({1} runs)" -f ($sw.Elapsed.TotalMilliseconds / $rounds), $rounds)

$model = $env:LOCAL_4B_MODEL
if (-not $model) { $model = "spark-4b" }
$base = $env:LOCAL_4B_URL
if (-not $base) { $base = "http://127.0.0.1:8080/v1" }
Write-Host "first token:   start scripts/local-4b.ps1, then:"
Write-Host ("  Measure-Command {{ & '{0}' --provider local --base-url '{1}' -m '{2}' --thinking off 'hi' }}" -f $Binary, $base, $model)
Write-Host "cache missed:  argent usage --explain --all"
