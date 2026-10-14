# Trust the uXa Server Agent's certificate on an admin PC, so the uXa Launcher can open https://<server>:8787.
#
#   powershell -ExecutionPolicy Bypass -File trust-agent-cert.ps1 -Cer .\agent.cer                 # this user only
#   powershell -ExecutionPolicy Bypass -File trust-agent-cert.ps1 -Cer .\agent.cer -Machine        # all users (elevated)
#   powershell -ExecutionPolicy Bypass -File trust-agent-cert.ps1 -Cer .\agent.cer -Remove         # undo
#
# agent.cer is the public "uXa Agent CA" certificate made by setup-agent-tls.ps1. It holds no private key.

param(
    [Parameter(Mandatory = $true)][string]$Cer,
    [switch]$Machine,
    [switch]$Remove
)
$ErrorActionPreference = "Stop"
$cert = New-Object Security.Cryptography.X509Certificates.X509Certificate2 (Resolve-Path $Cer).Path
if ($cert.HasPrivateKey) { throw "This file contains a private key. Use the public agent.cer only." }
$isCa = $cert.Extensions | Where-Object { $_ -is [Security.Cryptography.X509Certificates.X509BasicConstraintsExtension] -and $_.CertificateAuthority }
if (-not $isCa) { Write-Warning "This certificate is not marked as a CA. The launcher (rustls) may still reject the agent." }
$store = if ($Machine) { "Cert:\LocalMachine\Root" } else { "Cert:\CurrentUser\Root" }
if ($Remove) {
    Remove-Item "$store\$($cert.Thumbprint)" -ErrorAction SilentlyContinue
    Write-Host "Removed $($cert.Subject) ($($cert.Thumbprint)) from $store."
    return
}
Write-Host "Importing $($cert.Subject), thumbprint $($cert.Thumbprint), into $store."
Write-Host "Windows may ask you to confirm. Check the thumbprint against the one the server admin gave you."
Import-Certificate -FilePath (Resolve-Path $Cer).Path -CertStoreLocation $store | Out-Null
Write-Host "Done. Restart the uXa Launcher."