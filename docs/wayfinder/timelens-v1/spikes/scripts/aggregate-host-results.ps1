[CmdletBinding()]
param(
    [ValidateRange(10, 3600)]
    [int]$DurationSeconds = 30,

    [ValidatePattern('^[a-z0-9-]+$')]
    [string]$OutputPrefix = 'host-preliminary'
)

$ErrorActionPreference = 'Stop'
$spikeRoot = Split-Path -Parent $PSScriptRoot
$outputRoot = Join-Path $spikeRoot 'benchmark-output'
$logicalProcessors = [Environment]::ProcessorCount

function Get-CpuPercent {
    param([object[]]$Samples)

    if ($Samples.Count -lt 2) {
        return $null
    }
    $first = $Samples | Select-Object -First 1
    $last = $Samples | Select-Object -Last 1
    $elapsed = [Math]::Max(
        0.001,
        ([datetimeoffset]::Parse($last.TimestampUtc) - [datetimeoffset]::Parse($first.TimestampUtc)).TotalSeconds
    )
    if ($first.PSObject.Properties.Name -contains 'AccumulatedCpuSeconds') {
        return (([double]$last.AccumulatedCpuSeconds - [double]$first.AccumulatedCpuSeconds) / $elapsed / $logicalProcessors) * 100
    }
    return $null
}

function Get-Statistics {
    param([double[]]$Values)

    $ordered = @($Values | Sort-Object)
    if ($ordered.Count -eq 0) {
        return $null
    }
    $middle = [int][Math]::Floor($ordered.Count / 2)
    $median = if ($ordered.Count % 2 -eq 0) {
        ($ordered[$middle - 1] + $ordered[$middle]) / 2
    } else {
        $ordered[$middle]
    }
    [ordered]@{
        median = $median
        minimum = $ordered[0]
        maximum = $ordered[-1]
    }
}

$eligible = foreach ($directory in Get-ChildItem $outputRoot -Directory) {
    $summaryPath = Join-Path $directory.FullName 'summary.json'
    $samplesPath = Join-Path $directory.FullName 'process-samples.csv'
    $panPath = Join-Path $directory.FullName 'ui-pan.json'
    if (-not (Test-Path $summaryPath) -or -not (Test-Path $samplesPath) -or -not (Test-Path $panPath)) {
        continue
    }
    $summary = Get-Content -Raw $summaryPath | ConvertFrom-Json
    $sampleHeaders = (Get-Content $samplesPath -First 1)
    if ($summary.durationSeconds -ne $DurationSeconds -or $sampleHeaders -notlike '*AccumulatedCpuSeconds*') {
        continue
    }
    [pscustomobject]@{
        Directory = $directory
        Summary = $summary
        SamplesPath = $samplesPath
        PanPath = $panPath
    }
}

$selected = foreach ($candidate in @('slint', 'tauri', 'winui')) {
    $candidateRuns = @($eligible | Where-Object { $_.Summary.candidate -eq $candidate } | Sort-Object { $_.Directory.LastWriteTime } -Descending | Select-Object -First 3)
    if ($candidateRuns.Count -ne 3) {
        throw "Expected three $DurationSeconds-second runs for $candidate, found $($candidateRuns.Count)."
    }
    foreach ($run in $candidateRuns) {
        $samples = @(Import-Csv $run.SamplesPath)
        $pan = Get-Content -Raw $run.PanPath | ConvertFrom-Json
        $steadyStart = [datetimeoffset](Get-Item $run.PanPath).LastWriteTimeUtc.AddSeconds(1)
        $steadySamples = @($samples | Where-Object { [datetimeoffset]::Parse($_.TimestampUtc) -ge $steadyStart })
        [pscustomobject]@{
            candidate = $candidate
            run = $run.Directory.Name
            firstWindowMilliseconds = [double]$run.Summary.firstWindowMilliseconds
            totalCpuPercent = Get-CpuPercent $samples
            steadyCollectionCpuPercent = Get-CpuPercent $steadySamples
            peakWorkingSetBytes = [int64]$run.Summary.peakWorkingSetBytes
            peakPrivateBytes = [int64]$run.Summary.peakPrivateBytes
            maximumProcessCount = [int]$run.Summary.maximumProcessCount
            panElapsedMilliseconds = [double]$pan.elapsedMs
            maximumStepGapMilliseconds = [double]$pan.maxStepGapMs
            lateStepCount = [int]$pan.lateStepCount
        }
    }
}

$aggregate = foreach ($candidate in @('slint', 'tauri', 'winui')) {
    $runs = @($selected | Where-Object candidate -eq $candidate)
    [ordered]@{
        candidate = $candidate
        firstWindowMilliseconds = Get-Statistics @($runs.firstWindowMilliseconds)
        totalCpuPercent = Get-Statistics @($runs.totalCpuPercent)
        steadyCollectionCpuPercent = Get-Statistics @($runs.steadyCollectionCpuPercent)
        peakWorkingSetBytes = Get-Statistics @($runs.peakWorkingSetBytes)
        peakPrivateBytes = Get-Statistics @($runs.peakPrivateBytes)
        maximumProcessCount = Get-Statistics @($runs.maximumProcessCount)
        panElapsedMilliseconds = Get-Statistics @($runs.panElapsedMilliseconds)
        maximumStepGapMilliseconds = Get-Statistics @($runs.maximumStepGapMilliseconds)
        lateStepCount = Get-Statistics @($runs.lateStepCount)
    }
}

$selected | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $outputRoot "$OutputPrefix-runs.json") -Encoding utf8
$aggregate | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $outputRoot "$OutputPrefix-summary.json") -Encoding utf8
$aggregate
