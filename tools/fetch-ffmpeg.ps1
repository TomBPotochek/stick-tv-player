# Downloads a static ffmpeg (gyan.dev "essentials" release build: GPL, includes libx264) and puts it where Tauri
# expects the sidecar: src-tauri/binaries/ffmpeg-x86_64-pc-windows-msvc.exe. Then build with
#   npm run tauri build -- --config src-tauri/tauri.ffmpeg.conf.json
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $root "src-tauri\binaries"
New-Item -ItemType Directory -Force $dest | Out-Null
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("ffmpeg-" + [guid]::NewGuid())
New-Item -ItemType Directory $tmp | Out-Null
try {
    $zip = Join-Path $tmp "ffmpeg.zip"
    Invoke-WebRequest "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip" -OutFile $zip
    Expand-Archive $zip -DestinationPath $tmp
    $exe = Get-ChildItem $tmp -Recurse -Filter ffmpeg.exe | Select-Object -First 1
    Copy-Item $exe.FullName (Join-Path $dest "ffmpeg-x86_64-pc-windows-msvc.exe") -Force
    Write-Host "ffmpeg copied to $dest"
} finally {
    Remove-Item -Recurse -Force $tmp
}
