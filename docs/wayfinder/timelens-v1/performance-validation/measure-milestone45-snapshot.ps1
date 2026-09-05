[CmdletBinding()]
param(
    [Parameter(Mandatory)][int] $CoreProcessId,
    [Parameter(Mandatory)][int] $CollectorProcessId,
    [Parameter(Mandatory)][string] $OutputDirectory,
    [ValidateRange(15, 120)][int] $DurationSeconds = 30
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..\..')).Path
$app = Get-Process -Id $CoreProcessId
$collector = Get-Process -Id $CollectorProcessId
if ($app.Path -ne (Join-Path $repo 'target\release\timelens.exe') -or
    $collector.Path -ne (Join-Path $repo 'target\release\timelens-collector.exe')) {
    throw 'Only the explicitly selected workspace Release processes may be measured.'
}
[void](New-Item -ItemType Directory -Path $OutputDirectory -Force)
$samples = [Collections.Generic.List[object]]::new()
$watch = [Diagnostics.Stopwatch]::StartNew()
$lastResponseCheckMs = -1000.0
$responsive = $true
$initialCpu = [double]$app.CPU + [double]$collector.CPU
Write-Output 'Sampling is active. Trigger one manual capture in the isolated Timelens UI.'
while ($watch.Elapsed.TotalSeconds -lt $DurationSeconds) {
    $app.Refresh(); $collector.Refresh()
    if ($app.HasExited -or $collector.HasExited) { throw 'A selected process exited during measurement.' }
    if ($watch.Elapsed.TotalMilliseconds - $lastResponseCheckMs -ge 250) {
        $responsive = $responsive -and [bool]$app.Responding
        $lastResponseCheckMs = $watch.Elapsed.TotalMilliseconds
    }
    $samples.Add([pscustomobject]@{
        elapsedMs = [math]::Round($watch.Elapsed.TotalMilliseconds, 3)
        workingSetBytes = $app.WorkingSet64 + $collector.WorkingSet64
        privateBytes = $app.PrivateMemorySize64 + $collector.PrivateMemorySize64
        totalCpuSeconds = [double]$app.CPU + [double]$collector.CPU
    })
    Start-Sleep -Milliseconds 20
}
$watch.Stop()
$samples | Export-Csv -LiteralPath (Join-Path $OutputDirectory 'snapshot-samples.csv') -NoTypeInformation -Encoding UTF8
$intervals = for ($i = 1; $i -lt $samples.Count; $i++) { $samples[$i].elapsedMs - $samples[$i - 1].elapsedMs }
$summary = [ordered]@{
    capturedAt = [DateTimeOffset]::Now.ToString('O')
    scenario = 'Visible Release Slint core and Collector; one user-interface manual capture; scheduled capture disabled; no AI worker'
    measuredSeconds = $watch.Elapsed.TotalSeconds
    sampleCount = $samples.Count
    requestedSampleIntervalMs = 20
    averageSampleIntervalMs = [double](($intervals | Measure-Object -Average).Average)
    maximumSampleIntervalMs = [double](($intervals | Measure-Object -Maximum).Maximum)
    initialWorkingSetBytes = $samples[0].workingSetBytes
    peakWorkingSetBytes = [int64](($samples | Measure-Object workingSetBytes -Maximum).Maximum)
    finalWorkingSetBytes = $samples[$samples.Count - 1].workingSetBytes
    peakPrivateBytes = [int64](($samples | Measure-Object privateBytes -Maximum).Maximum)
    averageCpuPercent = ($samples[$samples.Count - 1].totalCpuSeconds - $initialCpu) / $watch.Elapsed.TotalSeconds / [Environment]::ProcessorCount * 100
    uiResponding = $responsive
    interpretation = 'On-demand sampled peak, not the steady-state 100 MiB gate. Confirm the successful capture separately before accepting this record.'
}
$summary | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'snapshot-performance.json') -Encoding UTF8
$summary | ConvertTo-Json -Depth 5
if (-not $responsive) { throw 'The UI failed a response check.' }
