[CmdletBinding()]
param(
    [ValidateSet('Keep', 'Delete')][string] $DataMode = 'Delete',
    [string] $TaskPath = '\Timelens\',
    [string] $DataDirectory = '',
    # The scripts live in the elevated directory; the core may be installed elsewhere.
    [string] $ElevatedDir = (Split-Path -Parent $PSScriptRoot),
    [string] $InstallDir = $ElevatedDir
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
& (Join-Path $PSScriptRoot 'maintenance.ps1') -InstallDir $InstallDir -ElevatedDir $ElevatedDir `
    -Mode Uninstall -DataMode $DataMode -TaskPath $TaskPath -DataDirectory $DataDirectory
