<#
Puts mpv.exe where the installer build picks it up: src-tauri/binaries/mpv/mpv.exe (gitignored).
Build with:  npm run tauri build -- --config src-tauri/tauri.mpv.conf.json

By default downloads a pinned build from https://github.com/zhongfly/mpv-winbuild (x86-64, not the -v3 variant,
so it runs on older CPUs too) and checks its SHA-256. To move to a newer mpv, update $Tag, $Asset and $Sha256 from
that release's sha256.txt.

  powershell -File tools/fetch-mpv.ps1                      # download the pinned build (the release workflow does this)
  powershell -File tools/fetch-mpv.ps1 -From C:\path\mpv.exe  # use an mpv.exe you already have
#>
param([string]$From = "")
$ErrorActionPreference = "Stop"

$Tag = "2026-11-20-d11ffba253"
$Asset = "mpv-x86_64-20261120-git-d11ffba253.7z"
$Sha256 = "45d1fda16ad15098e8193be381bcb34bd28fef1ae86148d5a7a9b3e54585560a"

$dest = Join-Path (Split-Path -Parent $PSScriptRoot) "src-tauri\binaries\mpv"
New-Item -ItemType Directory -Force $dest | Out-Null

if ($From) {
    Copy-Item $From (Join-Path $dest "mpv.exe") -Force
    Write-Host "copied $From -> $dest\mpv.exe"
    return
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("fetch-mpv-" + [guid]::NewGuid())
New-Item -ItemType Directory $tmp | Out-Null
try {
    $archive = Join-Path $tmp $Asset
    $ProgressPreference = "SilentlyContinue"
    Invoke-WebRequest "https://github.com/zhongfly/mpv-winbuild/releases/download/$Tag/$Asset" -OutFile $archive
    $hash = (Get-FileHash $archive -Algorithm SHA256).Hash.ToLower()
    if ($hash -ne $Sha256) { throw "SHA-256 mismatch for ${Asset}: got $hash" }

    $7z = (Get-Command 7z -ErrorAction SilentlyContinue).Source
    if (-not $7z) { $7z = "C:\Program Files\7-Zip\7z.exe" }
    if (-not (Test-Path $7z)) { throw "7-Zip not found (needed to unpack $Asset)" }
    & $7z e $archive "-o$tmp\x" mpv.exe -r -y | Out-Null
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path "$tmp\x\mpv.exe")) { throw "mpv.exe not found in $Asset" }
    Copy-Item "$tmp\x\mpv.exe" (Join-Path $dest "mpv.exe") -Force
    Write-Host "mpv $Tag -> $dest\mpv.exe"
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
