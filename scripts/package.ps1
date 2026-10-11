param(
    [switch]$Release,
    [string]$CorrespondingSourceDir,
    [switch]$LocalPreview,
    [string]$BuildDirectory,
    [string]$OutputDirectory
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$profile = if ($Release) { 'release' } else { 'debug' }
$buildDirectory = if ($BuildDirectory) {
    (Resolve-Path -LiteralPath $BuildDirectory -ErrorAction Stop).Path
} else {
    Join-Path $root "target/$profile"
}
$builtExe = Join-Path $buildDirectory 'opencap.exe'
if (!(Test-Path -LiteralPath $builtExe)) {
    $hint = if ($Release) { './scripts/dev.ps1 build -Release' } else { './scripts/dev.ps1 build' }
    throw "找不到构建产物：$builtExe`n请先执行：$hint"
}
$releaseDirectory = if ($OutputDirectory) {
    [System.IO.Path]::GetFullPath($OutputDirectory)
} else {
    Join-Path $root 'release'
}
$ffmpegRoot = Join-Path $root '.deps/ffmpeg/ffmpeg-9.0.2-full_build-shared'
if (!(Test-Path -LiteralPath (Join-Path $ffmpegRoot 'bin/ffmpeg.exe') -PathType Leaf)) {
    Write-Host '正在准备固定版本的 FFmpeg 运行库...'
    & "$PSScriptRoot/setup.ps1"
}
$buildconf = & "$ffmpegRoot/bin/ffmpeg.exe" -hide_banner -buildconf 2>&1 | Out-String
if ($buildconf.Contains('--enable-nonfree')) { throw 'Nonfree FFmpeg builds are not distributable by this project.' }

# 不提供对应源码时，打出来的是可运行、可转发的本地测试包。日常迭代就是这种情况，
# 所以默认走测试包，而不是直接报错。
$preview = $LocalPreview -or [string]::IsNullOrWhiteSpace($CorrespondingSourceDir)
if ($preview) {
    Write-Host '正在生成可直接运行和转发的本地测试包。' -ForegroundColor Yellow
    Write-Host '正式公开发布请加 -CorrespondingSourceDir，附带匹配的 FFmpeg 与依赖源码。' -ForegroundColor Yellow
} elseif (!(Test-Path -LiteralPath $CorrespondingSourceDir -PathType Container)) {
    throw "对应源码目录不存在：$CorrespondingSourceDir"
}

$stamp = Get-Date -Format yyyyMMdd-HHmmss
$stageRoot = Join-Path $releaseDirectory ".staging-$stamp"
$stage = Join-Path $stageRoot 'OpenCap'
$final = Join-Path $releaseDirectory 'OpenCap'
$archive = Join-Path $releaseDirectory 'OpenCap-windows-x64.zip'
New-Item -ItemType Directory -Force $stage | Out-Null
foreach ($exe in @('opencap.exe', 'opencap-bench.exe')) {
    $source = Join-Path $buildDirectory $exe
    if (!(Test-Path -LiteralPath $source)) { throw "缺少构建产物：$source" }
    Copy-Item -LiteralPath $source -Destination $stage
}
. "$PSScriptRoot/runtime.ps1"
Copy-OpenCapRuntime -Destination $stage
Copy-Item README.md, LICENSE, THIRD_PARTY.md $stage
Copy-Item LICENSES "$stage/LICENSES" -Recurse
Copy-Item docs "$stage/docs" -Recurse
$buildconf | Set-Content "$stage/ffmpeg-buildconf.txt" -Encoding utf8
& "$ffmpegRoot/bin/ffmpeg.exe" -version | Set-Content "$stage/ffmpeg-version.txt" -Encoding utf8
Copy-Item "$ffmpegRoot/LICENSE" "$stage/LICENSES/FFmpeg-build.txt"
if ($preview) {
    $notice = @(
        'OpenCap 测试包（可直接运行、可转发）',
        '',
        '请保持整个 OpenCap 文件夹完整：opencap.exe 会加载同目录下的 FFmpeg DLL。',
        '双击 opencap.exe 即可运行，不需要单独安装 FFmpeg。',
        '',
        '此测试包不包含所附带 FFmpeg 构建的完整对应源码。',
        '正式公开发布必须附带匹配的 FFmpeg 与依赖源码，详见 THIRD_PARTY.md。'
    ) -join [Environment]::NewLine
    Set-Content "$stage/SHARE-TEST-PACKAGE.txt" -Value $notice -Encoding utf8
} else {
    Copy-Item -LiteralPath (Resolve-Path -LiteralPath $CorrespondingSourceDir).Path -Destination "$stage/corresponding-source" -Recurse
    & cargo vendor --locked "$stage/rust-vendor" | Set-Content "$stage/vendor-config.toml" -Encoding utf8
    if ($LASTEXITCODE -ne 0) { throw 'cargo vendor failed' }
    New-Item -ItemType Directory -Force "$stage/source" | Out-Null
    Copy-Item Cargo.toml, Cargo.lock, rust-toolchain.toml, LICENSE, README.md, THIRD_PARTY.md "$stage/source"
    foreach ($dir in @('crates', 'scripts', 'docs', '.cargo', 'LICENSES')) {
        Copy-Item $dir "$stage/source/$dir" -Recurse
    }
}
# 拒绝只有在开发者 PATH 下才能跑的包。
& "$PSScriptRoot/test-runtime.ps1" -Directory $stage -CliOnly
Get-ChildItem $stage -File | Get-FileHash -Algorithm SHA256 |
    Select-Object @{Name = 'File'; Expression = { Split-Path $_.Path -Leaf } }, Hash |
    ConvertTo-Json | Set-Content "$stage/checksums.json" -Encoding utf8

# 重新打包是常规操作，直接替换上一次的产物，不再报错。
if (Test-Path -LiteralPath $final) { Remove-Item -LiteralPath $final -Recurse -Force }
if (Test-Path -LiteralPath $archive) { Remove-Item -LiteralPath $archive -Force }
Move-Item -LiteralPath $stage -Destination $final
Remove-Item -LiteralPath $stageRoot -Recurse -Force
Compress-Archive -Path $final -DestinationPath $archive
Write-Host ''
Write-Host "便携目录：$final"
Write-Host "可转发 ZIP：$archive"
Write-Host '解压 ZIP 后双击 OpenCap\opencap.exe 运行。'