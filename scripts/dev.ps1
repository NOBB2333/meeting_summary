param([ValidateSet('build','run','test','check','lint','bench')][string]$Task = 'run', [switch]$Release, [Parameter(ValueFromRemainingArguments=$true)][string[]]$ExtraArgs)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$ffmpegRoot = Join-Path $root '.deps/ffmpeg/ffmpeg-9.0.2-full_build-shared'
. "$PSScriptRoot/runtime.ps1"
$requiredFiles = @('include/libavcodec/avcodec.h', 'lib/avcodec.lib', 'include/libavfilter/avfilter.h', 'lib/avfilter.lib') + @(Get-OpenCapFfmpegRuntimeNames | ForEach-Object { "bin/$_" })
if (@($requiredFiles | Where-Object { !(Test-Path -LiteralPath (Join-Path $ffmpegRoot $_) -PathType Leaf) }).Count -gt 0) {
    Write-Host 'Preparing the pinned FFmpeg development and runtime libraries...'
    & "$PSScriptRoot/setup.ps1"
}
$env:PATH = "$ffmpegRoot/bin;$env:PATH"
$env:FFMPEG_DIR = $ffmpegRoot
. "$PSScriptRoot/toolchain.ps1"
if ($Release -and (!$env:GPUI_FXC_PATH -or !(Test-Path $env:GPUI_FXC_PATH))) { throw 'GPUI release builds require fxc.exe from the Windows SDK. Set GPUI_FXC_PATH if installed in a custom location.' }
$arguments = switch ($Task) {
    'run' { @('run','-p','opencap','--features','dev-profile') }
    'bench' { @('run','-p','opencap-bench','--features','dev-profile') }
    'build' { @('build','--workspace') }
    'test' { @('test','--workspace') }
    'check' { @('check','--workspace') }
    'lint' { @('clippy','--workspace','--all-targets','--all-features') }
}
if ($Task -in @('run', 'bench')) { $env:CARGO_TARGET_DIR = Join-Path $root 'target/opencap-dev' }
if ($Task -in @('run', 'bench')) {
    # The isolated target may not contain FFmpeg DLLs on its first launch.
    # Build and install the runtime before starting the process, rather than
    # copying it only after `cargo run` has already returned.
    $package = if ($Task -eq 'run') { 'opencap' } else { 'opencap-bench' }
    $buildArgs = @('build', '-p', $package, '--features', 'dev-profile', '--locked')
    if ($Release) { $buildArgs += '--release' }
    & cargo @buildArgs
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    $profile = if ($Release) { 'release' } else { 'debug' }
    $output = Join-Path $env:CARGO_TARGET_DIR $profile
    Copy-OpenCapRuntime -Destination $output
    & (Join-Path $output "$package.exe") @ExtraArgs
    exit $LASTEXITCODE
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
    $targetRoot = if ($Task -eq 'run') { 'target/opencap-dev' } else { 'target' }
    Copy-OpenCapRuntime -Destination (Join-Path $root "$targetRoot/$profile")
}
exit 0
