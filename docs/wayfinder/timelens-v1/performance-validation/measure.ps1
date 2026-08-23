[CmdletBinding()]
param(
    [ValidateRange(10, 300)]
    [int]$NormalObserverSeconds = 60,

    [ValidateRange(10, 120)]
    [int]$StormObserverSeconds = 30
)

$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$outputRoot = Join-Path $root ('evidence\' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
$release = Join-Path $root 'target\release'
$suiteExe = Join-Path $release 'timelens-performance-suite.exe'
$stormExe = Join-Path $release 'timelens-window-storm.exe'
$observerExe = Join-Path $release 'window-observer.exe'
$spikeRoot = Join-Path (Split-Path -Parent $root) 'spikes'
$slintExe = Join-Path $spikeRoot 'release-target\release\timelens-slint-spike.exe'
$timelineSource = Get-ChildItem -LiteralPath (Join-Path $spikeRoot 'benchmark-output') -Recurse -Filter 'timeline.json' -File |
    Sort-Object LastWriteTime -Descending |
    Select-Object -First 1
foreach ($required in @($suiteExe, $stormExe, $observerExe,$slintExe)) {
    if (-not (Test-Path -LiteralPath $required)) {
        throw "Missing Release artifact: $required"
    }
}
if (-not $timelineSource) {
    throw 'No existing 10,000-segment timeline fixture was found.'
}
New-Item -ItemType Directory -Path $outputRoot -Force | Out-Null

function Get-TreeIds {
    param([int[]]$RootIds)
    $rows = Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId
    $known = [System.Collections.Generic.HashSet[int]]::new()
    foreach ($id in $RootIds) { [void]$known.Add($id) }
    do {
        $added = $false
        foreach ($row in $rows) {
            if ($known.Contains([int]$row.ParentProcessId) -and $known.Add([int]$row.ProcessId)) {
                $added = $true
            }
        }
    } while ($added)
    @($known)
}

function Invoke-MeasuredScenario {
    param(
        [string]$Name,
        [scriptblock]$Start,
        [scriptblock]$IsComplete,
        [scriptblock]$Stop
    )
    $scenarioRoot = Join-Path $outputRoot $Name
    New-Item -ItemType Directory -Path $scenarioRoot -Force | Out-Null
    $state = & $Start $scenarioRoot
    $samples = [System.Collections.Generic.List[object]]::new()
    $cpuById = @{}
    $accumulatedCpu = 0.0
    $timer = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        do {
            $ids = Get-TreeIds -RootIds @($state.RootIds)
            $processes = @($ids | ForEach-Object { Get-Process -Id $_ -ErrorAction SilentlyContinue })
            $delta = 0.0
            foreach ($process in $processes) {
                $current = [double]$process.CPU
                if ($cpuById.ContainsKey($process.Id)) {
                    $delta += [Math]::Max(0.0, $current - [double]$cpuById[$process.Id])
                }
                $cpuById[$process.Id] = $current
            }
            $accumulatedCpu += $delta
            $samples.Add([pscustomobject]@{
                ElapsedMs = $timer.Elapsed.TotalMilliseconds
                ProcessCount = $processes.Count
                AccumulatedCpuSeconds = $accumulatedCpu
                WorkingSetBytes = [int64](($processes | Measure-Object WorkingSet64 -Sum).Sum)
                PrivateBytes = [int64](($processes | Measure-Object PrivateMemorySize64 -Sum).Sum)
                ProcessIds = ($processes.Id -join ',')
                ProcessNames = ($processes.ProcessName -join ',')
            })
            Start-Sleep -Milliseconds 100
        } while (-not (& $IsComplete $state))
    } finally {
        & $Stop $state
    }
    $timer.Stop()
    $samples | Export-Csv -LiteralPath (Join-Path $scenarioRoot 'process-samples.csv') -NoTypeInformation -Encoding utf8
    [ordered]@{
        scenario = $Name
        elapsedSeconds = $timer.Elapsed.TotalSeconds
        sampleCount = $samples.Count
        averageCpuPercent = ($accumulatedCpu / [Math]::Max(0.001, $timer.Elapsed.TotalSeconds) / [Environment]::ProcessorCount) * 100
        peakWorkingSetBytes = [int64](($samples | Measure-Object WorkingSetBytes -Maximum).Maximum)
        peakPrivateBytes = [int64](($samples | Measure-Object PrivateBytes -Maximum).Maximum)
        maximumProcessCount = [int](($samples | Measure-Object ProcessCount -Maximum).Maximum)
        exitCodes = @($state.Processes | ForEach-Object { $_.Refresh(); if ($_.HasExited) { $_.ExitCode } else { $null } })
        outputDirectory = $scenarioRoot
    }
}

$normal = Invoke-MeasuredScenario -Name 'observer-normal' -Start {
    param($scenarioRoot)
    $log = Join-Path $scenarioRoot 'observer.jsonl'
    $arguments = @('--output', ('"{0}"' -f $log), '--duration-seconds', $NormalObserverSeconds, '--reconcile-ms', 30000, '--class-prefix', 'Timelens.Performance.NoMatch') -join ' '
    $observer = Start-Process -FilePath $observerExe -ArgumentList $arguments -PassThru -WindowStyle Hidden
    [pscustomobject]@{ RootIds=@($observer.Id); Processes=@($observer); Observer=$observer }
} -IsComplete {
    param($state)
    $state.Observer.Refresh()
    $state.Observer.HasExited
} -Stop {
    param($state)
    if (-not $state.Observer.HasExited) { Stop-Process -Id $state.Observer.Id -ErrorAction SilentlyContinue }
}

$storm = Invoke-MeasuredScenario -Name 'observer-window-storm' -Start {
    param($scenarioRoot)
    $observerLog = Join-Path $scenarioRoot 'observer.jsonl'
    $observerArguments = @('--output', ('"{0}"' -f $observerLog), '--duration-seconds', $StormObserverSeconds, '--reconcile-ms', 30000, '--class-prefix', 'Timelens.Performance.Storm') -join ' '
    $observer = Start-Process -FilePath $observerExe -ArgumentList $observerArguments -PassThru -WindowStyle Hidden
    Start-Sleep -Milliseconds 750
    $stormMetric = Join-Path $scenarioRoot 'window-storm.json'
    $stormArguments = @('--out', ('"{0}"' -f $stormMetric), '--cycles', 500) -join ' '
    $stormProcess = Start-Process -FilePath $stormExe -ArgumentList $stormArguments -PassThru -WindowStyle Hidden
    [pscustomobject]@{ RootIds=@($observer.Id,$stormProcess.Id); Processes=@($observer,$stormProcess); Observer=$observer; Storm=$stormProcess }
} -IsComplete {
    param($state)
    $state.Observer.Refresh()
    $state.Observer.HasExited
} -Stop {
    param($state)
    foreach ($process in $state.Processes) {
        $process.Refresh()
        if (-not $process.HasExited) { Stop-Process -Id $process.Id -ErrorAction SilentlyContinue }
    }
}
if (-not (Test-Path -LiteralPath (Join-Path $storm.outputDirectory 'window-storm.json'))) {
    throw 'Window-storm fixture did not finish within the observer measurement window.'
}

$suite = Invoke-MeasuredScenario -Name 'core-suite' -Start {
    param($scenarioRoot)
    $arguments = @('--out', ('"{0}"' -f $scenarioRoot)) -join ' '
    $process = Start-Process -FilePath $suiteExe -ArgumentList $arguments -PassThru -WindowStyle Hidden
    [pscustomobject]@{ RootIds=@($process.Id); Processes=@($process); Suite=$process }
} -IsComplete {
    param($state)
    $state.Suite.Refresh()
    $state.Suite.HasExited
} -Stop {
    param($state)
    if (-not $state.Suite.HasExited) { Stop-Process -Id $state.Suite.Id -ErrorAction SilentlyContinue }
}

$fullStack = Invoke-MeasuredScenario -Name 'full-stack-peak' -Start {
    param($scenarioRoot)
    $timeline = Join-Path $scenarioRoot 'timeline.json'
    Copy-Item -LiteralPath $timelineSource.FullName -Destination $timeline
    $env:TIMELENS_SPIKE_TIMELINE = $timeline
    $env:TIMELENS_SPIKE_OUTPUT = $scenarioRoot
    $ui = Start-Process -FilePath $slintExe -PassThru
    $deadline = [datetime]::UtcNow.AddSeconds(10)
    do {
        $ui.Refresh()
        if ($ui.HasExited) { throw "Slint UI exited before full-stack measurement: $($ui.ExitCode)" }
        if ($ui.MainWindowHandle -ne 0) { break }
        Start-Sleep -Milliseconds 50
    } while ([datetime]::UtcNow -lt $deadline)
    if ($ui.MainWindowHandle -eq 0) { throw 'Slint UI did not create a window for full-stack measurement.' }

    $observerLog = Join-Path $scenarioRoot 'observer.jsonl'
    $observerArguments = @('--output', ('"{0}"' -f $observerLog), '--duration-seconds', 30, '--reconcile-ms', 30000, '--class-prefix', 'Timelens.Performance.NoMatch') -join ' '
    $observer = Start-Process -FilePath $observerExe -ArgumentList $observerArguments -PassThru -WindowStyle Hidden
    $suiteArguments = @('--out', ('"{0}"' -f $scenarioRoot)) -join ' '
    $suiteProcess = Start-Process -FilePath $suiteExe -ArgumentList $suiteArguments -PassThru -WindowStyle Hidden
    [pscustomobject]@{
        RootIds=@($ui.Id,$observer.Id,$suiteProcess.Id)
        Processes=@($ui,$observer,$suiteProcess)
        UI=$ui
        Observer=$observer
        Suite=$suiteProcess
    }
} -IsComplete {
    param($state)
    $state.Suite.Refresh()
    $state.Suite.HasExited
} -Stop {
    param($state)
    $state.UI.Refresh()
    if (-not $state.UI.HasExited) {
        [void]$state.UI.CloseMainWindow()
        if (-not $state.UI.WaitForExit(3000)) { Stop-Process -Id $state.UI.Id -ErrorAction SilentlyContinue }
    }
    $state.Observer.Refresh()
    if (-not $state.Observer.HasExited) { Stop-Process -Id $state.Observer.Id -ErrorAction SilentlyContinue }
    $state.Suite.Refresh()
    if (-not $state.Suite.HasExited) { Stop-Process -Id $state.Suite.Id -ErrorAction SilentlyContinue }
}
if (-not (Test-Path -LiteralPath (Join-Path $fullStack.outputDirectory 'suite-metrics.json'))) {
    throw 'Full-stack core suite did not complete.'
}

$environment = [ordered]@{
    capturedAt = [datetimeoffset]::Now.ToString('O')
    os = Get-CimInstance Win32_OperatingSystem | Select-Object Caption,Version,BuildNumber,OSArchitecture
    cpu = Get-CimInstance Win32_Processor | Select-Object Name,NumberOfCores,NumberOfLogicalProcessors
    physicalMemoryBytes = [int64](Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory
    powerPlan = (powercfg /GETACTIVESCHEME) -join ''
    rustc = (rustc --version)
    cargo = (cargo --version)
    display = Get-CimInstance Win32_VideoController | Select-Object Name,CurrentHorizontalResolution,CurrentVerticalResolution
}
$summary = [ordered]@{
    environment = $environment
    scenarios = @($normal,$storm,$suite,$fullStack)
}
$summaryPath = Join-Path $outputRoot 'measurement-summary.json'
$summary | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $summaryPath -Encoding utf8
$summary
