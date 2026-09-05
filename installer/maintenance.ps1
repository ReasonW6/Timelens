[CmdletBinding()]
param(
    [Parameter(Mandatory)][string] $InstallDir,
    [ValidateSet('PrepareUpgrade', 'Uninstall')][string] $Mode = 'PrepareUpgrade',
    [ValidateSet('Keep', 'Delete')][string] $DataMode = 'Delete',
    [ValidatePattern('^\\Timelens(?:-Acceptance-[a-fA-F0-9-]+)?\\$')][string] $TaskPath = '\Timelens\',
    [string] $DataDirectory = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'path-safety.ps1')

function Assert-FixedDirectory([string] $Path) {
    $full = [IO.Path]::GetFullPath($Path)
    $drive = [IO.DriveInfo]::new([IO.Path]::GetPathRoot($full))
    if ($drive.DriveType -ne [IO.DriveType]::Fixed) { throw 'A fixed local drive is required.' }
    $current = $full
    while ($current) {
        if ((Test-Path -LiteralPath $current) -and ((Get-Item -LiteralPath $current -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw "Reparse points are not permitted in the installation path: $current"
        }
        $current = Split-Path -Parent $current
    }
    return $full.TrimEnd('\')
}

function Get-InteractiveUser {
    $sessionId = [Diagnostics.Process]::GetCurrentProcess().SessionId
    $users = @(Get-Process -Name explorer -IncludeUserName -ErrorAction SilentlyContinue |
        Where-Object SessionId -eq $sessionId | ForEach-Object UserName | Where-Object { $_ } | Sort-Object -Unique)
    if ($users.Count -ne 1) { throw 'Exactly one interactive Explorer user is required for per-user maintenance.' }
    return $users[0]
}

$targetDir = Assert-InstallDirectory $InstallDir
$core = Join-Path $targetDir 'Timelens.exe'
$tasks = @(Get-ScheduledTask -TaskPath $TaskPath -ErrorAction SilentlyContinue | Where-Object TaskName -In @('Core', 'Collector'))
# An upgrade may choose a different installation directory. Only the protected
# existing task definition is allowed to identify the previous executable.
$oldCoreTask = $tasks | Where-Object TaskName -eq 'Core' | Select-Object -First 1
if ($Mode -eq 'PrepareUpgrade' -and $oldCoreTask) {
    $oldCore = @($oldCoreTask.Actions)[0].Execute
    if ([IO.Path]::GetFileName($oldCore) -ine 'Timelens.exe') { throw 'Unexpected core task target.' }
    $targetDir = Assert-FixedDirectory (Split-Path -Parent $oldCore)
    $core = Join-Path $targetDir 'Timelens.exe'
}
$collector = Join-Path $targetDir 'Timelens.Collector.exe'
$worker = Join-Path $targetDir 'Timelens.AI.exe'
foreach ($task in $tasks) {
    $expected = if ($task.TaskName -eq 'Core') { $core } else { $collector }
    if (@($task.Actions).Count -ne 1 -or @($task.Actions)[0].Execute -ine $expected) {
        throw "Refusing to alter a task with an unexpected executable: $($task.TaskName)"
    }
}
$dataArgument = ''
if ($DataDirectory) {
    $data = Assert-FixedDirectory $DataDirectory
    if ($data.Contains('"') -or $data.Length -le 3) { throw 'Invalid isolated data directory.' }
    $dataArgument = ' --data-dir "' + $data + '"'
}

function Invoke-UserCore([string] $Arguments) {
    if (-not (Test-Path -LiteralPath $core -PathType Leaf)) { throw 'The ordinary-privilege maintenance executable is missing.' }
    $user = Get-InteractiveUser
    $service = New-Object -ComObject 'Schedule.Service'
    $service.Connect()
    $folder = $TaskPath.TrimEnd('\')
    try { [void]$service.GetFolder($folder) } catch [IO.FileNotFoundException] {
        [void]$service.GetFolder('\').CreateFolder($folder.Trim('\'))
    }
    $name = 'Maintenance-' + [Guid]::NewGuid().ToString('N')
    $action = New-ScheduledTaskAction -Execute $core -Argument ($Arguments + $dataArgument) -WorkingDirectory $targetDir
    $principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Minutes 10) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    Register-ScheduledTask -TaskPath $TaskPath -TaskName $name -Action $action -Principal $principal -Settings $settings | Out-Null
    try {
        $started = Get-Date
        Start-ScheduledTask -TaskPath $TaskPath -TaskName $name
        do {
            Start-Sleep -Milliseconds 200
            $state = Get-ScheduledTask -TaskPath $TaskPath -TaskName $name
            $info = Get-ScheduledTaskInfo -TaskPath $TaskPath -TaskName $name
            if ((Get-Date) - $started -gt [TimeSpan]::FromMinutes(5)) { throw 'Per-user maintenance timed out; uninstall has not continued.' }
        } while ($state.State -eq 'Running' -or $state.State -eq 'Queued' -or $info.LastRunTime -lt $started.AddSeconds(-1))
        if ($info.LastTaskResult -ne 0) { throw "Per-user maintenance returned $($info.LastTaskResult). Local data may require recovery." }
    } finally {
        Stop-ScheduledTask -TaskPath $TaskPath -TaskName $name -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskPath $TaskPath -TaskName $name -Confirm:$false -ErrorAction SilentlyContinue
    }
}

$paths = @($core, $collector, $worker)
function Get-ProductProcesses {
    @(Get-Process -Name 'Timelens', 'Timelens.Collector', 'Timelens.AI' -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and $paths -contains $_.Path })
}

if (@(Get-ProductProcesses).Count -gt 0) {
    try { Invoke-UserCore '--shutdown' } catch { Write-Verbose 'The prior version did not acknowledge graceful shutdown; stopping only its registered tasks.' }
    $deadline = (Get-Date).AddSeconds(20)
    while (@(Get-ProductProcesses).Count -gt 0 -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 200 }
}
foreach ($task in $tasks) { Stop-ScheduledTask -TaskPath $TaskPath -TaskName $task.TaskName -ErrorAction SilentlyContinue }
# Legacy builds can have been launched outside Task Scheduler. Match exact paths.
foreach ($process in @(Get-ProductProcesses)) {
    Stop-Process -Id $process.Id -ErrorAction Stop
    Wait-Process -Id $process.Id -Timeout 10 -ErrorAction SilentlyContinue
}
if (@(Get-ProductProcesses).Count -gt 0) { throw 'Timelens is still using its installation files.' }

if ($Mode -eq 'Uninstall') {
    if ($DataMode -eq 'Delete') { Invoke-UserCore '--uninstall-data' }
    foreach ($task in $tasks) { Unregister-ScheduledTask -TaskPath $TaskPath -TaskName $task.TaskName -Confirm:$false }
    $service = New-Object -ComObject 'Schedule.Service'
    $service.Connect()
    try {
        $folder = $service.GetFolder($TaskPath.TrimEnd('\'))
        if ($folder.GetTasks(0).Count -eq 0) { $service.GetFolder('\').DeleteFolder($TaskPath.Trim('\'), 0) }
    } catch [IO.FileNotFoundException] { }
}
