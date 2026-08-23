[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$spikeRoot = Split-Path -Parent $PSScriptRoot
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$outputRoot = Join-Path $spikeRoot "package-output\$stamp"
$driver = Join-Path $spikeRoot 'release-target\release\timelens-spike-driver.exe'
$publish = Join-Path $spikeRoot 'winui-spike\publish-minimal'
$iexpress = Join-Path $env:SystemRoot 'System32\iexpress.exe'
$vcRuntimeRoot = Get-ChildItem 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Redist\MSVC' -Directory |
    Sort-Object Name -Descending |
    ForEach-Object { Join-Path $_.FullName 'x64\Microsoft.VC143.CRT' } |
    Where-Object { Test-Path -LiteralPath $_ } |
    Select-Object -First 1
$vcRuntimeFiles = @(
    Join-Path $vcRuntimeRoot 'vcruntime140.dll'
    Join-Path $vcRuntimeRoot 'vcruntime140_1.dll'
)

$candidates = @{
    slint = Join-Path $spikeRoot 'release-target\release\timelens-slint-spike.exe'
    tauri = Join-Path $spikeRoot 'release-target\release\timelens-tauri-spike.exe'
    winui = $publish
}

foreach ($required in @($driver, $candidates.slint, $candidates.tauri, $publish, $iexpress) + $vcRuntimeFiles) {
    if (-not (Test-Path -LiteralPath $required)) {
        throw "Missing packaging input: $required"
    }
}

$metrics = foreach ($candidate in @('slint', 'tauri', 'winui')) {
    $candidateRoot = Join-Path $outputRoot $candidate
    $payloadRoot = Join-Path $candidateRoot 'payload'
    $stageRoot = Join-Path $candidateRoot 'installer-stage'
    New-Item -ItemType Directory -Path $payloadRoot -Force | Out-Null
    New-Item -ItemType Directory -Path $stageRoot -Force | Out-Null

    Copy-Item -LiteralPath $driver -Destination $payloadRoot
    if ($candidate -eq 'winui') {
        Copy-Item -Path (Join-Path $publish '*') -Destination $payloadRoot -Recurse
    } else {
        Copy-Item -LiteralPath $candidates[$candidate] -Destination $payloadRoot
        Copy-Item -LiteralPath $vcRuntimeFiles -Destination $payloadRoot
    }

    $payloadZip = Join-Path $stageRoot 'payload.zip'
    Compress-Archive -Path (Join-Path $payloadRoot '*') -DestinationPath $payloadZip -CompressionLevel Optimal

    $installCommand = @"
@echo off
setlocal
set "TARGET=%LOCALAPPDATA%\Timelens Architecture Spikes\$candidate"
if defined TIMELENS_SPIKE_INSTALL_ROOT set "TARGET=%TIMELENS_SPIKE_INSTALL_ROOT%\$candidate"
if not exist "%TARGET%" mkdir "%TARGET%"
powershell.exe -NoProfile -NonInteractive -Command "Expand-Archive -LiteralPath '%~dp0payload.zip' -DestinationPath '%TARGET%' -Force"
exit /b %ERRORLEVEL%
"@
    $installPath = Join-Path $stageRoot 'install.cmd'
    Set-Content -LiteralPath $installPath -Value $installCommand -Encoding ascii

    $setupPath = Join-Path $candidateRoot "Timelens-$candidate-spike-setup.exe"
    $sedPath = Join-Path $candidateRoot 'package.sed'
    $sed = @"
[Version]
Class=IEXPRESS
SEDVersion=3
[Options]
PackagePurpose=InstallApp
ShowInstallProgramWindow=0
HideExtractAnimation=1
UseLongFileName=1
InsideCompressed=0
CAB_FixedSize=0
CAB_ResvCodeSigning=0
RebootMode=N
InstallPrompt=
DisplayLicense=
FinishMessage=
TargetName=$setupPath
FriendlyName=Timelens $candidate architecture spike
AppLaunched=install.cmd
PostInstallCmd=<None>
AdminQuietInstCmd=
UserQuietInstCmd=
SourceFiles=SourceFiles
[Strings]
FILE0="payload.zip"
FILE1="install.cmd"
[SourceFiles]
SourceFiles0=$stageRoot\
[SourceFiles0]
%FILE0%=
%FILE1%=
"@
    Set-Content -LiteralPath $sedPath -Value $sed -Encoding ascii

    $package = Start-Process -FilePath $iexpress -ArgumentList '/N', $sedPath -PassThru -Wait -WindowStyle Hidden
    if ($package.ExitCode -ne 0 -or -not (Test-Path -LiteralPath $setupPath)) {
        throw "IExpress packaging failed for $candidate with exit code $($package.ExitCode)"
    }

    $payloadFiles = @(Get-ChildItem $payloadRoot -Recurse -File)
    [ordered]@{
        candidate = $candidate
        payloadFileCount = $payloadFiles.Count
        installedPayloadBytes = [int64](($payloadFiles | Measure-Object Length -Sum).Sum)
        compressedPayloadBytes = (Get-Item $payloadZip).Length
        installerBytes = (Get-Item $setupPath).Length
        installer = $setupPath
        runtimeTreatment = switch ($candidate) {
            'tauri' { 'Uses the Windows-provided WebView2 runtime; runtime bytes excluded.' }
            'winui' { 'Both .NET and Windows App SDK are self-contained and counted; no external .NET runtime prerequisite.' }
            default { 'Visual C++ runtime is deployed app-local and counted; no external UI runtime.' }
        }
    }
}

$metricsPath = Join-Path $outputRoot 'package-metrics.json'
$metrics | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $metricsPath -Encoding utf8
$metrics
