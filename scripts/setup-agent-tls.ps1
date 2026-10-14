# Create a TLS certificate for the uXa Server Agent and switch the agent to HTTPS.
# Run from an elevated PowerShell on the game server (Windows PowerShell 5.1 or PowerShell 7).
#
#   powershell -ExecutionPolicy Bypass -File setup-agent-tls.ps1                 # IP 71.71.45.128
#   powershell -ExecutionPolicy Bypass -File setup-agent-tls.ps1 -Ip 127.0.0.1   # local test
#
# The launcher uses rustls, which will not accept a self-signed leaf as its own root.
# So this makes a small private CA ("uXa Agent CA") and a server certificate signed by it.
# Admin PCs trust only agent.cer (the CA's public certificate). The CA private key is
# deleted after signing, so nothing else can ever be issued under it.
#
# Output in -AgentDir:
#   agent.crt  server certificate + CA (PEM)      -> agent.json tlsCert
#   agent.key  server private key (PKCS#8 PEM)     -> agent.json tlsKey   (SYSTEM/Administrators only)
#   agent.cer  CA public certificate (DER)         -> give to admins for trust-agent-cert.ps1

param(
    [string]$Ip = "71.71.45.128",
    [string]$AgentDir = "C:\uXa\ServerAgent",
    [int]$Years = 5,
    [string]$Service = "uXaServerAgent",
    [switch]$NoRestart,
    # Testing without elevation only: let this account read agent.key.
    [string]$ExtraKeyReader = ""
)
$ErrorActionPreference = "Stop"

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin -and -not $ExtraKeyReader) { throw "Run this from an elevated PowerShell." }
New-Item -ItemType Directory -Force -Path $AgentDir | Out-Null

function To-Pem([byte[]]$bytes, [string]$label) {
    $b64 = [Convert]::ToBase64String($bytes)
    $lines = for ($i = 0; $i -lt $b64.Length; $i += 64) { $b64.Substring($i, [Math]::Min(64, $b64.Length - $i)) }
    "-----BEGIN $label-----`n" + ($lines -join "`n") + "`n-----END $label-----`n"
}

# Build both certificates in memory (.NET Framework 4.7.2+ / PowerShell 7). Nothing touches the Windows certificate store.
$C = "System.Security.Cryptography"
$X = "System.Security.Cryptography.X509Certificates"
$notBefore = [DateTimeOffset]::UtcNow.AddDays(-1)
$notAfter = [DateTimeOffset]::UtcNow.AddYears($Years)
$sha = [Security.Cryptography.HashAlgorithmName]::SHA256
$pad = [Security.Cryptography.RSASignaturePadding]::Pkcs1

$caKey = New-Object "$C.RSACng" 3072
$caReq = New-Object "$X.CertificateRequest" "CN=uXa Agent CA ($Ip)", $caKey, $sha, $pad
$caReq.CertificateExtensions.Add((New-Object "$X.X509BasicConstraintsExtension" $true, $true, 0, $true))
$caReq.CertificateExtensions.Add((New-Object "$X.X509KeyUsageExtension" ([Security.Cryptography.X509Certificates.X509KeyUsageFlags]"KeyCertSign,CrlSign,DigitalSignature"), $true))
$caReq.CertificateExtensions.Add((New-Object "$X.X509SubjectKeyIdentifierExtension" $caReq.PublicKey, $false))
$ca = $caReq.CreateSelfSigned($notBefore, $notAfter.AddDays(1))

$leafKey = New-Object "$C.RSACng" 2048
$leafReq = New-Object "$X.CertificateRequest" "CN=$Ip", $leafKey, $sha, $pad
$san = New-Object "$X.SubjectAlternativeNameBuilder"
$san.AddIpAddress([Net.IPAddress]::Parse($Ip))
$leafReq.CertificateExtensions.Add($san.Build($false))
$eku = New-Object "$C.OidCollection"
[void]$eku.Add((New-Object "$C.Oid" "1.3.6.1.5.5.7.3.1"))
$leafReq.CertificateExtensions.Add((New-Object "$X.X509EnhancedKeyUsageExtension" $eku, $false))
$leafReq.CertificateExtensions.Add((New-Object "$X.X509BasicConstraintsExtension" $false, $false, 0, $true))
$leafReq.CertificateExtensions.Add((New-Object "$X.X509KeyUsageExtension" ([Security.Cryptography.X509Certificates.X509KeyUsageFlags]"DigitalSignature,KeyEncipherment"), $true))
$serial = New-Object byte[] 16
[Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($serial)
$serial[0] = $serial[0] -band 0x7F
$leaf = $leafReq.Create($ca, $notBefore, $notAfter, $serial)

# Ephemeral CNG keys allow plaintext export, so PKCS#8 works on Windows PowerShell 5.1 too.
$pkcs8 = $leafKey.Key.Export([Security.Cryptography.CngKeyBlobFormat]::Pkcs8PrivateBlob)

$crtPath = Join-Path $AgentDir "agent.crt"
$keyPath = Join-Path $AgentDir "agent.key"
$cerPath = Join-Path $AgentDir "agent.cer"
[IO.File]::WriteAllText($crtPath, (To-Pem $leaf.RawData "CERTIFICATE") + (To-Pem $ca.RawData "CERTIFICATE"))
[IO.File]::WriteAllText($keyPath, (To-Pem $pkcs8 "PRIVATE KEY"))
[IO.File]::WriteAllBytes($cerPath, $ca.RawData)
[Array]::Clear($pkcs8, 0, $pkcs8.Length)
icacls $keyPath /inheritance:r /grant:r "SYSTEM:(R)" "Administrators:(F)" | Out-Null
if ($ExtraKeyReader) { icacls $keyPath /grant "$($ExtraKeyReader):(R)" | Out-Null }
$caThumb = $ca.Thumbprint
# The CA private key only ever lived in memory. Dropping it means nothing else can be issued under this CA.
$caKey.Dispose(); $leafKey.Dispose()
$cfgPath = Join-Path $AgentDir "agent.json"
if (Test-Path $cfgPath) {
    $cfg = Get-Content $cfgPath -Raw | ConvertFrom-Json
    $cfg | Add-Member -NotePropertyName tlsCert -NotePropertyValue $crtPath -Force
    $cfg | Add-Member -NotePropertyName tlsKey -NotePropertyValue $keyPath -Force
    [IO.File]::WriteAllText($cfgPath, ($cfg | ConvertTo-Json -Depth 8))
    Write-Host "agent.json: tlsCert and tlsKey set."
} else {
    Write-Host "No $cfgPath yet. Set tlsCert=$crtPath and tlsKey=$keyPath when you create it."
}

if (-not $NoRestart -and (Get-Service $Service -ErrorAction SilentlyContinue)) {
    Restart-Service $Service
    Write-Host "Restarted $Service."
}
Write-Host "Server certificate for IP $Ip, valid until $($notAfter.ToString('yyyy-MM-dd')). CA thumbprint $caThumb."
Write-Host "Give admins this file and have them run trust-agent-cert.ps1: $cerPath"
Write-Host "Open TCP 8787 inbound if it is not open yet:"
Write-Host "  New-NetFirewallRule -DisplayName 'uXa Server Agent' -Direction Inbound -Protocol TCP -LocalPort 8787 -Action Allow"