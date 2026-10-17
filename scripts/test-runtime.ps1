param(
    [Parameter(Mandatory)][string]$Directory,
    [switch]$CliOnly,
    [ValidateRange(1, 120)][int]$IdleSeconds = 5,
    [switch]$Record,
    [string]$OutputDirectory
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/runtime.ps1"
$directoryPath = (Resolve-Path -LiteralPath $Directory).Path
foreach ($name in (@('opencap.exe', 'opencap-bench.exe', 'opencap-transcribe.exe') + @(Get-OpenCapRuntimeNames))) {
    if (!(Test-Path -LiteralPath (Join-Path $directoryPath $name) -PathType Leaf)) {
        throw "Incomplete runtime directory: missing $name in $directoryPath"
    }
}
if (!$OutputDirectory) {
    $root = Split-Path -Parent $PSScriptRoot
    $OutputDirectory = Join-Path $root "target/verify/runtime-check-$(Get-Date -Format yyyyMMdd-HHmmss-fff)"
}
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
$outputPath = (Resolve-Path -LiteralPath $OutputDirectory).Path
if ($directoryPath -eq $outputPath) { throw 'Test output must be separate from the application directory.' }

if (!('OpenCap.RuntimeTestNative' -as [type])) {
    Add-Type -TypeDefinition @'
using System.Runtime.InteropServices;
namespace OpenCap {
    public static class RuntimeTestNative {
        [DllImport("kernel32.dll")]
        public static extern uint SetErrorMode(uint mode);
    }
}
'@
}

function Start-CleanProcess {
    param([string]$Name, [string[]]$Arguments = @())
    $info = [Diagnostics.ProcessStartInfo]::new((Join-Path $directoryPath $Name))
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    # Test executable-relative loading from an unrelated working directory, without dev PATH.
    $info.WorkingDirectory = $outputPath
    $info.Environment['PATH'] = "$env:SystemRoot\System32;$env:SystemRoot"
    [void]$info.Environment.Remove('FFMPEG_DIR')
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    [Diagnostics.Process]::Start($info)
}

function Invoke-CleanBench {
    param([string]$Label, [string[]]$Arguments, [int]$TimeoutSeconds = 20, [string]$Executable = 'opencap-bench.exe')
    $process = Start-CleanProcess $Executable $Arguments
    try {
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if ($Label -eq 'runtime-fixture') {
            Start-Sleep -Milliseconds 150
            $process.Refresh()
            if ($process.HasExited) { throw 'Runtime fixture exited before DLL inspection.' }
            $expected = @(Get-OpenCapFfmpegRuntimeNames) + @('vcruntime140.dll')
            $modules = @($process.Modules | Where-Object { $_.ModuleName -in $expected })
            if ($modules.Count -ne $expected.Count) { throw 'The codec/VC runtime modules were not loaded.' }
            foreach ($module in $modules) {
                if ((Split-Path -Parent $module.FileName) -ne $directoryPath) {
                    throw "Runtime loaded outside application directory: $($module.FileName)"
                }
            }
            $report.runtime_modules = @($modules | ForEach-Object { $_.FileName })
        }
        if (!$process.WaitForExit($TimeoutSeconds * 1000)) { throw "$Label timed out." }
        $stdout.GetAwaiter().GetResult() | Set-Content (Join-Path $outputPath "$Label.stdout.txt") -Encoding utf8
        $errorText = $stderr.GetAwaiter().GetResult()
        $errorText | Set-Content (Join-Path $outputPath "$Label.stderr.txt") -Encoding utf8
        if ($process.ExitCode -ne 0) { throw "$Label failed with exit code $($process.ExitCode): $errorText" }
        Write-Host "$Label passed (clean PATH)."
    } finally {
        if (!$process.HasExited) { $process.Kill($true); $process.WaitForExit() }
        $process.Dispose()
    }
}

$previousMode = [OpenCap.RuntimeTestNative]::SetErrorMode(0x8001)
$samples = [Collections.Generic.List[object]]::new()
$report = [ordered]@{ directory = $directoryPath; clean_path = $true; cli_passed = $false; gui_passed = $null; recording_passed = $null; runtime_modules = @(); passed = $false }
try {
    Invoke-CleanBench 'version' @('--version')
    Invoke-CleanBench 'transcribe-help' @('--help') -Executable 'opencap-transcribe.exe'
    Invoke-CleanBench 'runtime-fixture' @('fixture', '--seconds', '5', '--width', '160', '--height', '90', '--codec', 'h264', '--encoder', 'libx264', '--output', (Join-Path $outputPath ('fixture-' + [Guid]::NewGuid().ToString('N'))))
    $report.cli_passed = $true
    if (!$CliOnly) {
        $gui = Start-CleanProcess 'opencap.exe'
        try {
            $guiOut = $gui.StandardOutput.ReadToEndAsync()
            $guiErr = $gui.StandardError.ReadToEndAsync()
            $windowDeadline = [DateTime]::UtcNow.AddSeconds(15)
            do {
                Start-Sleep -Milliseconds 250
                $gui.Refresh()
                if ($gui.HasExited) { throw "GUI exited during startup: $($gui.ExitCode)" }
            } while ($gui.MainWindowHandle -eq 0 -and [DateTime]::UtcNow -lt $windowDeadline)
            if ($gui.MainWindowHandle -eq 0) { throw 'GUI did not create a main window.' }
            $modules = @($gui.Modules | Where-Object { $_.ModuleName -in @(Get-OpenCapRuntimeNames) })
            foreach ($module in $modules) {
                if ((Split-Path -Parent $module.FileName) -ne $directoryPath) {
                    throw "Runtime loaded outside application directory: $($module.FileName)"
                }
            }
            if ($modules.Count -ne @(Get-OpenCapRuntimeNames).Count) { throw 'Not all FFmpeg runtime modules were loaded.' }
            $report.runtime_modules = @($modules | ForEach-Object { $_.FileName })
            $timer = [Diagnostics.Stopwatch]::StartNew()
            $previousCpu = $gui.TotalProcessorTime.TotalSeconds
            $previousSeconds = 0.0
            while ($timer.Elapsed.TotalSeconds -lt $IdleSeconds) {
                Start-Sleep -Milliseconds ([Math]::Min(5000, [Math]::Max(1, ($IdleSeconds - $timer.Elapsed.TotalSeconds) * 1000)))
                $gui.Refresh()
                if ($gui.HasExited) { throw "GUI exited while idle: $($gui.ExitCode)" }
                if (!$gui.Responding) { throw 'GUI stopped responding.' }
                $seconds = $timer.Elapsed.TotalSeconds
                $cpu = $gui.TotalProcessorTime.TotalSeconds
                $sample = [ordered]@{
                    seconds = [Math]::Round($seconds, 2)
                    cpu_one_core_percent = [Math]::Round(100 * ($cpu - $previousCpu) / ($seconds - $previousSeconds), 2)
                    working_set_mb = [Math]::Round($gui.WorkingSet64 / 1MB, 2)
                    private_mb = [Math]::Round($gui.PrivateMemorySize64 / 1MB, 2)
                    handles = $gui.HandleCount
                }
                $samples.Add($sample)
                $sample | ConvertTo-Json -Compress | Write-Host
                if ($sample.working_set_mb -gt 800) { throw 'Idle working set exceeded 800 MB.' }
                if ($seconds -gt 10 -and $sample.cpu_one_core_percent -gt 60) { throw 'Unexpected sustained idle CPU load.' }
                $previousCpu = $cpu
                $previousSeconds = $seconds
            }
            $report.gui_passed = $true
            Write-Host 'GUI startup and idle check passed (clean PATH).'
        } finally {
            if (!$gui.HasExited) { $gui.Kill($true); $gui.WaitForExit() }
            $guiOut.GetAwaiter().GetResult() | Set-Content (Join-Path $outputPath 'gui.stdout.txt') -Encoding utf8
            $guiErr.GetAwaiter().GetResult() | Set-Content (Join-Path $outputPath 'gui.stderr.txt') -Encoding utf8
            $gui.Dispose()
        }
    }
    if ($Record) {
        $recordPath = Join-Path $outputPath 'recording'
        Invoke-CleanBench 'record' @('record', '--seconds', '3', '--region', '0,0,640,360', '--no-audio', '--codec', 'h264', '--encoder', 'libx264', '--output', $recordPath)
        # Reusing -OutputDirectory is allowed, so judge the newest file instead of requiring exactly one.
        $recordings = @(Get-ChildItem -LiteralPath $recordPath -Filter '*.mp4' |
            Where-Object { $_.Name -notlike '*.partial.mp4' } |
            Sort-Object LastWriteTime -Descending)
        if ($recordings.Count -lt 1 -or $recordings[0].Length -eq 0) { throw 'Recording did not produce a finalized MP4.' }
        Invoke-CleanBench 'decode' @('encode', '--input', $recordings[0].FullName, '--encoder', 'libx264', '--output', (Join-Path $outputPath 'decoded'))
        $report.recording_passed = $true
    }
    $report.passed = $true
} finally {
    [void][OpenCap.RuntimeTestNative]::SetErrorMode($previousMode)
    $report.samples = @($samples.ToArray())
    $report | ConvertTo-Json -Depth 6 | Set-Content (Join-Path $outputPath 'runtime-test.json') -Encoding utf8
}
Write-Host "Runtime verification passed: $outputPath"
