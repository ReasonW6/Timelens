[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $InstallDir,

    # Holds the elevated collector; defaults to the installation directory itself.
    [string] $ElevatedDir = $InstallDir,

    [switch] $StartNow,
    [ValidatePattern('^\\Timelens(?:-Acceptance-[a-fA-F0-9-]+)?\\$')][string] $TaskPath = '\Timelens\',
    [string] $DataDirectory = '',

    # An empty directory chosen at setup for the signed-in user's new dataset.
    [string] $UserDataDirectory = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'path-safety.ps1')

$taskFolderComPath = $TaskPath.TrimEnd('\')
$resolvedInstallDir = (Resolve-Path -LiteralPath $InstallDir).Path
$resolvedInstallDir = Assert-InstallDirectory $resolvedInstallDir
$resolvedElevatedDir = (Resolve-Path -LiteralPath $ElevatedDir).Path
$resolvedElevatedDir = Assert-InstallDirectory $resolvedElevatedDir -RequireProtectedAncestors
foreach ($directory in @($resolvedInstallDir, $resolvedElevatedDir)) {
    $drive = [System.IO.DriveInfo]::new([System.IO.Path]::GetPathRoot($directory))
    if ($drive.DriveType -ne [System.IO.DriveType]::Fixed) {
        throw 'Timelens tasks can only target an installation on a fixed local drive.'
    }
}

$corePath = Join-Path $resolvedInstallDir 'Timelens.exe'
$collectorPath = Join-Path $resolvedElevatedDir 'Timelens.Collector.exe'
$workerPath = Join-Path $resolvedInstallDir 'Timelens.AI.exe'
foreach ($path in @($corePath, $collectorPath, $workerPath)) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Required Timelens executable is missing: $path"
    }
}

function Get-InteractiveUserName {
    $sessionId = [System.Diagnostics.Process]::GetCurrentProcess().SessionId
    $users = @(
        Get-Process -Name explorer -IncludeUserName -ErrorAction SilentlyContinue |
            Where-Object SessionId -eq $sessionId |
            ForEach-Object UserName |
            Where-Object { $_ } |
            Sort-Object -Unique
    )
    if ($users.Count -ne 1) {
        throw "Expected one Explorer user in session $sessionId; found $($users.Count)."
    }
    return $users[0]
}

function Set-InstallPathAcl {
    param([Parameter(Mandatory)][string] $Path)

    $current = $Path
    while ($current) {
        if ((Get-Item -LiteralPath $current -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Installation paths cannot traverse reparse points.' }
        $current = Split-Path -Parent $current
    }

    $system = [System.Security.Principal.SecurityIdentifier]::new('S-1-5-18')
    $administrators = [System.Security.Principal.SecurityIdentifier]::new('S-1-5-32-544')
    $users = [System.Security.Principal.SecurityIdentifier]::new('S-1-5-32-545')
    $inherit = [System.Security.AccessControl.InheritanceFlags]::ContainerInherit -bor
        [System.Security.AccessControl.InheritanceFlags]::ObjectInherit
    $none = [System.Security.AccessControl.PropagationFlags]::None
    $allow = [System.Security.AccessControl.AccessControlType]::Allow

    $acl = [System.Security.AccessControl.DirectorySecurity]::new()
    $acl.SetAccessRuleProtection($true, $false)
    $acl.SetOwner($administrators)
    $acl.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new(
        $system,
        [System.Security.AccessControl.FileSystemRights]::FullControl,
        $inherit,
        $none,
        $allow
    ))
    $acl.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new(
        $administrators,
        [System.Security.AccessControl.FileSystemRights]::FullControl,
        $inherit,
        $none,
        $allow
    ))
    $acl.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new(
        $users,
        [System.Security.AccessControl.FileSystemRights]::ReadAndExecute,
        $inherit,
        $none,
        $allow
    ))
    Set-Acl -LiteralPath $Path -AclObject $acl

    # The Data folder keeps its own grant to the user, and its dataset is not walked.
    $children = @(Get-ChildItem -LiteralPath $Path -Force | Where-Object Name -ine 'Data')
    $nested = @($children | Where-Object PSIsContainer | ForEach-Object { Get-ChildItem -LiteralPath $_.FullName -Force -Recurse })
    $children + $nested | ForEach-Object {
        $childAcl = Get-Acl -LiteralPath $_.FullName
        $childAcl.SetAccessRuleProtection($false, $false)
        Set-Acl -LiteralPath $_.FullName -AclObject $childAcl
    }
}

$interactiveUser = Get-InteractiveUserName
[void] ([System.Security.Principal.NTAccount]::new($interactiveUser).Translate(
    [System.Security.Principal.SecurityIdentifier]
))
Set-InstallPathAcl -Path $resolvedInstallDir
if ($resolvedElevatedDir -ine $resolvedInstallDir) {
    Set-InstallPathAcl -Path $resolvedElevatedDir
}

$taskService = New-Object -ComObject 'Schedule.Service'
$taskService.Connect()
try {
    [void] $taskService.GetFolder($taskFolderComPath)
} catch [System.IO.FileNotFoundException] {
    $rootTaskFolder = $taskService.GetFolder('\')
    [void] $rootTaskFolder.CreateFolder($TaskPath.Trim('\'))
}

# Creates the chosen directory and lets the signed-in user write to it. Inside the
# installation directory only its Data folder may hold data, and never anything
# under the protected collector directory.
function Initialize-UserDataDirectory {
    param([Parameter(Mandatory)][string] $Path)

    $data = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    $root = [IO.Path]::GetPathRoot($data)
    if ($data.Length -le $root.Length -or $data.Contains('"')) { throw 'Invalid data directory.' }
    if (([IO.DriveInfo]::new($root)).DriveType -ne [IO.DriveType]::Fixed) {
        throw 'The data directory must be on a fixed local drive.'
    }
    $within = { param($Parent) $data -ieq $Parent -or $data.StartsWith($Parent + '\', [StringComparison]::OrdinalIgnoreCase) }
    if (& $within $resolvedElevatedDir) { throw 'The data directory cannot be inside the protected collector directory.' }
    if ((& $within $resolvedInstallDir) -and $data -ine (Join-Path $resolvedInstallDir 'Data')) {
        throw 'Inside the installation directory only its Data folder can hold data.'
    }
    $current = $data
    while ($current) {
        if ((Test-Path -LiteralPath $current) -and ((Get-Item -LiteralPath $current -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw "The data directory cannot pass through a link: $current"
        }
        $current = Split-Path -Parent $current
    }
    if (Test-Path -LiteralPath $data) {
        if (-not (Test-Path -LiteralPath $data -PathType Container) -or @(Get-ChildItem -LiteralPath $data -Force).Count -gt 0) {
            throw "The data directory must be empty: $data"
        }
    } else {
        [void] [IO.Directory]::CreateDirectory($data)
    }
    $acl = Get-Acl -LiteralPath $data
    $acl.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new(
        [System.Security.Principal.NTAccount]::new($interactiveUser),
        [System.Security.AccessControl.FileSystemRights]::Modify,
        ([System.Security.AccessControl.InheritanceFlags]::ContainerInherit -bor
            [System.Security.AccessControl.InheritanceFlags]::ObjectInherit),
        [System.Security.AccessControl.PropagationFlags]::None,
        [System.Security.AccessControl.AccessControlType]::Allow
    ))
    Set-Acl -LiteralPath $data -AclObject $acl
    return $data
}

# The location pointer lives in the user's own profile, so the core records it
# with the user's ordinary rights before its logon task first starts.
function Invoke-CoreAsUser {
    param([Parameter(Mandatory)][string] $Arguments)

    $name = 'Setup-' + [Guid]::NewGuid().ToString('N')
    $action = New-ScheduledTaskAction -Execute $corePath -Argument $Arguments -WorkingDirectory $resolvedInstallDir
    $principal = New-ScheduledTaskPrincipal -UserId $interactiveUser -LogonType Interactive -RunLevel Limited
    $taskSettings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Minutes 2) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    Register-ScheduledTask -TaskPath $TaskPath -TaskName $name -Action $action -Principal $principal -Settings $taskSettings | Out-Null
    try {
        $started = Get-Date
        Start-ScheduledTask -TaskPath $TaskPath -TaskName $name
        do {
            Start-Sleep -Milliseconds 200
            $state = Get-ScheduledTask -TaskPath $TaskPath -TaskName $name
            $info = Get-ScheduledTaskInfo -TaskPath $TaskPath -TaskName $name
            if ((Get-Date) - $started -gt [TimeSpan]::FromMinutes(2)) { throw 'Recording the data location timed out.' }
        } while ($state.State -eq 'Running' -or $state.State -eq 'Queued' -or $info.LastRunTime -lt $started.AddSeconds(-1))
        if ($info.LastTaskResult -ne 0) { throw "Recording the data location returned $($info.LastTaskResult)." }
    } finally {
        Stop-ScheduledTask -TaskPath $TaskPath -TaskName $name -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskPath $TaskPath -TaskName $name -Confirm:$false -ErrorAction SilentlyContinue
    }
}

if ($UserDataDirectory) {
    if ($DataDirectory) { throw 'An isolated install cannot also choose a user data directory.' }
    $userData = Initialize-UserDataDirectory -Path $UserDataDirectory
    Invoke-CoreAsUser -Arguments ('--set-data-location "' + $userData + '"')
}

$trigger = New-ScheduledTaskTrigger -AtLogOn -User $interactiveUser
$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -StartWhenAvailable `
    -RestartCount 3 `
    -RestartInterval (New-TimeSpan -Minutes 1) `
    -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -MultipleInstances IgnoreNew

$arguments = '--background'
if ($DataDirectory) {
    $data = [IO.Path]::GetFullPath($DataDirectory).TrimEnd('\')
    if ($data.Contains('"') -or $data.Length -le 3) { throw 'Invalid isolated data directory.' }
    $arguments += ' --data-dir "' + $data + '"'
}
# The core restarts the collector through its task. It only assumes the default
# task when nothing is isolated, so an isolated install names its task explicitly.
# The collector itself never receives this argument.
$coreArguments = $arguments
if ($DataDirectory -or $TaskPath -ne '\Timelens\') {
    $coreArguments += ' --collector-task "' + $TaskPath + 'Collector"'
}
# The core reads the collector location from the collector task; the collector
# learns the core location from this argument. Both definitions are admin-only.
$collectorArguments = $arguments
if ($resolvedElevatedDir -ine $resolvedInstallDir) {
    $collectorArguments += ' --core-dir "' + $resolvedInstallDir + '"'
}
$coreTask = New-ScheduledTask `
    -Action (New-ScheduledTaskAction `
        -Execute $corePath `
        -Argument $coreArguments `
        -WorkingDirectory $resolvedInstallDir
    ) `
    -Trigger $trigger `
    -Principal (New-ScheduledTaskPrincipal `
        -UserId $interactiveUser `
        -LogonType Interactive `
        -RunLevel Limited
    ) `
    -Settings $settings `
    -Description 'Timelens normal-privilege core and local database writer.'

$collectorTask = New-ScheduledTask `
    -Action (New-ScheduledTaskAction `
        -Execute $collectorPath `
        -Argument $collectorArguments `
        -WorkingDirectory $resolvedElevatedDir
    ) `
    -Trigger $trigger `
    -Principal (New-ScheduledTaskPrincipal `
        -UserId $interactiveUser `
        -LogonType Interactive `
        -RunLevel Highest
    ) `
    -Settings $settings `
    -Description 'Timelens narrow elevated window and input collector.'

Register-ScheduledTask -TaskPath $taskPath -TaskName 'Core' -InputObject $coreTask -Force | Out-Null
Register-ScheduledTask -TaskPath $taskPath -TaskName 'Collector' -InputObject $collectorTask -Force | Out-Null

if ($StartNow) {
    Start-ScheduledTask -TaskPath $taskPath -TaskName 'Core'
    Start-Sleep -Milliseconds 500
    Start-ScheduledTask -TaskPath $taskPath -TaskName 'Collector'
}
