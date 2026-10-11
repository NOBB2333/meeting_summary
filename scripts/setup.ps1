$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$version = '9.0.2'
$archive = Join-Path $root '.deps/ffmpeg-shared.7z'
$destination = Join-Path $root '.deps/ffmpeg'
$uri = "https://www.gyan.dev/ffmpeg/builds/packages/ffmpeg-$version-full_build-shared.7z"
$expectedHash = '4D2060A8B34A940AA47D785142055BB92A63053781E55F2ACE4546EDD519A8F5'
$ffmpegRoot = Join-Path $destination "ffmpeg-$version-full_build-shared"
. "$PSScriptRoot/runtime.ps1"
$requiredFiles = @('include/libavcodec/avcodec.h', 'lib/avcodec.lib', 'bin/ffmpeg.exe') + @(Get-OpenCapRuntimeNames | ForEach-Object { "bin/$_" })
$missing = @($requiredFiles | Where-Object { !(Test-Path -LiteralPath (Join-Path $ffmpegRoot $_) -PathType Leaf) })
New-Item -ItemType Directory -Force (Split-Path $archive) | Out-Null
if ($missing.Count -gt 0) {
    if (!(Get-Command 7z -ErrorAction SilentlyContinue)) { throw '7-Zip is required: scoop install 7zip' }
    $validArchive = (Test-Path -LiteralPath $archive) -and (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash -eq $expectedHash
    if (!$validArchive) {
    if (Get-Command aria2c -ErrorAction SilentlyContinue) {
        & aria2c -c -x 8 -s 8 -d (Split-Path $archive) -o (Split-Path $archive -Leaf) $uri
        if ($LASTEXITCODE -ne 0) { throw 'FFmpeg download failed.' }
    } else { Invoke-WebRequest -Uri $uri -OutFile $archive }
    }
    if ((Get-FileHash -Algorithm SHA256 $archive).Hash -ne $expectedHash) { throw 'FFmpeg archive checksum mismatch.' }
    & 7z x $archive "-o$destination" -y
    if ($LASTEXITCODE -ne 0) { throw 'FFmpeg extraction failed.' }
}
foreach ($file in $requiredFiles) {
    if (!(Test-Path -LiteralPath (Join-Path $ffmpegRoot $file) -PathType Leaf)) { throw "FFmpeg installation is incomplete: $file" }
}
. "$PSScriptRoot/toolchain.ps1"
if (!$env:GPUI_FXC_PATH) {
    Write-Warning 'Windows SDK fxc.exe was not found. Install the Windows 10/11 SDK shader tools before a GPUI release build.'
}
Write-Host 'FFmpeg development libraries ready. Build: ./scripts/dev.ps1 build'
