# Wrapper for the Python performance baseline script.
$root = Split-Path -Parent $PSScriptRoot
$python = if ($env:PYTHON) { $env:PYTHON } else { "python" }
& $python (Join-Path $root "scripts\perf-baseline.py")
exit $LASTEXITCODE
