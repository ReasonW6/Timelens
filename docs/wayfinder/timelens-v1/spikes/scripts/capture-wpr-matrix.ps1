[CmdletBinding()]
param(
    [ValidateRange(10, 3600)]
    [int]$DurationSeconds = 30
)

$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'WPR matrix capture must run from an elevated PowerShell window.'
}

$spikeRoot = Split-Path -Parent $PSScriptRoot
$resultPath = Join-Path $spikeRoot 'benchmark-output\wpr\local-controlled-status.json'
$results = @()

try {
    foreach ($candidate in @('slint', 'tauri', 'winui')) {
        $tracePath = & (Join-Path $PSScriptRoot 'capture-wpr.ps1') -Candidate $candidate -DurationSeconds $DurationSeconds
        $results += [ordered]@{
            candidate = $candidate
            status = 'completed'
            tracePath = [string]($tracePath | Select-Object -Last 1)
        }
    }
} catch {
    $results += [ordered]@{
        candidate = if ($candidate) { $candidate } else { 'unknown' }
        status = 'failed'
        error = $_.Exception.Message
    }
    throw
} finally {
    $directory = Split-Path -Parent $resultPath
    New-Item -ItemType Directory -Path $directory -Force | Out-Null
    $results | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $resultPath -Encoding utf8
}
