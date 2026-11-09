# Split a release zip that is too big for one GitHub asset (2 GiB limit).
#
#   powershell -ExecutionPolicy Bypass -File split-release-asset.ps1 -File E:\FNF\Release\FNF_0.9.9_Client_Win64.zip
#
# Files at or under -ThresholdBytes are left alone (upload them as a single asset).
# Bigger files become <name>.001, <name>.002, ... plus <name>.parts.json:
#   { "version":1, "file":"<name>", "size":N, "sha256":"<whole file>",
#     "parts":[ { "name":"<name>.001", "size":N, "sha256":"..." }, ... ] }
# Upload the parts and the .parts.json (NOT the original zip) to the release.
# Launcher/agent 0.3.9+ download each part with resume, check every hash, join, check
# the whole-file hash, then install. Launcher/agent 0.3.8 and older cannot read split
# assets: publish launcher/agent 0.3.9 first.
param(
    [Parameter(Mandatory = $true)][string]$File,
    [string]$OutDir = "",
    [long]$PartBytes = 1900MB,
    [long]$ThresholdBytes = 1900MB
)
$ErrorActionPreference = "Stop"
$src = Get-Item -LiteralPath $File
if (-not $OutDir) { $OutDir = Join-Path $src.DirectoryName ($src.Name + ".split") }
if ($PartBytes -ge 2GB) { throw "PartBytes must be under 2 GiB (GitHub's per-asset limit)." }
if ($src.Length -le $ThresholdBytes) {
    Write-Host "$($src.Name) is $([math]::Round($src.Length / 1GB, 3)) GB: under the threshold, upload it as one asset."
    return
}
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$whole = [Security.Cryptography.SHA256]::Create()
$in = [IO.File]::OpenRead($src.FullName)
$buf = New-Object byte[] (4MB)
$parts = @()
$index = 1
try {
    while ($in.Position -lt $in.Length) {
        $name = "{0}.{1:D3}" -f $src.Name, $index
        $path = Join-Path $OutDir $name
        $out = [IO.File]::Create($path)
        $partHash = [Security.Cryptography.SHA256]::Create()
        $written = [long]0
        try {
            while ($written -lt $PartBytes) {
                $want = [int][math]::Min($buf.Length, $PartBytes - $written)
                $n = $in.Read($buf, 0, $want)
                if ($n -le 0) { break }
                $out.Write($buf, 0, $n)
                [void]$partHash.TransformBlock($buf, 0, $n, $null, 0)
                [void]$whole.TransformBlock($buf, 0, $n, $null, 0)
                $written += $n
            }
        } finally { $out.Close() }
        [void]$partHash.TransformFinalBlock($buf, 0, 0)
        $hex = -join ($partHash.Hash | ForEach-Object { $_.ToString("x2") })
        $parts += [ordered]@{ name = $name; size = $written; sha256 = $hex }
        Write-Host ("  {0}  {1:N0} bytes  {2}" -f $name, $written, $hex)
        $index++
    }
} finally { $in.Close() }
[void]$whole.TransformFinalBlock($buf, 0, 0)
$wholeHex = -join ($whole.Hash | ForEach-Object { $_.ToString("x2") })
$manifest = [ordered]@{ version = 1; file = $src.Name; size = $src.Length; sha256 = $wholeHex; parts = $parts }
$manifestPath = Join-Path $OutDir ($src.Name + ".parts.json")
[IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json -Depth 5), (New-Object Text.UTF8Encoding $false))
Write-Host "Wrote $($parts.Count) parts and $manifestPath (whole sha256 $wholeHex)"
