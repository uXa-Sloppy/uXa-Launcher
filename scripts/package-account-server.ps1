# Build FNF_<version>_Accounts_Win64.zip: embeddable Python + the FNF account server scripts.
# The server agent (0.3.9+) installs it to FNFAccountServer next to the game server, runs it on
# TCP 7798, keeps it up, and swaps in new versions with rollback. Nothing needs to be installed
# on the game PC (no system Python).
#
#   powershell -ExecutionPolicy Bypass -File package-account-server.ps1 -Version 0.9.9 -Tools C:\FNF\Builds\FNF_purgatory_p1\Tools -OutDir E:\FNF\Release
#
# The name has no "Server" or "Client" in it on purpose: older launchers and agents match
# game builds on those words and must never pick this package up.
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$Tools,
    [string]$OutDir = ".",
    [string]$PythonVersion = "3.12.8"
)
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
$need = @("fnf_account_server.py", "fnf_account_store.py", "fnf_invite_key.py")
foreach ($f in $need) { if (-not (Test-Path (Join-Path $Tools $f))) { throw "$Tools has no $f (is PR #46 in this tree?)" } }
$stage = Join-Path $env:TEMP ("fnf-accounts-" + [Guid]::NewGuid().ToString("N").Substring(0, 8))
New-Item -ItemType Directory -Force (Join-Path $stage "python"), (Join-Path $stage "Tools") | Out-Null
$pyZip = Join-Path $env:TEMP "python-$PythonVersion-embed-amd64.zip"
if (-not (Test-Path $pyZip)) {
    Invoke-WebRequest -UseBasicParsing -Uri "https://www.python.org/ftp/python/$PythonVersion/python-$PythonVersion-embed-amd64.zip" -OutFile $pyZip
}
Write-Host ("python embeddable sha256 {0}" -f (Get-FileHash -Algorithm SHA256 $pyZip).Hash.ToLower())
Expand-Archive -LiteralPath $pyZip -DestinationPath (Join-Path $stage "python") -Force
# Embedded Python ignores PYTHONPATH and the script folder; list ..\Tools in its ._pth so
# fnf_account_server.py can import fnf_account_store.
$pth = Get-ChildItem (Join-Path $stage "python") -Filter "python*._pth" | Select-Object -First 1
if (-not $pth) { throw "No ._pth in the embeddable Python." }
Add-Content -LiteralPath $pth.FullName -Value "..\Tools"
foreach ($f in $need) { Copy-Item (Join-Path $Tools $f) (Join-Path $stage "Tools\$f") }
[IO.File]::WriteAllText((Join-Path $stage "VERSION.txt"), "v$Version")
# Smoke test: the packaged interpreter can import the store and answer --help.
& (Join-Path $stage "python\python.exe") (Join-Path $stage "Tools\fnf_account_server.py") --help | Out-Null
if ($LASTEXITCODE -ne 0) { throw "Packaged account server failed its smoke test." }
New-Item -ItemType Directory -Force $OutDir | Out-Null
$zip = Join-Path (Resolve-Path $OutDir) "FNF_$($Version)_Accounts_Win64.zip"
if (Test-Path $zip) { Remove-Item $zip }
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip
Remove-Item $stage -Recurse -Force
Write-Host ("Wrote {0} ({1:N0} bytes, sha256 {2})" -f $zip, (Get-Item $zip).Length, (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower())
