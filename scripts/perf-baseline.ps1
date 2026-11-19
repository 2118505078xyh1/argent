# Wrapper for the Python performance baseline script.
$root = Split-Path -Parent $PSScriptRoot
$python = "C:\Users\xyh\AppData\Local\Programs\Python\Python313\python.exe"
if (-not (Test-Path $python)) { $python = "python" }
& $python (Join-Path $root "scripts\perf-baseline.py")
exit $LASTEXITCODE
