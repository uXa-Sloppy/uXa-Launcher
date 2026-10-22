# One-shot setup of an admin PC: installs the uXa Launcher and trusts the game server's agent.cer.
#
#   powershell -ExecutionPolicy Bypass -File setup-admin-pc.ps1                  # asks for agent.cer with a file picker
#   powershell -ExecutionPolicy Bypass -File setup-admin-pc.ps1 -Cer .\agent.cer
#
# Downloading the installer from the private release needs a read-only GitHub token (asked, never shown).
# Already have the installer? Pass -Installer <path>, or -SkipLauncher to only trust the certificate.

param(
    [string]$Cer = "",
    [string]$Installer = "",
    [switch]$SkipLauncher,
    [switch]$Machine,
    [string]$Tag = "",   # empty = the Latest release; pass -Tag vX.Y.Z to pin one
    [string]$Repo = "uXa-Sloppy/FridayNightFights",
    [string]$AssetName = "",   # empty = the newest uXa Launcher x64 installer on the release
    [switch]$DryRun
)
$ErrorActionPreference = "Stop"

if (-not $SkipLauncher) {
    if (-not $Installer) {
        $token = $env:UXA_GITHUB_TOKEN
        if (-not $token) {
            $s = Read-Host -AsSecureString "Read-only GitHub token"
            $p = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($s)
            try { $token = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($p).Trim() } finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($p) }
        }
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
        $ProgressPreference = "SilentlyContinue"
        try {
            $relUrl = if ($Tag) { "https://api.github.com/repos/$Repo/releases/tags/$Tag" } else { "https://api.github.com/repos/$Repo/releases/latest" }
            $rel = Invoke-RestMethod -Headers @{ Authorization = "Bearer $token"; "User-Agent" = "uxa-setup" } $relUrl
        } catch { throw "Could not read release $(if ($Tag) { $Tag } else { 'latest' }). Check the GitHub token." }
        if (-not $Tag) { $Tag = $rel.tag_name }
        Write-Host "Using release $Tag"
        if ($AssetName) {
            $asset = $rel.assets | Where-Object { $_.name -eq $AssetName } | Select-Object -First 1
        } else {
            $asset = $rel.assets | Where-Object { $_.name -like "uXa*Launcher_*_x64-setup.exe" } | Sort-Object name -Descending | Select-Object -First 1
            if ($asset) { $AssetName = $asset.name } else { $AssetName = "uXa Launcher x64 installer" }
        }
        if (-not $asset) { throw "Release $Tag has no $AssetName." }
        $Installer = Join-Path $env:TEMP $AssetName
        Write-Host "Downloading $AssetName ($([math]::Round($asset.size / 1MB, 1)) MB)..."
        Invoke-WebRequest -UseBasicParsing -Headers @{ Authorization = "Bearer $token"; "User-Agent" = "uxa-setup"; Accept = "application/octet-stream" } -Uri $asset.url -OutFile $Installer
        $token = $null
        if ((Get-Item $Installer).Length -ne $asset.size) { throw "Download has the wrong size." }
        Unblock-File $Installer
    }
    if ($DryRun) {
        Write-Host "Dry run: would run $Installer /S"
    } else {
        Write-Host "Installing the uXa Launcher..."
        $proc = Start-Process $Installer -ArgumentList "/S" -Wait -PassThru
        if ($proc.ExitCode -ne 0) { throw "Installer exited with code $($proc.ExitCode)." }
        Write-Host "uXa Launcher installed."
    }
}

if (-not $Cer) {
    Add-Type -AssemblyName System.Windows.Forms
    $dlg = New-Object System.Windows.Forms.OpenFileDialog
    $dlg.Title = "Pick agent.cer from the game server"
    $dlg.Filter = "Agent certificate (*.cer)|*.cer|All files (*.*)|*.*"
    $dlg.InitialDirectory = Join-Path $env:USERPROFILE "Downloads"
    if ($dlg.ShowDialog() -ne "OK") { throw "No certificate picked. Run again with -Cer <path to agent.cer>." }
    $Cer = $dlg.FileName
}
$cert = New-Object Security.Cryptography.X509Certificates.X509Certificate2 (Resolve-Path $Cer).Path
if ($cert.HasPrivateKey) { throw "This file contains a private key. Use the public agent.cer only." }
$isCa = $cert.Extensions | Where-Object { $_ -is [Security.Cryptography.X509Certificates.X509BasicConstraintsExtension] -and $_.CertificateAuthority }
if (-not $isCa) { throw "$Cer is not the uXa Agent CA certificate (agent.cer from setup-agent-tls.ps1)." }
$store = if ($Machine) { "Cert:\LocalMachine\Root" } else { "Cert:\CurrentUser\Root" }
Write-Host "Certificate: $($cert.Subject)"
Write-Host "Thumbprint:  $($cert.Thumbprint)   (should match what the server setup printed)"
if ($DryRun) {
    Write-Host "Dry run: would import it into $store."
} else {
    Write-Host "Windows will ask you to confirm installing it. Click Yes."
    Import-Certificate -FilePath (Resolve-Path $Cer).Path -CertStoreLocation $store | Out-Null
    Write-Host "Trusted. In the launcher, use the agent address https://<server IP>:8787 and your admin key." -ForegroundColor Green
}
