param([ValidateSet('build','run','test','check','lint','bench')][string]$Task = 'run', [switch]$Release, [Parameter(ValueFromRemainingArguments=$true)][string[]]$ExtraArgs)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$ffmpegRoot = Join-Path $root '.deps/ffmpeg/ffmpeg-9.0.2-full_build-shared'
. "$PSScriptRoot/runtime.ps1"
$requiredFiles = @('include/libavcodec/avcodec.h', 'lib/avcodec.lib') + @(Get-OpenCapFfmpegRuntimeNames | ForEach-Object { "bin/$_" })
if (@($requiredFiles | Where-Object { !(Test-Path -LiteralPath (Join-Path $ffmpegRoot $_) -PathType Leaf) }).Count -gt 0) {
    Write-Host 'Preparing the pinned FFmpeg development and runtime libraries...'
    & "$PSScriptRoot/setup.ps1"
}
$env:PATH = "$ffmpegRoot/bin;$env:PATH"
$env:FFMPEG_DIR = $ffmpegRoot
. "$PSScriptRoot/toolchain.ps1"
if ($Release -and (!$env:GPUI_FXC_PATH -or !(Test-Path $env:GPUI_FXC_PATH))) { throw 'GPUI release builds require fxc.exe from the Windows SDK. Set GPUI_FXC_PATH if installed in a custom location.' }
$arguments = switch ($Task) {
    'run' { @('run','-p','opencap') }
    'bench' { @('run','-p','opencap-bench') }
    'build' { @('build','--workspace') }
    'test' { @('test','--workspace') }
    'check' { @('check','--workspace') }
    'lint' { @('clippy','--workspace','--all-targets','--all-features') }
}
if ($Release) { $arguments += '--release' }
$arguments += '--locked'
# PowerShell consumes a literal -- while binding a script's remaining arguments.
# Restore the rustc/Clippy separator when the remainder starts with lint flags.
if ($Task -eq 'lint' -and $ExtraArgs -and $ExtraArgs[0] -match '^(-[DAWF]|--(deny|allow|warn|forbid))') {
    $ExtraArgs = @('--') + $ExtraArgs
}
if ($Task -in @('run','bench') -and $ExtraArgs) { $arguments += '--' }
& cargo @arguments @ExtraArgs
$buildExitCode = $LASTEXITCODE
if ($buildExitCode -ne 0) { exit $buildExitCode }
if ($Task -in @('build', 'run', 'bench')) {
    . "$PSScriptRoot/runtime.ps1"
    $profile = if ($Release) { 'release' } else { 'debug' }
    Copy-OpenCapRuntime -Destination (Join-Path $root "target/$profile")
}
exit 0
