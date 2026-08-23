[CmdletBinding()]
param(
    [ValidateRange(1, 10)]
    [int]$Repetitions = 3,

    [ValidateRange(10, 3600)]
    [int]$DurationSeconds = 30
)

$ErrorActionPreference = 'Stop'
$measure = Join-Path $PSScriptRoot 'measure-candidate.ps1'
foreach ($repetition in 1..$Repetitions) {
    foreach ($candidate in @('slint', 'tauri', 'winui')) {
        Write-Host "[$repetition/$Repetitions] $candidate"
        & $measure -Candidate $candidate -DurationSeconds $DurationSeconds
    }
}
