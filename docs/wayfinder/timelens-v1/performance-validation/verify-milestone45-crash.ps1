[CmdletBinding()]
param([Parameter(Mandatory)][string] $DataDirectory, [Parameter(Mandatory)][string] $Endpoint, [Parameter(Mandatory)][string] $OutputPath)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..\..')).Path
$release = Join-Path $repo 'target\release'
$appExe = Join-Path $release 'timelens.exe'
$collectorExe = Join-Path $release 'timelens-collector.exe'
if (Test-Path -LiteralPath $DataDirectory) { throw 'Crash acceptance requires a new, isolated dataset.' }
if (@(Get-Process -Name timelens, timelens-collector, Timelens.Collector -ErrorAction SilentlyContinue).Count) { throw 'Close Timelens first.' }
$seed = & (Join-Path $release 'examples\milestone45_fixture.exe') $DataDirectory $Endpoint normal
if ($LASTEXITCODE -ne 0) { throw 'Fixture creation failed.' }
$credential = @($seed | Where-Object { $_ -match '^credential acceptance-normal-\d+-\d+$' })
if ($credential.Count -ne 1) { throw 'Unexpected synthetic credential.' }
$key = Join-Path $DataDirectory 'data-key.dpapi'
$before = (Get-FileHash -LiteralPath $key -Algorithm SHA256).Hash
$app = $null; $collector = $null
$arguments = @('--background', '--data-dir', ('"{0}"' -f $DataDirectory))
try {
    $app = Start-Process -FilePath $appExe -ArgumentList $arguments -WindowStyle Hidden -PassThru
    $collector = Start-Process -FilePath $collectorExe -ArgumentList $arguments -WindowStyle Hidden -PassThru
    Start-Sleep -Seconds 5
    if ($app.Path -ne $appExe -or $collector.Path -ne $collectorExe) { throw 'Unexpected process path.' }
    Stop-Process -Id $app.Id -Force
    [void]$app.WaitForExit(3000)
    Start-Sleep -Seconds 5
    $app = Start-Process -FilePath $appExe -ArgumentList $arguments -WindowStyle Hidden -PassThru
    Start-Sleep -Seconds 5
    $app.Refresh(); $collector.Refresh()
    if ($app.HasExited -or $collector.HasExited) { throw 'Core restart failed.' }
    Stop-Process -Id $collector.Id -Force
    [void]$collector.WaitForExit(3000)
    Start-Sleep -Seconds 3
    $collector = Start-Process -FilePath $collectorExe -ArgumentList $arguments -WindowStyle Hidden -PassThru
    Start-Sleep -Seconds 5
    & $appExe --shutdown
    if (-not $app.WaitForExit(25000) -or -not $collector.WaitForExit(5000)) { throw 'Graceful shutdown did not finish.' }
    $inspection = & (Join-Path $release 'examples\inspect_dataset.exe') $DataDirectory 120
    if ($LASTEXITCODE -ne 0) { throw 'The dataset did not reopen after forced process crashes.' }
    $gaps = @($inspection | Where-Object { $_ -like 'gap=activity reason=collector_restart*' })
    $systemEnd = @($inspection | Where-Object { $_ -like 'system_interval=system_end*' }).Count -eq 1
    $sameKey = (Get-FileHash -LiteralPath $key -Algorithm SHA256).Hash -eq $before
    $summary = [ordered]@{ coreRestarted = $true; collectorRestarted = $true; encryptedDatasetReopened = $true; dataKeyUnchanged = $sameKey; collectorRestartGapCount = $gaps.Count; gracefulSystemEndRecorded = $systemEnd; passed = $sameKey -and $systemEnd -and $gaps.Count -ge 2 }
    $summary | ConvertTo-Json | Set-Content -LiteralPath $OutputPath -Encoding UTF8
    if (-not $summary.passed) { throw 'Crash recovery invariants failed.' }
    $summary
} finally {
    foreach ($p in @($app, $collector)) {
        if ($p) { $p.Refresh(); if (-not $p.HasExited -and $p.Path -in @($appExe, $collectorExe)) { Stop-Process -Id $p.Id -Force } }
    }
    & cmdkey.exe ("/delete:Timelens/ai/" + $credential[0].Substring('credential '.Length)) | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Synthetic credential cleanup failed.' }
}
