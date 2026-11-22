# Native Windows dev run: the bot (release build) on 127.0.0.1:$HttpPort plus the panel through Vite on
# 0.0.0.0:5173 (Vite proxies /backend to the bot, like nginx in the image). Point the NPM proxy host
# at this PC, port 5173, with websockets on. Stop with Ctrl+C (stops both).
#
#   powershell -ExecutionPolicy Bypass -File scripts\dev-windows.ps1 [-NoBuild]
param([switch]$NoBuild, [int]$HttpPort = 8080)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# Fresh PATH (rustup, cmake and deno may have been installed after this shell started).
$env:Path = [Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' + [Environment]::GetEnvironmentVariable('Path', 'User')
$deno = Get-Command deno -ErrorAction SilentlyContinue
if ($deno) { $env:Path = (Split-Path $deno.Source) + ';' + $env:Path }

$env:CARGO_TARGET_DIR = Join-Path $root 'target-win'
if (-not $NoBuild) {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
}

# Local folders instead of the container volumes; the bot always listens on localhost here.
$env:APPDATA_DIR = Join-Path $root 'appdata'
$env:DATA_DIR = Join-Path $root 'data'
$env:HTTP_HOST = '127.0.0.1'
$env:HTTP_PORT = "$HttpPort"
if (-not $env:LOG_LEVEL) { $env:LOG_LEVEL = 'info,lynx=debug,serenity=warn,songbird=warn' }

$bot = Start-Process -FilePath (Join-Path $env:CARGO_TARGET_DIR 'release\lynx.exe') -NoNewWindow -PassThru
$env:LYNX_BACKEND = "http://127.0.0.1:$HttpPort"
$web = Start-Process -FilePath 'pnpm.cmd' -ArgumentList 'dev', '--host', '0.0.0.0', '--port', '5173', '--strictPort' -WorkingDirectory (Join-Path $root 'web') -NoNewWindow -PassThru

try {
    Wait-Process -Id $bot.Id
} finally {
    foreach ($p in @($web, $bot)) { if ($p -and -not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue } }
    Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -match 'vite' -and $_.CommandLine -match [regex]::Escape($root) } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
}
