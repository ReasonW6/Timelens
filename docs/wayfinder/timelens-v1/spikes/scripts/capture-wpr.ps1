[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('slint', 'tauri', 'winui')]
    [string]$Candidate,

    [ValidateRange(10, 3600)]
    [int]$DurationSeconds = 30
)

$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'WPR capture must run from an elevated PowerShell window.'
}

$spikeRoot = Split-Path -Parent $PSScriptRoot
$traceRoot = Join-Path $spikeRoot 'benchmark-output\wpr'
New-Item -ItemType Directory -Path $traceRoot -Force | Out-Null
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$tracePath = Join-Path $traceRoot "$Candidate-$stamp.etl"

wpr.exe -start GeneralProfile -filemode
if ($LASTEXITCODE -ne 0) {
    throw "WPR start failed with exit code $LASTEXITCODE"
}
try {
    & (Join-Path $PSScriptRoot 'measure-candidate.ps1') -Candidate $Candidate -DurationSeconds $DurationSeconds
} finally {
    wpr.exe -stop $tracePath
}
if ($LASTEXITCODE -ne 0) {
    throw "WPR stop failed with exit code $LASTEXITCODE"
}
$tracePath
