param([int]$Seconds = 300, [string[]]$Encoders = @('h264_qsv','hevc_qsv','libx264'), [switch]$Release, [switch]$Fast)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$env:PATH = "$(Join-Path $root '.deps/ffmpeg/ffmpeg-9.0.2-full_build-shared/bin');$env:PATH"
$profile = if ($Release) { 'release' } else { 'debug' }
$exe = Join-Path $root "target/$profile/opencap-bench.exe"
if (!(Test-Path $exe)) { throw 'Build first with scripts/dev.ps1 build [-Release].' }
$runDir = Join-Path $root "target/verify/benchmark-$(Get-Date -Format yyyyMMdd-HHmmss)"
New-Item -ItemType Directory -Force $runDir | Out-Null
& $exe probe --output "$runDir/capabilities.json"
if ($LASTEXITCODE -ne 0) { throw 'Encoder probe failed.' }
$capabilities = Get-Content "$runDir/capabilities.json" -Raw | ConvertFrom-Json
foreach ($encoder in $Encoders) {
    if (!($capabilities | Where-Object { $_.name -eq $encoder -and $_.available })) { Write-Warning "Skipping unavailable encoder: $encoder"; continue }
    foreach ($scene in @('ppt','document','ide','scroll','motion')) {
        foreach ($adaptive in @($true,$false)) {
            $arguments = @('fixture','--scene',$scene,'--seconds',"$Seconds",'--encoder',$encoder,'--output',$runDir)
            if ($Fast) { $arguments += '--fast' }
            if (!$adaptive) { $arguments += @('--fixed-fps','--fps','15') }
            & $exe @arguments
            if ($LASTEXITCODE -ne 0) { throw "Benchmark failed: $scene / $encoder" }
        }
    }
}
$rows = Get-ChildItem $runDir -Filter '*.benchmark.json' | ForEach-Object {
    $r = Get-Content $_.FullName -Raw | ConvertFrom-Json
    [PSCustomObject]@{ Scene=$r.scene; Encoder=$r.encoder; Adaptive=$r.config.adaptive; Seconds=$r.duration_seconds; MB=$r.bytes/1e6; MBPerHour=$r.mb_per_hour_extrapolated; FPS=$r.actual_fps; CPU=$r.cpu_peak_percent; GPU=$r.gpu_percent; TextReview='pending' }
}
$rows | Export-Csv "$runDir/summary.csv" -NoTypeInformation -Encoding utf8
Write-Host "Results: $runDir"
