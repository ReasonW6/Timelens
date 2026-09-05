[CmdletBinding()]
param([Parameter(Mandatory)][string] $AcceptanceDirectory)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
if (-not ([Security.Principal.WindowsPrincipal]::new($identity)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'This isolated installation acceptance must be launched with user-approved elevation.'
}
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..\..')).Path
$root = (Resolve-Path -LiteralPath $AcceptanceDirectory).Path
if ($root -ne (Join-Path $repo 'target\acceptance\84d2c6a9')) { throw 'Unexpected acceptance scope.' }
$product = 'Timelens-Acceptance-84d2c6a9'
$install = Join-Path $env:ProgramFiles $product
$taskPath = '\Timelens-Acceptance-84d2c6a9\'
$setup = Join-Path $root 'installer\Timelens-Acceptance.exe'
$control = Join-Path $root 'data'
$pointer = Join-Path $control 'data-location.txt'
$data = if (Test-Path -LiteralPath $pointer) { @(Get-Content -LiteralPath $pointer)[1] } else { $control }
$data = [IO.Path]::GetFullPath($data).TrimEnd('\')
if (-not $data.StartsWith($root + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'The data pointer escaped the synthetic acceptance root.' }
$keyPath = Join-Path $data 'data-key.dpapi'
$external = Join-Path $root 'final-ui-plain.zip'
$externalHash = (Get-FileHash -LiteralPath $external -Algorithm SHA256).Hash
$keyHash = (Get-FileHash -LiteralPath $keyPath -Algorithm SHA256).Hash
$corePath = Join-Path $install 'Timelens.exe'
$collectorPath = Join-Path $install 'Timelens.Collector.exe'
$workerPath = Join-Path $install 'Timelens.AI.exe'
$paths = @($corePath, $collectorPath, $workerPath)
$checks = [ordered]@{}
$result = [ordered]@{ startedAt = [DateTimeOffset]::Now.ToString('O'); checks = $checks; completed = $false; error = $null }
$reportPath = Join-Path $root 'installer-result.json'
Start-Transcript -LiteralPath (Join-Path $root 'installer-acceptance-transcript.log') -Force | Out-Null
function Save-Result { $result | ConvertTo-Json -Depth 7 | Set-Content -LiteralPath $reportPath -Encoding UTF8 }
function Assert-Check([string] $Name, [bool] $Passed) {
    $checks[$Name] = $Passed
    Save-Result
    if (-not $Passed) { throw "Acceptance failed: $Name" }
}
function Invoke-Setup([string] $Label) {
    $arguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', ('/DIR="{0}"' -f $install), ('/LOG="{0}"' -f (Join-Path $root "$Label.log")))
    $process = Start-Process -FilePath $setup -ArgumentList $arguments -WindowStyle Hidden -Wait -PassThru
    Assert-Check "$Label-exit-zero" ($process.ExitCode -eq 0)
    Start-Sleep -Seconds 5
}
function Product-Processes {
    @(Get-Process -Name Timelens, Timelens.Collector, Timelens.AI -ErrorAction SilentlyContinue | Where-Object { $_.Path -in $paths })
}
function Invoke-Uninstall([string] $DataMode) {
    $uninstaller = Join-Path $install 'unins000.exe'
    $arguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/DATA=$DataMode", ('/LOG="{0}"' -f (Join-Path $root "uninstall-$DataMode.log")))
    $process = Start-Process -FilePath $uninstaller -ArgumentList $arguments -WindowStyle Hidden -Wait -PassThru
    Assert-Check "uninstall-$DataMode-exit-zero" ($process.ExitCode -eq 0)
    Start-Sleep -Seconds 2
    Assert-Check "uninstall-$DataMode-no-product-processes" (@(Product-Processes).Count -eq 0)
    Assert-Check "uninstall-$DataMode-no-product-tasks" (@(Get-ScheduledTask -TaskPath $taskPath -ErrorAction SilentlyContinue).Count -eq 0)
    Assert-Check "uninstall-$DataMode-removed-payload" (-not (Test-Path -LiteralPath $corePath))
}
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
public static class TimelensAcceptanceToken {
    [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr OpenProcess(uint access, bool inherit, int id);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    [DllImport("advapi32.dll", SetLastError=true)] static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
    [DllImport("advapi32.dll", SetLastError=true)] static extern bool GetTokenInformation(IntPtr token, int type, out int value, int size, out int returned);
    [DllImport("advapi32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern bool CredReadW(string target, int type, int flags, out IntPtr credential);
    [DllImport("advapi32.dll")] static extern void CredFree(IntPtr credential);
    public static bool SyntheticCredentialExists() {
        IntPtr credential;
        if (CredReadW("Timelens/ai/acceptance-ui-84d2c6a9", 1, 0, out credential)) { CredFree(credential); return true; }
        int error = Marshal.GetLastWin32Error();
        if (error != 1168) throw new Win32Exception(error);
        return false;
    }
    public static bool IsElevated(int id) {
        IntPtr process = OpenProcess(0x1000, false, id), token = IntPtr.Zero;
        if (process == IntPtr.Zero) throw new Win32Exception();
        try {
            if (!OpenProcessToken(process, 8, out token)) throw new Win32Exception();
            int value, returned;
            if (!GetTokenInformation(token, 20, out value, 4, out returned)) throw new Win32Exception();
            return value != 0;
        } finally { if (token != IntPtr.Zero) CloseHandle(token); CloseHandle(process); }
    }
}
'@
try {
    Assert-Check 'new-isolated-installation' (-not (Test-Path -LiteralPath $install))
    Assert-Check 'no-existing-acceptance-tasks' (@(Get-ScheduledTask -TaskPath $taskPath -ErrorAction SilentlyContinue).Count -eq 0)
    Assert-Check 'no-running-timelens' (@(Get-Process -Name Timelens, timelens-collector, Timelens.Collector, timelens-ai-worker, Timelens.AI -ErrorAction SilentlyContinue).Count -eq 0)
    Assert-Check 'synthetic-credential-exists-before-install' ([TimelensAcceptanceToken]::SyntheticCredentialExists())
    Invoke-Setup 'install'
    $coreTask = Get-ScheduledTask -TaskPath $taskPath -TaskName Core
    $collectorTask = Get-ScheduledTask -TaskPath $taskPath -TaskName Collector
    Assert-Check 'core-task-limited' ($coreTask.Principal.RunLevel -eq 'Limited')
    Assert-Check 'collector-task-highest' ($collectorTask.Principal.RunLevel -eq 'Highest')
    Assert-Check 'tasks-use-isolated-data' ($coreTask.Actions.Arguments.Contains($control) -and $collectorTask.Actions.Arguments.Contains($control))
    $processes = @(Product-Processes)
    $core = @($processes | Where-Object Path -eq $corePath)
    $collector = @($processes | Where-Object Path -eq $collectorPath)
    Assert-Check 'resident-processes-running' ($core.Count -eq 1 -and $collector.Count -eq 1)
    Assert-Check 'core-process-not-elevated' (-not [TimelensAcceptanceToken]::IsElevated($core[0].Id))
    Assert-Check 'collector-process-elevated' ([TimelensAcceptanceToken]::IsElevated($collector[0].Id))
    Assert-Check 'ai-worker-is-on-demand' (@($processes | Where-Object Path -eq $workerPath).Count -eq 0)
    Assert-Check 'three-payloads-present' ((Test-Path -LiteralPath $corePath) -and (Test-Path -LiteralPath $collectorPath) -and (Test-Path -LiteralPath $workerPath))
    . (Join-Path $repo 'installer\path-safety.ps1')
    Assert-Check 'protected-installation-ancestors' ((Assert-InstallDirectory $install) -eq $install)
    $acl = Get-Acl -LiteralPath $install
    $ordinaryWriteRules = @($acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]) | Where-Object {
        $_.AccessControlType -eq 'Allow' -and $_.IdentityReference.Value -notin @('S-1-5-18', 'S-1-5-32-544') -and ([long]$_.FileSystemRights -band 0xD0156)
    })
    Assert-Check 'ordinary-users-cannot-modify-payload' ($ordinaryWriteRules.Count -eq 0)
    Assert-Check 'install-preserved-data-key' ((Get-FileHash -LiteralPath $keyPath -Algorithm SHA256).Hash -eq $keyHash)
    Invoke-Setup 'upgrade'
    Assert-Check 'upgrade-preserved-data-key' ((Get-FileHash -LiteralPath $keyPath -Algorithm SHA256).Hash -eq $keyHash)
    Assert-Check 'upgrade-restarted-two-resident-processes' (@(Product-Processes).Count -eq 2)
    Invoke-Uninstall 'keep'
    Assert-Check 'keep-preserved-encrypted-data' ((Test-Path -LiteralPath (Join-Path $data 'timelens.sqlite3')) -and (Get-FileHash -LiteralPath $keyPath -Algorithm SHA256).Hash -eq $keyHash)
    Invoke-Setup 'reinstall'
    Assert-Check 'reinstall-preserved-data-key' ((Get-FileHash -LiteralPath $keyPath -Algorithm SHA256).Hash -eq $keyHash)
    Invoke-Uninstall 'delete'
    Assert-Check 'delete-removed-active-encrypted-data' (-not (Test-Path -LiteralPath $keyPath) -and -not (Test-Path -LiteralPath (Join-Path $data 'timelens.sqlite3')))
    Assert-Check 'delete-removed-location-pointer' (-not (Test-Path -LiteralPath $pointer))
    Assert-Check 'delete-removed-synthetic-credential' (-not [TimelensAcceptanceToken]::SyntheticCredentialExists())
    Assert-Check 'external-export-preserved' ((Get-FileHash -LiteralPath $external -Algorithm SHA256).Hash -eq $externalHash)
    $result.completed = $true
} catch {
    $result.error = $_.Exception.Message
    throw
} finally {
    $result['finishedAt'] = [DateTimeOffset]::Now.ToString('O')
    Save-Result
    Stop-Transcript | Out-Null
}
