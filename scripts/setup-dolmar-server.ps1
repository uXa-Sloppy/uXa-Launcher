# One-shot setup of the uXa Server Agent on the Friday Night Fights game server.
#
#   powershell -ExecutionPolicy Bypass -File setup-dolmar-server.ps1
#
# It asks for: the agent admin key, a read-only GitHub token, and the FNF server folder.
# It then downloads the agent from the release, installs the uXaServerAgent service,
# turns on HTTPS, opens the firewall, and prints agent.cer for the admins.
# Secrets are never printed, never put on a command line, and never written to agent.json.
#
# -DryRun downloads and checks everything, then prints what it would do. No admin rights needed, nothing changed.

param(
    [string]$Ip = "71.71.45.128",
    [string]$ServerDir = "",
    [string]$AgentDir = "C:\uXa\ServerAgent",
    [string]$Tag = "v0.9.5",
    [string]$Repo = "uXa-Sloppy/FridayNightFights",
    [int]$GamePort = 7777,
    [int]$AgentPort = 8788,
    [switch]$DryRun
)
$ErrorActionPreference = "Stop"
$Service = "uXaServerAgent"

# --- Elevate -----------------------------------------------------------------------------------
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin -and -not $DryRun) {
    Write-Host "Asking Windows for administrator rights..."
    $argList = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-NoExit", "-File", "`"$PSCommandPath`"")
    foreach ($k in $PSBoundParameters.Keys) {
        $v = $PSBoundParameters[$k]
        if ($v -is [switch]) { if ($v) { $argList += "-$k" } } else { $argList += "-$k"; $argList += "`"$v`"" }
    }
    Start-Process powershell.exe -Verb RunAs -ArgumentList $argList
    return
}

function Step($msg) { Write-Host ""; Write-Host "==> $msg" -ForegroundColor Cyan }
function Plain([Security.SecureString]$s) {
    $p = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($s)
    try { [Runtime.InteropServices.Marshal]::PtrToStringBSTR($p) } finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($p) }
}
function Ask-Secret($prompt, $envName) {
    $pre = [Environment]::GetEnvironmentVariable($envName, "Process")
    if ($pre) { Write-Host "$prompt : (taken from `$env:$envName)"; return $pre.Trim() }
    while ($true) {
        $v = (Plain (Read-Host -AsSecureString $prompt)).Trim()
        if ($v) { return $v }
        Write-Host "Cannot be empty."
    }
}

Write-Host "Friday Night Fights server setup ($Tag)" -ForegroundColor Green
if ($DryRun) { Write-Host "DRY RUN: nothing will be installed or changed." -ForegroundColor Yellow }

# --- Questions ---------------------------------------------------------------------------------
$secret = Ask-Secret "Agent admin key (the one Dave gave you)" "UXA_AGENT_SECRET"
$token = Ask-Secret "Read-only GitHub token" "UXA_GITHUB_TOKEN"

if (-not $ServerDir) {
    $default = "C:\FNF\Server"
    $cfg = Join-Path $AgentDir "agent.json"
    if (Test-Path $cfg) {
        try { $old = (Get-Content $cfg -Raw | ConvertFrom-Json).installDir; if ($old) { $default = $old } } catch {}
    } else {
        foreach ($c in @("C:\FNF\Server", "C:\FNF\FridayNightFightsServer", "C:\uXa\FridayNightFightsServer", "D:\FNF\Server")) {
            if (Test-Path (Join-Path $c "FridayNightFightsServer.exe")) { $default = $c; break }
        }
    }
    $ans = Read-Host "FNF server folder [$default]"
    $ServerDir = if ($ans.Trim()) { $ans.Trim().Trim('"') } else { $default }
}

# --- Download ----------------------------------------------------------------------------------
Step "Downloading the agent from $Repo $Tag"
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
$ProgressPreference = "SilentlyContinue"
$api = @{ Authorization = "Bearer $token"; "User-Agent" = "uxa-setup"; Accept = "application/vnd.github+json" }
try {
    $rel = Invoke-RestMethod -Headers $api "https://api.github.com/repos/$Repo/releases/tags/$Tag"
} catch {
    throw "Could not read release $Tag. Check the GitHub token (it needs read access to $Repo)."
}
$work = Join-Path $env:TEMP ("uxa-setup-" + [Guid]::NewGuid().ToString("N").Substring(0, 8))
New-Item -ItemType Directory -Force $work | Out-Null
$need = @("uxa-server-agent.exe", "install-server-agent.ps1", "agent.example.json", "setup-agent-tls.ps1")
foreach ($name in $need) {
    $asset = $rel.assets | Where-Object { $_.name -eq $name } | Select-Object -First 1
    if (-not $asset) { throw "Release $Tag has no asset named $name." }
    $dest = Join-Path $work $name
    # The asset URL redirects to a signed download link. HttpWebRequest drops the token on that redirect, which is what we want.
    Invoke-WebRequest -UseBasicParsing -Headers @{ Authorization = "Bearer $token"; "User-Agent" = "uxa-setup"; Accept = "application/octet-stream" } -Uri $asset.url -OutFile $dest
    if ((Get-Item $dest).Length -ne $asset.size) { throw "$name downloaded with the wrong size." }
    Write-Host "  $name ($($asset.size) bytes)"
}
Get-ChildItem $work | Unblock-File

if (-not (Test-Path (Join-Path $ServerDir "FridayNightFightsServer.exe"))) {
    Write-Warning "$ServerDir has no FridayNightFightsServer.exe yet. That is fine: the agent installs the server there on the first deploy from the launcher."
}

if ($DryRun) {
    Step "Dry run: would now"
    Write-Host "  install the $Service service in $AgentDir (admin key goes to agent.secret, SYSTEM/Administrators only)"
    Write-Host "  set agent.json installDir=$ServerDir, port=$GamePort, listen=0.0.0.0:$AgentPort"
    Write-Host "  set machine env UXA_GITHUB_TOKEN (and the service's own environment)"
    Write-Host "  create the TLS certificate for $Ip and set tlsCert/tlsKey"
    Write-Host "  open firewall UDP $GamePort and TCP $AgentPort, restart $Service, check port $AgentPort"
    Remove-Item $work -Recurse -Force
    $secret = $null; $token = $null
    Write-Host ""; Write-Host "Dry run OK: token works, all 4 files downloaded." -ForegroundColor Green
    return
}

# --- Install -----------------------------------------------------------------------------------
Step "Installing the $Service service"
New-Item -ItemType Directory -Force $AgentDir | Out-Null
$cfgPath = Join-Path $AgentDir "agent.json"
$cfgSrc = if (Test-Path $cfgPath) { $cfgPath } else { Join-Path $work "agent.example.json" }
$cfg = Get-Content $cfgSrc -Raw | ConvertFrom-Json
$cfg.installDir = $ServerDir
$cfg.port = $GamePort
$cfg.listen = "0.0.0.0:$AgentPort"
$cfgTmp = Join-Path $work "agent.json"
[IO.File]::WriteAllText($cfgTmp, ($cfg | ConvertTo-Json -Depth 8))
# Same process: the key is passed through this process's environment only, never on a command line.
$env:UXA_AGENT_SECRET = $secret
try {
    & (Join-Path $work "install-server-agent.ps1") -InstallDir $AgentDir -Binary (Join-Path $work "uxa-server-agent.exe") -Config $cfgTmp
} finally {
    Remove-Item Env:UXA_AGENT_SECRET -ErrorAction SilentlyContinue
    $secret = $null
}

Step "Saving the GitHub token for the agent"
[Environment]::SetEnvironmentVariable("UXA_GITHUB_TOKEN", $token, "Machine")
# Services only see new machine variables after a reboot, so also give the service its own copy now.
$svcKey = "HKLM:\SYSTEM\CurrentControlSet\Services\$Service"
New-ItemProperty -Path $svcKey -Name Environment -PropertyType MultiString -Value @("UXA_GITHUB_TOKEN=$token") -Force | Out-Null
$token = $null
Write-Host "  UXA_GITHUB_TOKEN set (machine and service). Not printed."

Step "Turning on HTTPS for $Ip"
& (Join-Path $work "setup-agent-tls.ps1") -Ip $Ip -AgentDir $AgentDir -NoRestart

Step "Opening the firewall"
foreach ($r in @(@{ n = "FNF Game Server (UDP $GamePort)"; p = "UDP"; port = $GamePort }, @{ n = "uXa Server Agent (TCP $AgentPort)"; p = "TCP"; port = $AgentPort })) {
    Get-NetFirewallRule -DisplayName $r.n -ErrorAction SilentlyContinue | Remove-NetFirewallRule
    New-NetFirewallRule -DisplayName $r.n -Direction Inbound -Protocol $r.p -LocalPort $r.port -Action Allow -Profile Any | Out-Null
    Write-Host "  $($r.n)"
}

Step "Restarting $Service"
Restart-Service $Service -Force
$listening = $false
for ($i = 0; $i -lt 15 -and -not $listening; $i++) {
    Start-Sleep 1
    $listening = [bool](Get-NetTCPConnection -State Listen -LocalPort $AgentPort -ErrorAction SilentlyContinue)
}
$svc = Get-Service $Service
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue

# --- Summary -----------------------------------------------------------------------------------
$cer = Join-Path $AgentDir "agent.cer"
$thumb = (New-Object Security.Cryptography.X509Certificates.X509Certificate2 $cer).Thumbprint
$ok = $svc.Status -eq "Running" -and $listening
Write-Host ""
Write-Host "================ Summary ================" -ForegroundColor Green
Write-Host ("Service {0}: {1}" -f $Service, $svc.Status)
Write-Host ("Agent port {0}: {1}" -f $AgentPort, $(if ($listening) { "listening (HTTPS)" } else { "NOT listening - see logs in $AgentDir" }))
Write-Host "Server folder: $ServerDir   game port: UDP $GamePort"
Write-Host "Agent address for the launcher: https://$($Ip):$AgentPort"
Write-Host ""
Write-Host "Send this file to Dave and the admins (it is public, no secrets):" -ForegroundColor Yellow
Write-Host "  $cer"
Write-Host "  CA thumbprint: $thumb"
Write-Host "Do NOT send agent.key or agent.secret."
Write-Host ""
Write-Host "Router: forward UDP $GamePort (game) and TCP $AgentPort (agent) to this PC's LAN IP." -ForegroundColor Yellow
Write-Host "        $Ip must be this network's public IP."
if ($ok) { Write-Host "All done." -ForegroundColor Green } else { Write-Host "Something is not right. Send Dave a screenshot of this window." -ForegroundColor Red }
explorer.exe /select,"$cer"
