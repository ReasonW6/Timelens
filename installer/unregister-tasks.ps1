[CmdletBinding()]
param(
    [ValidateSet('Keep', 'Delete')][string] $DataMode = 'Delete',
    [string] $TaskPath = '\Timelens\',
    [string] $DataDirectory = ''
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
& (Join-Path $PSScriptRoot 'maintenance.ps1') -InstallDir (Split-Path -Parent $PSScriptRoot) `
    -Mode Uninstall -DataMode $DataMode -TaskPath $TaskPath -DataDirectory $DataDirectory
