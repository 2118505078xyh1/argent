# Start a local llama.cpp 4B server and exercise argent against it.
#
#   powershell -File scripts/local-4b.ps1              # start + one-shot smoke
#   powershell -File scripts/local-4b.ps1 -Tui         # start + interactive TUI
#   powershell -File scripts/local-4b.ps1 -Stop        # stop the server
#
# Overrides:
#   $env:LOCAL_4B_EXE     llama-server.exe   (default D:\code\local_text\llama.cpp\llama-server.exe)
#   $env:LOCAL_4B_MODEL   .gguf              (default D:\code\local_text\models\Spark-X2.6-4B-Q4_K_M.gguf)
#   $env:LOCAL_4B_PORT    port               (default 8080)
#   $env:ARGENT_BIN       argent.exe         (default ~/.local/bin/argent.exe)
param(
    [switch]$Tui,
    [switch]$Stop,
    [string]$Prompt = "用一句话回答：你是什么模型？"
)

$ErrorActionPreference = 'Stop'

$exe    = if ($env:LOCAL_4B_EXE)   { $env:LOCAL_4B_EXE }   else { 'D:\code\local_text\llama.cpp\llama-server.exe' }
$model  = if ($env:LOCAL_4B_MODEL) { $env:LOCAL_4B_MODEL } else { 'D:\code\local_text\models\Spark-X2.6-4B-Q4_K_M.gguf' }
$port   = if ($env:LOCAL_4B_PORT)  { [int]$env:LOCAL_4B_PORT } else { 8080 }
$argent = if ($env:ARGENT_BIN)     { $env:ARGENT_BIN }     else { Join-Path $HOME '.local\bin\argent.exe' }
$logDir = Split-Path $model -Parent
$outLog = Join-Path $logDir 'spark-server.out.log'
$errLog = Join-Path $logDir 'spark-server.err.log'

function Get-ServerProcess {
    Get-CimInstance Win32_Process -Filter "Name = 'llama-server.exe'" |
        Where-Object { $_.CommandLine -match "--port\s+$port(\s|$)" }
}

function Wait-Ready {
    param([int]$Seconds = 60)
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        try {
            $response = Invoke-WebRequest -UseBasicParsing -TimeoutSec 2 "http://127.0.0.1:$port/v1/models"
            if ($response.StatusCode -eq 200) { return $true }
        } catch {
            Start-Sleep -Milliseconds 500
        }
    }
    return $false
}

if ($Stop) {
    Get-ServerProcess | ForEach-Object {
        Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
        Write-Host "stopped llama-server pid=$($_.ProcessId)"
    }
    return
}

if (-not (Test-Path $exe))   { throw "llama-server not found: $exe" }
if (-not (Test-Path $model)) { throw "model not found: $model" }
if (-not (Test-Path $argent)) { throw "argent not found: $argent (set ARGENT_BIN)" }

if (-not (Get-ServerProcess)) {
    Write-Host "starting llama.cpp on 127.0.0.1:$port"
    Start-Process -FilePath $exe `
        -ArgumentList '-m', $model, '--alias', 'spark-4b', '-c', '16384', '-ngl', '99', '--port', "$port", '--jinja' `
        -RedirectStandardOutput $outLog `
        -RedirectStandardError $errLog `
        -WindowStyle Hidden
    if (-not (Wait-Ready)) {
        Write-Host "server did not become ready; tail of $errLog"
        Get-Content $errLog -Tail 20 -ErrorAction SilentlyContinue
        exit 1
    }
} else {
    Write-Host "reusing llama-server on 127.0.0.1:$port"
}

$common = @(
    '--provider', 'local',
    '--base-url', "http://127.0.0.1:$port/v1",
    '-m', 'spark-4b',
    '--thinking', 'off'
)

if ($Tui) {
    & $argent @common
} else {
    & $argent @common --max-turns 1 $Prompt
}
exit $LASTEXITCODE
