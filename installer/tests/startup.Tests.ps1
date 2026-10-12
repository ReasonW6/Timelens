$ErrorActionPreference = 'Stop'

# Load only the functions under test, without running installer maintenance.
function Import-InstallerFunction([string] $File, [string] $Name) {
    $tokens = $null
    $errors = $null
    $ast = [Management.Automation.Language.Parser]::ParseFile($File, [ref]$tokens, [ref]$errors)
    if ($errors.Count -gt 0) { throw $errors[0] }
    $function = $ast.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $Name }, $true)
    if (-not $function) { throw "Missing function: $Name" }
    Invoke-Expression ($function.Extent.Text -replace '^function ', 'function script:')
}

$installer = Split-Path -Parent $PSScriptRoot
Import-InstallerFunction (Join-Path $installer 'maintenance.ps1') 'Set-TaskStartup'
Import-InstallerFunction (Join-Path $installer 'register-tasks.ps1') 'New-ProductLogonTrigger'

function New-TestFolder {
    $folder = [pscustomobject]@{
        Tasks = @{}
        FailName = ''
        Failed = $false
        Calls = [Collections.ArrayList]::new()
    }
    foreach ($name in @('Core', 'Collector')) {
        $definition = [pscustomobject]@{
            Triggers = @([pscustomobject]@{ Type = 9; Enabled = $true; UserId = 'test-user' }, [pscustomobject]@{ Type = 1; Enabled = $false })
            Settings = [pscustomobject]@{ Enabled = $true; RestartCount = 3; ExecutionTimeLimit = 'PT0S' }
            Actions = @([pscustomobject]@{ Path = "C:\Program Files\Timelens\$name.exe"; Arguments = '--background' })
            Principal = [pscustomobject]@{ LogonType = 3; RunLevel = $(if ($name -eq 'Collector') { 1 } else { 0 }); UserId = 'test-user' }
        }
        $folder.Tasks[$name] = [pscustomobject]@{ Xml = ($definition | ConvertTo-Json -Depth 10 -Compress); Security = 'D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;GRGX;;;BU)' }
    }
    $folder | Add-Member ScriptMethod GetTask {
        param($name)
        $stored = $this.Tasks[$name]
        $definition = $stored.Xml | ConvertFrom-Json
        $task = [pscustomobject]@{ Definition = $definition; Xml = $stored.Xml; Security = $stored.Security; Enabled = $definition.Settings.Enabled }
        $task | Add-Member ScriptMethod GetSecurityDescriptor { param($flags) return $this.Security }
        return $task
    }
    $folder | Add-Member ScriptMethod RegisterTaskDefinition {
        param($name, $definition, $flags, $user, $password, $logonType, $security)
        [void]$this.Calls.Add("update:$name")
        if ($name -eq $this.FailName -and -not $this.Failed) {
            $this.Failed = $true
            throw 'Simulated second task registration failure'
        }
        $this.Tasks[$name] = [pscustomobject]@{ Xml = ($definition | ConvertTo-Json -Depth 10 -Compress); Security = $security }
    }
    $folder | Add-Member ScriptMethod RegisterTask {
        param($name, $xml, $flags, $user, $password, $logonType, $security)
        [void]$this.Calls.Add("rollback:$name")
        $this.Tasks[$name] = [pscustomobject]@{ Xml = $xml; Security = $security }
    }
    return $folder
}

Describe 'Installed startup toggle' {
    It 'disables both logon triggers while retaining manual starts, actions, principals, other triggers and DACLs' {
        $folder = New-TestFolder
        $original = @{}
        foreach ($name in @('Core', 'Collector')) { $original[$name] = $folder.GetTask($name) }
        Set-TaskStartup $folder $false
        foreach ($name in @('Core', 'Collector')) {
            $saved = $folder.GetTask($name)
            $saved.Enabled | Should Be $true
            $saved.Definition.Triggers[0].Enabled | Should Be $false
            $saved.Definition.Triggers[1].Enabled | Should Be $false
            ($saved.Definition.Actions | ConvertTo-Json -Compress) | Should Be ($original[$name].Definition.Actions | ConvertTo-Json -Compress)
            ($saved.Definition.Principal | ConvertTo-Json -Compress) | Should Be ($original[$name].Definition.Principal | ConvertTo-Json -Compress)
            ($saved.Definition.Settings | ConvertTo-Json -Compress) | Should Be ($original[$name].Definition.Settings | ConvertTo-Json -Compress)
            $saved.Security | Should Be $original[$name].Security
        }
    }
    It 'enables both logon triggers and restores a task disabled outside Timelens' {
        $folder = New-TestFolder
        Set-TaskStartup $folder $false
        $definition = $folder.Tasks.Core.Xml | ConvertFrom-Json
        $definition.Settings.Enabled = $false
        $folder.Tasks.Core.Xml = $definition | ConvertTo-Json -Depth 10 -Compress
        Set-TaskStartup $folder $true
        foreach ($name in @('Core', 'Collector')) {
            $folder.GetTask($name).Enabled | Should Be $true
            $folder.GetTask($name).Definition.Triggers[0].Enabled | Should Be $true
        }
    }
    It 'restores the original definitions if the second update fails' {
        $folder = New-TestFolder
        $coreXml = $folder.Tasks.Core.Xml
        $collectorXml = $folder.Tasks.Collector.Xml
        $folder.FailName = 'Collector'
        { Set-TaskStartup $folder $false } | Should Throw
        $folder.Tasks.Core.Xml | Should Be $coreXml
        $folder.Tasks.Collector.Xml | Should Be $collectorXml
        $folder.Calls.Contains('rollback:Core') | Should Be $true
        $folder.Calls.Contains('rollback:Collector') | Should Be $true
    }
    It 'does not update either task when a logon trigger is missing' {
        $folder = New-TestFolder
        $definition = $folder.Tasks.Collector.Xml | ConvertFrom-Json
        $definition.Triggers = @($definition.Triggers[1])
        $folder.Tasks.Collector.Xml = $definition | ConvertTo-Json -Depth 10 -Compress
        { Set-TaskStartup $folder $false } | Should Throw
        $folder.Calls.Count | Should Be 0
    }
}

Describe 'Startup choice during upgrades' {
    $script:interactiveUser = 'test-user'
    $script:TaskPath = '\Timelens\'
    Mock New-ScheduledTaskTrigger { [pscustomobject]@{ Enabled = $true } }
    It 'enables startup for a new installation' {
        Mock Get-ScheduledTask { $null }
        (New-ProductLogonTrigger 'Core').Enabled | Should Be $true
    }
    It 'retains a disabled logon trigger' {
        Mock Get-ScheduledTask { [pscustomobject]@{ Settings = [pscustomobject]@{ Enabled = $true }; Triggers = @([pscustomobject]@{ CimClass = [pscustomobject]@{ CimClassName = 'MSFT_TaskLogonTrigger' }; Enabled = $false }) } }
        (New-ProductLogonTrigger 'Core').Enabled | Should Be $false
    }
    It 'retains an enabled logon trigger' {
        Mock Get-ScheduledTask { [pscustomobject]@{ Settings = [pscustomobject]@{ Enabled = $true }; Triggers = @([pscustomobject]@{ CimClass = [pscustomobject]@{ CimClassName = 'MSFT_TaskLogonTrigger' }; Enabled = $true }) } }
        (New-ProductLogonTrigger 'Collector').Enabled | Should Be $true
    }
    It 'retains disabled startup when the task itself was disabled' {
        Mock Get-ScheduledTask { [pscustomobject]@{ Settings = [pscustomobject]@{ Enabled = $false }; Triggers = @([pscustomobject]@{ CimClass = [pscustomobject]@{ CimClassName = 'MSFT_TaskLogonTrigger' }; Enabled = $true }) } }
        (New-ProductLogonTrigger 'Collector').Enabled | Should Be $false
    }
}
