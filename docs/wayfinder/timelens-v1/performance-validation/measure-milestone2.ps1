[CmdletBinding()]
param(
    [ValidateRange(60, 300)]
    [int]$DurationSeconds = 60
)

$ErrorActionPreference = 'Stop'
$validationRoot = $PSScriptRoot
$repoRoot = (Resolve-Path (Join-Path $validationRoot '..\..\..\..')).Path
$releaseRoot = Join-Path $repoRoot 'target\release'
$appExe = Join-Path $releaseRoot 'timelens.exe'
$collectorExe = Join-Path $releaseRoot 'timelens-collector.exe'
$capturedAt = Get-Date -Format 'yyyyMMdd-HHmmss'
$outputRoot = Join-Path $validationRoot ("evidence\milestone2-$capturedAt")
$scratchRoot = Join-Path $repoRoot (".scratch\milestone2-performance-$capturedAt")
$memoryBudgetBytes = 100MB
$packageBudgetBytes = 20MB
$cpuBudgetPercent = 1.0

foreach ($required in @($appExe, $collectorExe)) {
    if (-not (Test-Path -LiteralPath $required)) {
        throw "Missing Release artifact: $required"
    }
}

$existing = @(Get-Process -Name 'timelens', 'timelens-collector' -ErrorAction SilentlyContinue)
if ($existing.Count -ne 0) {
    throw "Timelens processes are already running: $($existing.Id -join ',')"
}

New-Item -ItemType Directory -Path $outputRoot -Force | Out-Null
New-Item -ItemType Directory -Path $scratchRoot -Force | Out-Null

function Stop-RunProcesses {
    param($App, $Collector)

    if ($App) {
        $App.Refresh()
        if (-not $App.HasExited) {
            [void]$App.CloseMainWindow()
            if (-not $App.WaitForExit(3000)) {
                Stop-Process -Id $App.Id -ErrorAction SilentlyContinue
            }
        }
    }
    if ($Collector) {
        $Collector.Refresh()
        if (-not $Collector.HasExited) {
            Stop-Process -Id $Collector.Id -ErrorAction SilentlyContinue
        }
    }
}

function Wait-ForMainWindow {
    param($Process, [int]$TimeoutSeconds)

    $deadline = [datetime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $Process.Refresh()
        if ($Process.HasExited) {
            throw "Timelens exited before creating its main window: $($Process.ExitCode)"
        }
        if ($Process.MainWindowHandle -ne 0) {
            return
        }
        Start-Sleep -Milliseconds 100
    } while ([datetime]::UtcNow -lt $deadline)
    throw 'Timelens did not create its main window within 15 seconds.'
}

function Invoke-NormalRun {
    param([int]$RunNumber)

    $runRoot = Join-Path $outputRoot ("run-$RunNumber")
    $dataRoot = Join-Path $scratchRoot ("run-$RunNumber")
    New-Item -ItemType Directory -Path $runRoot -Force | Out-Null
    New-Item -ItemType Directory -Path $dataRoot -Force | Out-Null
    $app = $null
    $collector = $null
    $samples = [System.Collections.Generic.List[object]]::new()
    try {
        $appArguments = @('--data-dir', ('"{0}"' -f $dataRoot))
        $app = Start-Process -FilePath $appExe -ArgumentList $appArguments -PassThru
        Wait-ForMainWindow -Process $app -TimeoutSeconds 15
        $collectorArguments = @('--data-dir', ('"{0}"' -f $dataRoot))
        $collector = Start-Process -FilePath $collectorExe -ArgumentList $collectorArguments -PassThru -WindowStyle Hidden
        Start-Sleep -Seconds 5

        $app.Refresh()
        $collector.Refresh()
        if ($app.HasExited -or $collector.HasExited) {
            throw "A process exited during warm-up (app=$($app.HasExited), collector=$($collector.HasExited))."
        }
        $startedCpuSeconds = [double]$app.CPU + [double]$collector.CPU
        $timer = [System.Diagnostics.Stopwatch]::StartNew()
        while ($timer.Elapsed.TotalSeconds -lt $DurationSeconds) {
            Start-Sleep -Milliseconds 500
            $app.Refresh()
            $collector.Refresh()
            if ($app.HasExited -or $collector.HasExited) {
                throw "A process exited during run $RunNumber."
            }
            $samples.Add([pscustomobject]@{
                ElapsedMs = [math]::Round($timer.Elapsed.TotalMilliseconds, 3)
                AppCpuSeconds = [double]$app.CPU
                CollectorCpuSeconds = [double]$collector.CPU
                WorkingSetBytes = [int64]$app.WorkingSet64 + [int64]$collector.WorkingSet64
                PrivateBytes = [int64]$app.PrivateMemorySize64 + [int64]$collector.PrivateMemorySize64
                AppWorkingSetBytes = [int64]$app.WorkingSet64
                CollectorWorkingSetBytes = [int64]$collector.WorkingSet64
                AppResponding = [bool]$app.Responding
                AppProcessId = [int]$app.Id
                CollectorProcessId = [int]$collector.Id
            })
        }
        $timer.Stop()
        $app.Refresh()
        $collector.Refresh()
        $cpuSeconds = ([double]$app.CPU + [double]$collector.CPU) - $startedCpuSeconds
        $averageCpuPercent = ($cpuSeconds / $timer.Elapsed.TotalSeconds / [Environment]::ProcessorCount) * 100
        $samples | Export-Csv -LiteralPath (Join-Path $runRoot 'process-samples.csv') -NoTypeInformation -Encoding utf8

        [ordered]@{
            run = $RunNumber
            measuredSeconds = $timer.Elapsed.TotalSeconds
            sampleCount = $samples.Count
            logicalProcessorCount = [Environment]::ProcessorCount
            cpuSeconds = $cpuSeconds
            averageCpuPercent = $averageCpuPercent
            peakWorkingSetBytes = [int64](($samples | Measure-Object WorkingSetBytes -Maximum).Maximum)
            peakPrivateBytes = [int64](($samples | Measure-Object PrivateBytes -Maximum).Maximum)
            minimumProcessCount = 2
            maximumProcessCount = 2
            appResponsiveForEverySample = -not ($samples | Where-Object { -not $_.AppResponding } | Select-Object -First 1)
            dataDirectory = $dataRoot
        }
    } finally {
        Stop-RunProcesses -App $app -Collector $collector
    }
}

$runs = @(
    Invoke-NormalRun -RunNumber 1
    Invoke-NormalRun -RunNumber 2
    Invoke-NormalRun -RunNumber 3
)

$packageBytes = [int64](Get-Item -LiteralPath $appExe).Length + [int64](Get-Item -LiteralPath $collectorExe).Length
$signatures = foreach ($path in @($appExe, $collectorExe)) {
    $signature = Get-AuthenticodeSignature -LiteralPath $path
    [ordered]@{
        path = $signature.Path
        status = $signature.Status.ToString()
        statusMessage = $signature.StatusMessage
    }
}
$cpuPass = -not ($runs | Where-Object { $_.averageCpuPercent -gt $cpuBudgetPercent } | Select-Object -First 1)
$memoryPass = -not ($runs | Where-Object { $_.peakWorkingSetBytes -gt $memoryBudgetBytes } | Select-Object -First 1)
$responsivePass = -not ($runs | Where-Object { -not $_.appResponsiveForEverySample } | Select-Object -First 1)
$packagePass = $packageBytes -le $packageBudgetBytes
$passed = $cpuPass -and $memoryPass -and $responsivePass -and $packagePass

$environment = [ordered]@{
    capturedAt = [datetimeoffset]::Now.ToString('O')
    os = Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, BuildNumber, OSArchitecture
    cpu = Get-CimInstance Win32_Processor | Select-Object Name, NumberOfCores, NumberOfLogicalProcessors
    physicalMemoryBytes = [int64](Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory
    powerPlan = (powercfg /GETACTIVESCHEME) -join ''
    rustc = (rustc --version)
    cargo = (cargo --version)
}
$summary = [ordered]@{
    environment = $environment
    scenario = 'Timelens Release UI core plus collector, normal collection'
    measurementSecondsPerRun = $DurationSeconds
    runs = $runs
    budgets = [ordered]@{
        averageCpuPercent = $cpuBudgetPercent
        peakWorkingSetBytes = $memoryBudgetBytes
        packageProxyBytes = $packageBudgetBytes
    }
    packageProxy = [ordered]@{
        files = @($appExe, $collectorExe)
        bytes = $packageBytes
        passed = $packagePass
    }
    authenticode = $signatures
    checks = [ordered]@{
        cpuPassed = $cpuPass
        memoryPassed = $memoryPass
        responsivePassed = $responsivePass
        packagePassed = $packagePass
    }
    passed = $passed
}
$summaryPath = Join-Path $outputRoot 'measurement-summary.json'
$summary | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $summaryPath -Encoding utf8
$summary
if (-not $passed) {
    throw "Milestone 2 performance gate failed. Evidence: $summaryPath"
}
