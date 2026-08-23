[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$spikeRoot = Split-Path -Parent $PSScriptRoot
$rustTarget = Join-Path $spikeRoot 'release-target'
$portableDotnet = Join-Path $spikeRoot '.tools\dotnet\dotnet.exe'

Push-Location $spikeRoot
try {
    cargo build --release --offline --target-dir $rustTarget
    if ($LASTEXITCODE -ne 0) {
        throw "Cargo release build failed with exit code $LASTEXITCODE"
    }

    $dotnet = if (Test-Path -LiteralPath $portableDotnet) {
        $portableDotnet
    } else {
        (Get-Command dotnet -ErrorAction Stop).Source
    }
    $sdkLines = & $dotnet --list-sdks
    if (-not $sdkLines) {
        throw 'A .NET SDK is required; installed runtimes alone cannot build the WinUI spike.'
    }

    $project = Join-Path $spikeRoot 'winui-spike\TimelensWinUISpike.csproj'
    & $dotnet restore $project -p:Platform=x64
    if ($LASTEXITCODE -ne 0) {
        throw ".NET restore failed with exit code $LASTEXITCODE"
    }
    & $dotnet publish $project -c Release -p:Platform=x64 --no-restore
    if ($LASTEXITCODE -ne 0) {
        throw ".NET publish failed with exit code $LASTEXITCODE"
    }
} finally {
    Pop-Location
}
