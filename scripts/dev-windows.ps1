# Native Windows dev run: the bot (release build) on 127.0.0.1:$HttpPort plus the built panel served by
# `vite preview` on 0.0.0.0:5173 (it proxies /backend to the bot, like nginx in the image). Point the NPM proxy host
# at this PC, port 5173, with websockets on. Stop with Ctrl+C (stops both).
# Admin panel (only with ADMIN_USERNAME + ADMIN_PASSWORD or ADMIN_PASSWORD_HASH in .env): there is no nginx
# here, so the bot itself serves the built admin UI (admin/web/dist, kept up to date by `vite build --watch`)
# and the admin API on http://127.0.0.1:$AdminPort (localhost only). Reload the page to see UI changes.
#
#   powershell -ExecutionPolicy Bypass -File scripts\dev-windows.ps1 [-NoBuild] [-AdminPort 8081]
param([switch]$NoBuild, [int]$HttpPort = 8080, [int]$AdminPort = 8081)

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
$env:ADMIN_HTTP_HOST = '127.0.0.1'
$env:ADMIN_HTTP_PORT = "$AdminPort"
$env:ADMIN_WEB_DIR = Join-Path $root 'admin\web\dist'
if (-not $env:LOG_LEVEL) { $env:LOG_LEVEL = 'info,lynx=debug,serenity=warn,songbird=warn' }

$bot = Start-Process -FilePath (Join-Path $env:CARGO_TARGET_DIR 'release\lynx.exe') -NoNewWindow -PassThru
$env:LYNX_BACKEND = "http://127.0.0.1:$HttpPort"
# The panel is served BUILT (like nginx in the image): a first build, then `vite build --watch` keeps
# web/dist up to date and `vite preview` serves it with the /backend proxy. Reopen the Activity to see
# panel changes. (The Vite dev server is not used inside Discord: no HMR in the Activity iframe.)
$webDir = Join-Path $root 'web'
Push-Location $webDir
pnpm exec vite build
if ($LASTEXITCODE -ne 0) { Pop-Location; throw 'panel build failed' }
Pop-Location
# Admin UI (its own app in admin\web): a first build, then a watcher; the bot serves admin\web\dist.
$adminDir = Join-Path $root 'admin\web'
Push-Location $adminDir
if (-not (Test-Path 'node_modules')) { pnpm install --frozen-lockfile }
pnpm exec vite build
if ($LASTEXITCODE -ne 0) { Pop-Location; throw 'admin build failed' }
Pop-Location
Write-Host "Admin panel: http://127.0.0.1:$AdminPort (needs ADMIN_USERNAME and ADMIN_PASSWORD in .env)"
$adminWatch = Start-Process -FilePath 'pnpm.cmd' -ArgumentList 'exec', 'vite', 'build', '--watch', '--emptyOutDir', 'false' -WorkingDirectory $adminDir -NoNewWindow -PassThru
$watch = Start-Process -FilePath 'pnpm.cmd' -ArgumentList 'exec', 'vite', 'build', '--watch', '--emptyOutDir', 'false' -WorkingDirectory $webDir -NoNewWindow -PassThru
$web = Start-Process -FilePath 'pnpm.cmd' -ArgumentList 'exec', 'vite', 'preview', '--host', '0.0.0.0', '--port', '5173', '--strictPort' -WorkingDirectory $webDir -NoNewWindow -PassThru

try {
    Wait-Process -Id $bot.Id
} finally {
    foreach ($p in @($web, $watch, $adminWatch, $bot)) { if ($p -and -not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue } }
    Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -match 'vite' -and $_.CommandLine -match [regex]::Escape($root) } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
}
