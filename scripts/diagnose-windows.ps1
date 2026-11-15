param(
    [ValidateSet('closed','idle','recording','no-asr','no-preview','no-asr-preview','audio-only','minimized')][string]$Scenario = 'idle',
    [ValidateRange(5,1800)][int]$Seconds = 660,
    [string]$Output = '',
    [string]$RecordingReport = ''
)
$ErrorActionPreference = 'Stop'
if (-not $Output) {
    $runId = [Guid]::NewGuid().ToString('N')
    $Output = Join-Path (Split-Path -Parent $PSScriptRoot) "target/verify/windows-performance/$Scenario-$runId.json"
}
if (Test-Path -LiteralPath $Output) { throw "输出路径已存在，不会覆盖：$Output" }
# Observe only. Change the scenario manually in the app before starting this script.
# CPU is normalized to all logical processors, matching the app's meter.
$cores = [Environment]::ProcessorCount
$previous = @{}
$samples = [System.Collections.Generic.List[object]]::new()
$watch = [Diagnostics.Stopwatch]::StartNew()
while ($watch.Elapsed.TotalSeconds -lt $Seconds) {
    $timestamp = $watch.Elapsed.TotalSeconds
    $processes = @(Get-Process -Name opencap,dwm -ErrorAction SilentlyContinue)
    $processRows = @($processes | ForEach-Object {
        $process = $_
        $cpuSeconds = $process.TotalProcessorTime.TotalSeconds
        $cpu = $null
        if ($previous.ContainsKey($process.Id)) {
            $last = $previous[$process.Id]
            $interval = $timestamp - $last.Time
            # A PID can be reused while sampling; never report a negative CPU
            # delta from a different process as a measurement.
            if ($interval -gt 0 -and $cpuSeconds -ge $last.Cpu) { $cpu = [Math]::Round(100 * ($cpuSeconds - $last.Cpu) / $interval / $cores, 2) }
        }
        $previous[$process.Id] = @{ Time = $timestamp; Cpu = $cpuSeconds }
        [pscustomobject]@{
            Name = $process.ProcessName; Pid = $process.Id; CpuPercent = $cpu
            WorkingSetMB = [Math]::Round($process.WorkingSet64 / 1MB, 1)
            PrivateMB = [Math]::Round($process.PrivateMemorySize64 / 1MB, 1)
            Threads = $process.Threads.Count
        }
    })
    $gpuRows = @()
    $gpuError = $null
    try {
        # CIM counter class names do not depend on the Windows display language.
        $gpuRows = @(Get-CimInstance Win32_PerfFormattedData_GPUPerformanceCounters_GPUEngine -ErrorAction Stop |
            Where-Object { $_.UtilizationPercentage -gt 0 } |
            Select-Object Name,UtilizationPercentage)
    } catch { $gpuError = $_.Exception.Message }
    $samples.Add([pscustomobject]@{ AtSeconds = [Math]::Round($timestamp, 2); Processes = $processRows; GpuEngines = $gpuRows; GpuCounterError = $gpuError })
    Start-Sleep -Seconds 2
}
$report = $null
if ($RecordingReport) {
    $saved = Get-Content -LiteralPath $RecordingReport -Raw | ConvertFrom-Json
    $report = [pscustomobject]@{
        Encoder = $saved.stats.encoder; Width = $saved.encoder_details.width; Height = $saved.encoder_details.height
        FrameRateValid = $saved.frame_rate_valid
        Fps = if ($saved.frame_rate_valid -eq $true) { $saved.stats.fps } else { $null }
        BitrateValid = $saved.bitrate_valid
        BitrateKbps = if ($saved.bitrate_valid -eq $true) { $saved.stats.bitrate_kbps } else { $null }
        DroppedFrames = $saved.stats.dropped_frames
        RealtimeTranscription = $saved.config.realtime_transcription
        ContextRevision = $saved.config.transcription_context_revision
        RecordingPreview = $saved.config.show_recording_preview
        FastSoftwareEncoding = $saved.config.fast_software_encoding
        MaxFps = $saved.config.max_fps
        AudioOnly = $saved.config.audio_only
        PerformanceSamples = $saved.performance_samples
        PerformanceSamplesDiscarded = $saved.performance_samples_discarded
    }
}
$platformInfo = [pscustomobject]@{
    Os = [Environment]::OSVersion.VersionString
    Cpu = @(Get-CimInstance Win32_Processor -ErrorAction SilentlyContinue | Select-Object Name,NumberOfLogicalProcessors)
    Gpu = @(Get-CimInstance Win32_VideoController -ErrorAction SilentlyContinue | Select-Object Name,DriverVersion)
}
$data = [pscustomobject]@{
    Platform = $platformInfo
    Scenario = $Scenario; LogicalProcessors = $cores; CpuDefinition = 'percent of all logical processors'
    GpuDefinition = 'raw nonzero per-engine counters; do not sum as whole-GPU percent'
    Recording = $report; Samples = $samples
}
$directory = Split-Path -Parent ([IO.Path]::GetFullPath($Output))
[IO.Directory]::CreateDirectory($directory) | Out-Null
$json = $data | ConvertTo-Json -Depth 8
$stream = [IO.File]::Open($Output, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
try {
    $writer = [IO.StreamWriter]::new($stream, [Text.UTF8Encoding]::new($false))
    try { $writer.Write($json); $writer.Flush() } finally { $writer.Dispose() }
} finally { $stream.Dispose() }
Write-Host "Saved local performance report: $Output"
