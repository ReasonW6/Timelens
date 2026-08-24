[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $InstallDir,

    [switch] $StartNow
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$taskPath = '\Timelens\'
$taskFolderComPath = '\Timelens'
$resolvedInstallDir = (Resolve-Path -LiteralPath $InstallDir).Path
$drive = [System.IO.DriveInfo]::new([System.IO.Path]::GetPathRoot($resolvedInstallDir))
if ($drive.DriveType -ne [System.IO.DriveType]::Fixed) {
    throw 'Timelens tasks can only target an installation on a fixed local drive.'
}

$corePath = Join-Path $resolvedInstallDir 'Timelens.exe'
$collectorPath = Join-Path $resolvedInstallDir 'Timelens.Collector.exe'
foreach ($path in @($corePath, $collectorPath)) {
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

    Get-ChildItem -LiteralPath $Path -Force -Recurse | ForEach-Object {
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

$taskService = New-Object -ComObject 'Schedule.Service'
$taskService.Connect()
try {
    [void] $taskService.GetFolder($taskFolderComPath)
} catch [System.IO.FileNotFoundException] {
    $rootTaskFolder = $taskService.GetFolder('\')
    [void] $rootTaskFolder.CreateFolder('Timelens')
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

$coreTask = New-ScheduledTask `
    -Action (New-ScheduledTaskAction `
        -Execute $corePath `
        -Argument '--background' `
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
        -Argument '--background' `
        -WorkingDirectory $resolvedInstallDir
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
