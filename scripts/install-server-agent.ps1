# Install the uXa Server Agent as a Windows service.
# Run this from an elevated PowerShell on the game server.
#
#   powershell -ExecutionPolicy Bypass -File scripts\install-server-agent.ps1 -Secret $env:UXA_AGENT_SECRET
#
# The admin key is written to agent.secret next to the executable and locked
# down to SYSTEM and Administrators. It is not placed in agent.json.

param(
    [string]$InstallDir = "C:\uXa\ServerAgent",
    [string]$Binary = "",
    [string]$Config = "",
    [string]$Secret = ""
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot

if (-not $Binary) {
    $candidate = Join-Path $repo "target\release\uxa-server-agent.exe"
    if (Test-Path $candidate) { $Binary = $candidate }
}
if (-not $Binary -or -not (Test-Path $Binary)) {
    throw "Build the agent first (cargo build -p uxa-server-agent --release) or pass -Binary."
}
if (-not $Secret) { $Secret = $env:UXA_AGENT_SECRET }
if ([string]::IsNullOrWhiteSpace($Secret)) {
    throw "Pass -Secret or set UXA_AGENT_SECRET. The agent has no built-in key, and the key does not belong in agent.json."
}

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Copy-Item -Force $Binary (Join-Path $InstallDir "uxa-server-agent.exe")

$configDest = Join-Path $InstallDir "agent.json"
if ($Config) {
    Copy-Item -Force $Config $configDest
} elseif (-not (Test-Path $configDest)) {
    Copy-Item (Join-Path $repo "config\agent.example.json") $configDest
    Write-Host "Wrote $configDest from the example. Set installDir to the dedicated server folder, then start the service again."
}

$secretPath = Join-Path $InstallDir "agent.secret"
[System.IO.File]::WriteAllText($secretPath, $Secret.Trim())
icacls $secretPath /inheritance:r /grant:r "SYSTEM:(R)" "Administrators:(R)" | Out-Null

$binPath = Join-Path $InstallDir "uxa-server-agent.exe"
sc.exe stop uXaServerAgent | Out-Null
sc.exe delete uXaServerAgent | Out-Null
Start-Sleep -Seconds 1
sc.exe create uXaServerAgent binPath= "`"$binPath`" --service" start= auto DisplayName= "uXa Server Agent" | Out-Null
if ($LASTEXITCODE -ne 0) { throw "sc create failed. Run this script from an elevated PowerShell." }
sc.exe description uXaServerAgent "Deploys and restarts the Friday Night Fights dedicated server." | Out-Null
sc.exe start uXaServerAgent | Out-Null
if ($LASTEXITCODE -ne 0) {
    Write-Host "Service created, but it did not start. Check agent.json installDir and the log under the work directory."
    exit 1
}
Write-Host "uXa Server Agent is installed as the uXaServerAgent service."
Write-Host "Listening on the listen address in agent.json (default 0.0.0.0:8787)."
Write-Host "The admin key is in $secretPath and is not stored in agent.json."
