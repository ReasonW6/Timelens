[CmdletBinding()]
param(
    [Parameter(Mandatory)][string] $Endpoint,
    [Parameter(Mandatory)][string] $SlowEndpoint,
    [Parameter(Mandatory)][string] $ScratchDirectory,
    [Parameter(Mandatory)][string] $OutputDirectory,
    [ValidateRange(60, 300)][int] $DurationSeconds = 60
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..\..')).Path
$release = Join-Path $repo 'target\release'
$appExe = Join-Path $release 'timelens.exe'
$collectorExe = Join-Path $release 'timelens-collector.exe'
$workerExe = Join-Path $release 'timelens-ai-worker.exe'
$fixtureExe = Join-Path $release 'examples\milestone45_fixture.exe'
$groupPaths = @($appExe, $collectorExe, $workerExe)
if ($Endpoint -notmatch '^http://127\.0\.0\.1:\d+/v1$' -or $SlowEndpoint -notmatch '^http://127\.0\.0\.1:\d+/v1$') {
    throw 'Only synthetic loopback endpoints are allowed.'
}
if (@(Get-Process -Name timelens, timelens-collector, timelens-ai-worker, Timelens.Collector, Timelens.AI -ErrorAction SilentlyContinue).Count) {
    throw 'Close Timelens before running isolated performance acceptance.'
}
foreach ($path in @($ScratchDirectory, $OutputDirectory)) { [void](New-Item -ItemType Directory -Path $path -Force) }
$runs = [Collections.Generic.List[object]]::new()
$runNumber = 0
foreach ($mode in @('normal', 'normal', 'normal', 'idle', 'ai')) {
    $runNumber++
    $data = Join-Path $ScratchDirectory "run-$runNumber-$mode"
    if (Test-Path -LiteralPath $data) { throw 'The performance dataset must be new.' }
    $fixtureEndpoint = if ($mode -eq 'ai') { $SlowEndpoint } else { $Endpoint }
    $seedOutput = & $fixtureExe $data $fixtureEndpoint $mode
    if ($LASTEXITCODE -ne 0) { throw 'Synthetic fixture failed.' }
    $credential = @($seedOutput | Where-Object { $_ -match '^credential acceptance-(normal|idle|ai)-\d+-\d+$' })
    if ($credential.Count -ne 1) { throw 'Unable to identify the synthetic credential for cleanup.' }
    $credentialId = $credential[0].Substring('credential '.Length)
    $app = $null
    $collector = $null
    $samples = [Collections.Generic.List[object]]::new()
    try {
        $arguments = @('--data-dir', ('"{0}"' -f $data))
        # The visible window is intentional: acceptance measures a real Slint UI.
        $app = Start-Process -FilePath $appExe -ArgumentList $arguments -PassThru -RedirectStandardError (Join-Path $data 'app.stderr.log')
        $collector = Start-Process -FilePath $collectorExe -ArgumentList $arguments -PassThru -WindowStyle Hidden -RedirectStandardError (Join-Path $data 'collector.stderr.log')
        Start-Sleep -Seconds 5
        $cpuByPid = @{}
        $initialCpu = 0.0
        $initialProcesses = @(Get-Process -Name timelens, timelens-collector, timelens-ai-worker -ErrorAction SilentlyContinue | Where-Object { $_.Path -in $groupPaths })
        foreach ($p in $initialProcesses) { $cpuByPid[$p.Id] = [double]$p.CPU; $initialCpu += [double]$p.CPU }
        $watch = [Diagnostics.Stopwatch]::StartNew()
        while ($watch.Elapsed.TotalSeconds -lt $DurationSeconds) {
            Start-Sleep -Milliseconds 500
            $app.Refresh(); $collector.Refresh()
            if ($app.HasExited -or $collector.HasExited) { throw 'A resident process exited during measurement.' }
            $processes = @(Get-Process -Name timelens, timelens-collector, timelens-ai-worker -ErrorAction SilentlyContinue | Where-Object { $_.Path -in $groupPaths })
            $working = 0L; $private = 0L
            foreach ($p in $processes) {
                $cpuByPid[$p.Id] = [double]$p.CPU
                $working += $p.WorkingSet64
                $private += $p.PrivateMemorySize64
            }
            $samples.Add([pscustomobject]@{
                elapsedMs = [math]::Round($watch.Elapsed.TotalMilliseconds, 2)
                workingSetBytes = $working
                privateBytes = $private
                processCount = $processes.Count
                aiWorkerPresent = [bool]($processes | Where-Object Path -eq $workerExe)
                uiResponding = [bool]$app.Responding
                totalCpuSeconds = [double](($cpuByPid.Values | Measure-Object -Sum).Sum)
            })
        }
        $watch.Stop()
        $cpu = ([double](($cpuByPid.Values | Measure-Object -Sum).Sum) - $initialCpu) / $watch.Elapsed.TotalSeconds / [Environment]::ProcessorCount * 100
        $peak = [int64](($samples | Measure-Object workingSetBytes -Maximum).Maximum)
        $responsive = @($samples | Where-Object { -not $_.uiResponding }).Count -eq 0
        $workers = @($samples | Where-Object aiWorkerPresent).Count
        $cpuBudget = if ($mode -eq 'idle') { 0.5 } elseif ($mode -eq 'normal') { 1.0 } else { $null }
        $passed = $peak -le 100MB -and $responsive -and ($null -eq $cpuBudget -or $cpu -le $cpuBudget) -and ($mode -ne 'ai' -or $workers -gt 0)
        $samples | Export-Csv -LiteralPath (Join-Path $OutputDirectory "run-$runNumber-$mode.csv") -NoTypeInformation -Encoding UTF8
        $runs.Add([ordered]@{
            run = $runNumber; mode = $mode; measuredSeconds = $watch.Elapsed.TotalSeconds
            averageCpuPercent = $cpu; cpuBudgetPercent = $cpuBudget
            peakWorkingSetBytes = $peak
            peakPrivateBytes = [int64](($samples | Measure-Object privateBytes -Maximum).Maximum)
            sampleCount = $samples.Count; aiWorkerSamples = $workers
            minimumProcessCount = [int](($samples | Measure-Object processCount -Minimum).Minimum)
            maximumProcessCount = [int](($samples | Measure-Object processCount -Maximum).Maximum)
            uiResponding = $responsive; passed = $passed
        })
        $runs | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'performance-runs.json') -Encoding UTF8
        if ($mode -eq 'ai') { Start-Sleep -Seconds 5 }
        Write-Output ("Completed {0}: CPU {1:N3}%, peak {2:N2} MiB, passed={3}" -f $mode, $cpu, ($peak / 1MB), $passed)
    } finally {
        if ($app -and -not $app.HasExited) {
            & $appExe --shutdown
            Wait-Process -Id $app.Id -Timeout 25 -ErrorAction SilentlyContinue
        }
        foreach ($p in @($app, $collector)) {
            if ($p) {
                $p.Refresh()
                if (-not $p.HasExited -and $p.Path -in $groupPaths) { Stop-Process -Id $p.Id; Wait-Process -Id $p.Id -Timeout 10 -ErrorAction SilentlyContinue }
            }
        }
        & cmdkey.exe ("/delete:Timelens/ai/$credentialId") | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Synthetic credential cleanup failed.' }
    }
}
$summary = [ordered]@{
    capturedAt = [DateTimeOffset]::Now.ToString('O')
    logicalProcessors = [Environment]::ProcessorCount
    modes = 'Three normal collection runs; one globally paused run; one real core-launched AI worker streaming synthetic loopback data'
    memoryBudgetBytes = 100MB
    runs = $runs
    passed = @($runs | Where-Object { -not $_.passed }).Count -eq 0
}
$summary | ConvertTo-Json -Depth 7 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'performance-summary.json') -Encoding UTF8
if (-not $summary.passed) { throw 'A final performance budget was exceeded. Inspect the retained evidence.' }
