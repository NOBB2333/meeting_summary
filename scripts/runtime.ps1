function Get-OpenCapFfmpegRuntimeNames {
    # Include swresample, which avcodec imports even when the app does not use it directly.
    @('avcodec-63.dll', 'avformat-63.dll', 'avutil-61.dll', 'swresample-7.dll', 'swscale-10.dll')
}

function Get-OpenCapRuntimeNames {
    @(Get-OpenCapFfmpegRuntimeNames) + @('vcruntime140.dll', 'vcruntime140_1.dll')
}

function Get-OpenCapVcRuntimeDirectory {
    $candidates = @()
    if ($env:OPENCAP_VC_RUNTIME_DIR) {
        $candidates += $env:OPENCAP_VC_RUNTIME_DIR
    } else {
        if ($env:VCToolsRedistDir) {
            $candidates += @(Get-ChildItem "$env:VCToolsRedistDir/x64/Microsoft.VC*.CRT" -Directory -ErrorAction SilentlyContinue | ForEach-Object FullName)
        }
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
        if (Test-Path -LiteralPath $vswhere) {
            $install = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
            if ($install) {
                $candidates += @(Get-ChildItem "$install/VC/Redist/MSVC/*/x64/Microsoft.VC*.CRT" -Directory -ErrorAction SilentlyContinue | ForEach-Object FullName)
            }
        }
    }
    $available = @($candidates | Where-Object {
        (Test-Path -LiteralPath (Join-Path $_ 'vcruntime140.dll')) -and
        (Test-Path -LiteralPath (Join-Path $_ 'vcruntime140_1.dll'))
    } | Sort-Object -Descending | Select-Object -Unique)
    if (!$available) {
        throw 'Missing x64 Visual C++ redistributable DLLs. Install the MSVC C++ tools/redist component, or set OPENCAP_VC_RUNTIME_DIR to its x64 Microsoft.VC*.CRT directory.'
    }
    $available[0]
}

function Copy-OpenCapRuntime {
    param([Parameter(Mandatory)][string]$Destination)
    $ffmpegBin = Join-Path (Split-Path -Parent $PSScriptRoot) '.deps/ffmpeg/ffmpeg-9.0.2-full_build-shared/bin'
    foreach ($name in Get-OpenCapFfmpegRuntimeNames) {
        if (!(Test-Path -LiteralPath (Join-Path $ffmpegBin $name) -PathType Leaf)) {
            throw "Missing FFmpeg runtime: $name. Run scripts/setup.ps1 first."
        }
    }
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    foreach ($name in Get-OpenCapFfmpegRuntimeNames) {
        $source = Join-Path $ffmpegBin $name
        $target = Join-Path $Destination $name
        if ((Test-Path -LiteralPath $target) -and
            (Get-FileHash -Algorithm SHA256 -LiteralPath $source).Hash -eq (Get-FileHash -Algorithm SHA256 -LiteralPath $target).Hash) { continue }
        Copy-Item -LiteralPath $source -Destination $Destination -Force
    }
    $vc = Get-OpenCapVcRuntimeDirectory
    foreach ($name in @('vcruntime140.dll', 'vcruntime140_1.dll')) {
        $source = Join-Path $vc $name
        $stream = [IO.File]::OpenRead($source)
        $reader = [IO.BinaryReader]::new($stream)
        try {
            $stream.Position = 0x3c
            $offset = $reader.ReadInt32()
            $stream.Position = $offset
            if ($reader.ReadUInt32() -ne 0x4550 -or $reader.ReadUInt16() -ne 0x8664) {
                throw "VC runtime is not an x64 PE file: $source"
            }
        } finally { $reader.Dispose(); $stream.Dispose() }
        $target = Join-Path $Destination $name
        if ((Test-Path -LiteralPath $target) -and
            (Get-FileHash -Algorithm SHA256 -LiteralPath $source).Hash -eq (Get-FileHash -Algorithm SHA256 -LiteralPath $target).Hash) { continue }
        Copy-Item -LiteralPath $source -Destination $Destination -Force
    }
}
