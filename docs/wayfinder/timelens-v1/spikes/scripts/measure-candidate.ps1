[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('slint', 'tauri', 'winui')]
    [string]$Candidate,

    [ValidateRange(10, 3600)]
    [int]$DurationSeconds = 30,

    [string]$OutputRoot
)

$ErrorActionPreference = 'Stop'
$spikeRoot = Split-Path -Parent $PSScriptRoot
if (-not $OutputRoot) {
    $OutputRoot = Join-Path $spikeRoot 'benchmark-output'
}

$rustRelease = Join-Path $spikeRoot 'release-target\release'
$driverPath = Join-Path $rustRelease 'timelens-spike-driver.exe'
$candidateConfig = @{
    slint = @{
        Title = 'Timelens Slint Spike'
        Executable = Join-Path $rustRelease 'timelens-slint-spike.exe'
    }
    tauri = @{
        Title = 'Timelens Tauri Spike'
        Executable = Join-Path $rustRelease 'timelens-tauri-spike.exe'
    }
    winui = @{
        Title = 'Timelens WinUI Spike'
        Executable = Join-Path $spikeRoot 'winui-spike\publish-minimal\TimelensWinUISpike.exe'
    }
}[$Candidate]

foreach ($requiredPath in @($driverPath, $candidateConfig.Executable)) {
    if (-not (Test-Path -LiteralPath $requiredPath)) {
        throw "Missing build artifact: $requiredPath. Run scripts\build.ps1 first."
    }
}

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$runDirectory = Join-Path $OutputRoot "$Candidate-$stamp-$([guid]::NewGuid().ToString('N').Substring(0, 8))"
New-Item -ItemType Directory -Path $runDirectory -Force | Out-Null

function Get-ProcessTreeIds {
    param([int[]]$Roots)

    $rows = Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId
    $known = [System.Collections.Generic.HashSet[int]]::new()
    foreach ($root in $Roots) {
        [void]$known.Add($root)
    }
    do {
        $added = $false
        foreach ($row in $rows) {
            if ($known.Contains([int]$row.ParentProcessId) -and $known.Add([int]$row.ProcessId)) {
                $added = $true
            }
        }
    } while ($added)
    return @($known)
}

function Get-ProcessSnapshot {
    param([int[]]$Ids)

    $processes = foreach ($id in $Ids) {
        Get-Process -Id $id -ErrorAction SilentlyContinue
    }
    $processes = @($processes)
    $cpuDelta = 0.0
    foreach ($process in $processes) {
        $currentCpu = [double]$process.CPU
        if ($script:timelensSpikeCpuById.ContainsKey($process.Id)) {
            $cpuDelta += [Math]::Max(0.0, $currentCpu - [double]$script:timelensSpikeCpuById[$process.Id])
        }
        $script:timelensSpikeCpuById[$process.Id] = $currentCpu
    }
    $script:timelensSpikeAccumulatedCpu = [double]$script:timelensSpikeAccumulatedCpu + $cpuDelta
    [pscustomobject]@{
        TimestampUtc = [datetime]::UtcNow.ToString('O')
        ProcessCount = $processes.Count
        CpuSeconds = [double](($processes | Measure-Object CPU -Sum).Sum)
        AccumulatedCpuSeconds = [double]$script:timelensSpikeAccumulatedCpu
        WorkingSetBytes = [int64](($processes | Measure-Object WorkingSet64 -Sum).Sum)
        PrivateBytes = [int64](($processes | Measure-Object PrivateMemorySize64 -Sum).Sum)
        ProcessIds = ($processes.Id -join ',')
    }
}

$driver = $null
$ui = $null
$samples = [System.Collections.Generic.List[object]]::new()
$uiStarted = $null
$firstWindowMilliseconds = $null
$script:timelensSpikeCpuById = @{}
$script:timelensSpikeAccumulatedCpu = 0.0
try {
    $driverArguments = @(
        '--out', ('"{0}"' -f $runDirectory),
        '--ui-title', ('"{0}"' -f $candidateConfig.Title),
        '--duration-seconds', $DurationSeconds
    ) -join ' '
    $driver = Start-Process -FilePath $driverPath -ArgumentList $driverArguments -PassThru -WindowStyle Hidden

    $readyPath = Join-Path $runDirectory 'ready'
    $readyDeadline = [datetime]::UtcNow.AddSeconds(30)
    while (-not (Test-Path -LiteralPath $readyPath)) {
        if ($driver.HasExited) {
            throw "Workload driver exited before becoming ready: $($driver.ExitCode)"
        }
        if ([datetime]::UtcNow -gt $readyDeadline) {
            throw 'Timed out waiting for workload driver readiness.'
        }
        Start-Sleep -Milliseconds 50
        $driver.Refresh()
    }

    $env:TIMELENS_SPIKE_TIMELINE = Join-Path $runDirectory 'timeline.json'
    $env:TIMELENS_SPIKE_OUTPUT = $runDirectory
    $uiStarted = [System.Diagnostics.Stopwatch]::StartNew()
    $ui = Start-Process -FilePath $candidateConfig.Executable -PassThru

    $windowDeadline = [datetime]::UtcNow.AddSeconds(20)
    while ([datetime]::UtcNow -lt $windowDeadline) {
        $ui.Refresh()
        if ($ui.MainWindowHandle -ne 0) {
            $firstWindowMilliseconds = $uiStarted.Elapsed.TotalMilliseconds
            break
        }
        if ($ui.HasExited) {
            throw "$Candidate UI exited before creating a window: $($ui.ExitCode)"
        }
        Start-Sleep -Milliseconds 20
    }

    do {
        $rootIds = @($driver.Id, $ui.Id)
        $treeIds = Get-ProcessTreeIds -Roots $rootIds
        $samples.Add((Get-ProcessSnapshot -Ids $treeIds))
        Start-Sleep -Milliseconds 500
        $driver.Refresh()
    } while (-not $driver.HasExited)

    $ui.Refresh()
    if (-not $ui.HasExited) {
        [void]$ui.CloseMainWindow()
        if (-not $ui.WaitForExit(5000)) {
            Stop-Process -Id $ui.Id
        }
    }
} finally {
    foreach ($process in @($ui, $driver)) {
        if ($process -and -not $process.HasExited) {
            Stop-Process -Id $process.Id -ErrorAction SilentlyContinue
        }
    }
}

$samplesPath = Join-Path $runDirectory 'process-samples.csv'
$samples | Export-Csv -LiteralPath $samplesPath -NoTypeInformation -Encoding utf8
$first = $samples | Select-Object -First 1
$last = $samples | Select-Object -Last 1
$elapsedSeconds = [Math]::Max(
    0.001,
    ([datetime]::Parse($last.TimestampUtc) - [datetime]::Parse($first.TimestampUtc)).TotalSeconds
)
$logicalProcessors = [Environment]::ProcessorCount
$panMetricsPath = Join-Path $runDirectory 'ui-pan.json'
$panMetrics = if (Test-Path -LiteralPath $panMetricsPath) {
    Get-Content -Raw -LiteralPath $panMetricsPath | ConvertFrom-Json
} else {
    $null
}
$steadyCpuPercent = $null
if ($panMetrics) {
    $steadyStart = (Get-Item -LiteralPath $panMetricsPath).LastWriteTimeUtc.AddSeconds(1)
    $steadyStartOffset = [datetimeoffset]$steadyStart
    $steadySamples = @($samples | Where-Object { [datetimeoffset]::Parse($_.TimestampUtc) -ge $steadyStartOffset })
    if ($steadySamples.Count -ge 2) {
        $steadyFirst = $steadySamples | Select-Object -First 1
        $steadyLast = $steadySamples | Select-Object -Last 1
        $steadyElapsed = [Math]::Max(
            0.001,
            ([datetime]::Parse($steadyLast.TimestampUtc) - [datetime]::Parse($steadyFirst.TimestampUtc)).TotalSeconds
        )
        $steadyCpuPercent = (($steadyLast.AccumulatedCpuSeconds - $steadyFirst.AccumulatedCpuSeconds) / $steadyElapsed / $logicalProcessors) * 100
    }
}
$summary = [ordered]@{
    candidate = $Candidate
    durationSeconds = $DurationSeconds
    sampleCount = $samples.Count
    firstWindowMilliseconds = $firstWindowMilliseconds
    averageCpuPercent = (($last.AccumulatedCpuSeconds - $first.AccumulatedCpuSeconds) / $elapsedSeconds / $logicalProcessors) * 100
    steadyCollectionCpuPercent = $steadyCpuPercent
    peakWorkingSetBytes = [int64](($samples | Measure-Object WorkingSetBytes -Maximum).Maximum)
    peakPrivateBytes = [int64](($samples | Measure-Object PrivateBytes -Maximum).Maximum)
    maximumProcessCount = [int](($samples | Measure-Object ProcessCount -Maximum).Maximum)
    pan = $panMetrics
    driverExitCode = $driver.ExitCode
    outputDirectory = $runDirectory
}
$summary | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $runDirectory 'summary.json') -Encoding utf8
$summary
