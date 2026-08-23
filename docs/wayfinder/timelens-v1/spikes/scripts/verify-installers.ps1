[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$PackageRoot
)

$ErrorActionPreference = 'Stop'
$resolvedPackageRoot = (Resolve-Path -LiteralPath $PackageRoot).Path
$spikeRoot = Split-Path -Parent $PSScriptRoot
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$installRoot = Join-Path $spikeRoot "install-verification\$stamp"
New-Item -ItemType Directory -Path $installRoot -Force | Out-Null
$env:TIMELENS_SPIKE_INSTALL_ROOT = $installRoot

$results = foreach ($candidate in @('slint', 'tauri', 'winui')) {
    $setup = Join-Path $resolvedPackageRoot "$candidate\Timelens-$candidate-spike-setup.exe"
    if (-not (Test-Path -LiteralPath $setup)) {
        throw "Missing installer: $setup"
    }
    $process = Start-Process -FilePath $setup -PassThru -Wait -WindowStyle Hidden
    if ($process.ExitCode -ne 0) {
        throw "$candidate installer failed with exit code $($process.ExitCode)"
    }
    $installed = Join-Path $installRoot $candidate
    $files = @(Get-ChildItem $installed -Recurse -File)
    [ordered]@{
        candidate = $candidate
        installerExitCode = $process.ExitCode
        installedFileCount = $files.Count
        installedBytes = [int64](($files | Measure-Object Length -Sum).Sum)
        installDirectory = $installed
    }
}

$results | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $installRoot 'verification.json') -Encoding utf8
$results
