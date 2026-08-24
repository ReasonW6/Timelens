[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$taskPath = '\Timelens\'
$taskFolderComPath = '\Timelens'
$installDir = Split-Path -Parent $PSScriptRoot
$corePath = Join-Path $installDir 'Timelens.exe'
$collectorPath = Join-Path $installDir 'Timelens.Collector.exe'

$collectorTask = Get-ScheduledTask -TaskPath $taskPath -TaskName 'Collector' -ErrorAction SilentlyContinue
if ($null -ne $collectorTask) {
    Stop-ScheduledTask -TaskPath $taskPath -TaskName 'Collector' -ErrorAction SilentlyContinue
}

$coreTask = Get-ScheduledTask -TaskPath $taskPath -TaskName 'Core' -ErrorAction SilentlyContinue
if ($null -ne $coreTask) {
    Stop-ScheduledTask -TaskPath $taskPath -TaskName 'Core' -ErrorAction SilentlyContinue
}

$collectorProcesses = @(
    Get-Process -Name 'Timelens.Collector' -ErrorAction SilentlyContinue |
        Where-Object Path -eq $collectorPath
)
if ($collectorProcesses.Count -gt 0) {
    Wait-Process -Id $collectorProcesses.Id -Timeout 10 -ErrorAction Stop
}

$coreProcesses = @(
    Get-Process -Name 'Timelens' -ErrorAction SilentlyContinue |
        Where-Object Path -eq $corePath
)
if ($coreProcesses.Count -gt 0) {
    Wait-Process -Id $coreProcesses.Id -Timeout 10 -ErrorAction Stop
}

if ($null -ne $collectorTask) {
    Unregister-ScheduledTask -TaskPath $taskPath -TaskName 'Collector' -Confirm:$false
}

if ($null -ne $coreTask) {
    Unregister-ScheduledTask -TaskPath $taskPath -TaskName 'Core' -Confirm:$false
}

$taskService = New-Object -ComObject 'Schedule.Service'
$taskService.Connect()
$rootTaskFolder = $taskService.GetFolder('\')
try {
    [void] $taskService.GetFolder($taskFolderComPath)
    $rootTaskFolder.DeleteFolder('Timelens', 0)
} catch [System.IO.FileNotFoundException] {
    # A partially completed install may not have created the task folder.
}
