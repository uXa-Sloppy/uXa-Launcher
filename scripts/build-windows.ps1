# Build the uXa Launcher NSIS installer on Windows.
# Requires Node.js 22+, Rust stable, and the Visual Studio C++ build tools.
$ErrorActionPreference = "Stop"
Set-Location (Split-Path -Parent $PSScriptRoot)

if (-not (Get-Command npm -ErrorAction SilentlyContinue)) {
    throw "Install Node.js 22 or newer and reopen the shell."
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "Install Rust stable from https://rustup.rs and reopen the shell."
}

npm ci
npm test
cargo build -p uxa-server-agent --release
npm run tauri build -- --bundles nsis

$installer = Get-ChildItem -Path "src-tauri\target\release\bundle\nsis\*.exe" | Select-Object -First 1
if (-not $installer) {
    throw "NSIS installer was not produced."
}
Write-Host "Installer: $($installer.FullName)"
$agent = Join-Path (Get-Location) "target\release\uxa-server-agent.exe"
if (Test-Path $agent) {
    Write-Host "Server agent: $agent"
}
