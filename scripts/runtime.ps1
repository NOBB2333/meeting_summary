function Get-OpenCapRuntimeNames {
    # Include swresample, which avcodec imports even when the app does not use it directly.
    @('avcodec-63.dll', 'avformat-63.dll', 'avutil-61.dll', 'swresample-7.dll', 'swscale-10.dll')
}

function Copy-OpenCapRuntime {
    param([Parameter(Mandatory)][string]$Destination)
    $ffmpegBin = Join-Path (Split-Path -Parent $PSScriptRoot) '.deps/ffmpeg/ffmpeg-9.0.2-full_build-shared/bin'
    foreach ($name in Get-OpenCapRuntimeNames) {
        if (!(Test-Path -LiteralPath (Join-Path $ffmpegBin $name) -PathType Leaf)) {
            throw "Missing FFmpeg runtime: $name. Run scripts/setup.ps1 first."
        }
    }
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    foreach ($name in Get-OpenCapRuntimeNames) {
        $source = Join-Path $ffmpegBin $name
        $target = Join-Path $Destination $name
        if ((Test-Path -LiteralPath $target) -and
            (Get-FileHash -Algorithm SHA256 -LiteralPath $source).Hash -eq (Get-FileHash -Algorithm SHA256 -LiteralPath $target).Hash) { continue }
        Copy-Item -LiteralPath $source -Destination $Destination -Force
    }
}
