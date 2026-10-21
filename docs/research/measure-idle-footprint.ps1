<#
.SYNOPSIS
测量 Timelens 核心在三种常驻状态下的内存、CPU 与唤醒次数，以及冷启动到窗口可见的时间。

.DESCRIPTION
用隔离的合成数据和独立管道运行指定的 timelens.exe。核心是单实例的，运行前必须先停止已安装的
Timelens 核心（采集器可以继续运行，它会把数据留在自己的环形缓冲里）。

每一轮都从原始合成数据复制一份新的数据目录，各轮之间互不影响。阶段依次为：
  hidden-never-shown  以 --background 启动，从未显示窗口
  visible-idle        通过第二次启动唤起窗口，停在首页不操作
  hidden-after-shown  向主窗口发送 WM_CLOSE（关到托盘）后
最后单独测量 StartupRuns 次冷启动：从进程启动到主窗口可见的时间。

唤醒次数取自 Win32_PerfRawData_PerfProc_Thread 中所有线程上下文切换计数的差值。
私有工作集即任务管理器“内存”列显示的数值。

.EXAMPLE
.\measure-idle-footprint.ps1 -Exe .\target\release\timelens.exe -Fixture D:\tmp\fixture -Label before -Out before.json
#>
param(
    [Parameter(Mandatory)][string] $Exe,
    [Parameter(Mandatory)][string] $Fixture,
    [Parameter(Mandatory)][string] $Label,
    [Parameter(Mandatory)][string] $Out,
    [int] $SettleSeconds = 20,
    [int] $HiddenSettleSeconds = 45,
    [int] $SampleSeconds = 60,
    [int] $StartupRuns = 3
)
$ErrorActionPreference = 'Stop'

Add-Type @"
using System; using System.Runtime.InteropServices; using System.Text;
public static class TlWin {
    delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc p, IntPtr l);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
    public static IntPtr Find(uint pid, string cls) {
        IntPtr found = IntPtr.Zero;
        EnumWindows((h, l) => {
            uint p; GetWindowThreadProcessId(h, out p);
            if (p != pid) return true;
            var sb = new StringBuilder(256); GetClassName(h, sb, 256);
            if (sb.ToString() == cls) { found = h; return false; }
            return true;
        }, IntPtr.Zero);
        return found;
    }
}
"@

$Exe = (Resolve-Path $Exe).Path
$Fixture = (Resolve-Path $Fixture).Path
$pipe = "\\.\pipe\Timelens.measure.$PID"
$work = Join-Path ([IO.Path]::GetTempPath()) "timelens-measure-$Label-$PID"

if (Get-Process -Name Timelens -ErrorAction SilentlyContinue | Where-Object { $_.Path -ne $Exe }) {
    throw '已安装的 Timelens 核心仍在运行，请先停止它（例如 Stop-ScheduledTask -TaskPath \Timelens\ -TaskName Core）。'
}

function New-DataDir {
    if (Test-Path $work) { Remove-Item -LiteralPath $work -Recurse -Force }
    Copy-Item -LiteralPath $Fixture -Destination $work -Recurse
    # 预先写入“已说明关到托盘”的标记，避免关窗时弹出系统通知。
    Set-Content -LiteralPath (Join-Path $work 'tray-explained.txt') -Value '1' -NoNewline
    $work
}

function Wait-Until([scriptblock] $Condition, [int] $TimeoutMs, [string] $What) {
    $watch = [Diagnostics.Stopwatch]::StartNew()
    while (-not (& $Condition)) {
        if ($watch.ElapsedMilliseconds -gt $TimeoutMs) { throw "等待超时：$What" }
        Start-Sleep -Milliseconds 20
    }
    $watch.ElapsedMilliseconds
}

function Get-ContextSwitches([int] $ProcessId) {
    (Get-CimInstance Win32_PerfRawData_PerfProc_Thread -Filter "IDProcess=$ProcessId" |
        Measure-Object ContextSwitchesPersec -Sum).Sum
}

function Get-PrivateWorkingSet([int] $ProcessId) {
    (Get-CimInstance Win32_PerfRawData_PerfProc_Process -Filter "IDProcess=$ProcessId").WorkingSetPrivate
}

function Measure-Phase([Diagnostics.Process] $Process, [string] $Name) {
    $Process.Refresh()
    $cpu0 = $Process.TotalProcessorTime.TotalMilliseconds
    $cs0 = Get-ContextSwitches $Process.Id
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $samples = for ($i = 0; $i -lt $SampleSeconds; $i++) {
        Start-Sleep -Milliseconds ([Math]::Max(0, ($i + 1) * 1000 - $watch.ElapsedMilliseconds))
        $Process.Refresh()
        [pscustomobject]@{
            private = $Process.PrivateMemorySize64
            ws      = $Process.WorkingSet64
            pws     = Get-PrivateWorkingSet $Process.Id
        }
    }
    $Process.Refresh()
    $seconds = $watch.Elapsed.TotalSeconds
    $cpu = $Process.TotalProcessorTime.TotalMilliseconds - $cpu0
    $cs = (Get-ContextSwitches $Process.Id) - $cs0
    $mib = { param($v) [Math]::Round($v / 1MB, 2) }
    [pscustomobject]@{
        phase                = $Name
        seconds              = [Math]::Round($seconds, 1)
        private_avg_mib      = & $mib ($samples | Measure-Object private -Average).Average
        private_max_mib      = & $mib ($samples | Measure-Object private -Maximum).Maximum
        private_ws_avg_mib   = & $mib ($samples | Measure-Object pws -Average).Average
        working_set_avg_mib  = & $mib ($samples | Measure-Object ws -Average).Average
        cpu_ms_per_min       = [Math]::Round($cpu / $seconds * 60, 1)
        cpu_percent_one_core = [Math]::Round($cpu / ($seconds * 1000) * 100, 3)
        wakeups_per_sec      = [Math]::Round($cs / $seconds, 1)
        threads              = $Process.Threads.Count
        handles              = $Process.HandleCount
    }
}

# 正常退出会先等采集器停止，而测量实例没有自己的采集器；数据是一次性副本，直接结束即可。
function Stop-Core([Diagnostics.Process] $Process) {
    $Process.Kill()
    $null = $Process.WaitForExit(15000)
}

$phases = @()
$data = New-DataDir
$core = Start-Process -FilePath $Exe -ArgumentList @('--data-dir', "`"$data`"", '--pipe-name', $pipe, '--background') -PassThru
try {
    $null = Wait-Until { [TlWin]::Find($core.Id, 'Timelens.Tray.v1') -ne [IntPtr]::Zero } 30000 '托盘窗口'
    Start-Sleep -Seconds $SettleSeconds
    $phases += Measure-Phase $core 'hidden-never-shown'

    # 第二次启动会把 SHOW 交给已运行的核心，与快捷方式唤起窗口走同一条路径。
    & $Exe --data-dir $data --pipe-name $pipe | Out-Null
    $null = Wait-Until {
        $h = [TlWin]::Find($core.Id, 'Window Class'); $h -ne [IntPtr]::Zero -and [TlWin]::IsWindowVisible($h)
    } 30000 '主窗口显示'
    Start-Sleep -Seconds $SettleSeconds
    $phases += Measure-Phase $core 'visible-idle'

    $main = [TlWin]::Find($core.Id, 'Window Class')
    [void][TlWin]::PostMessage($main, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) # WM_CLOSE
    $null = Wait-Until { -not [TlWin]::IsWindowVisible($main) } 10000 '主窗口隐藏'
    Start-Sleep -Seconds $HiddenSettleSeconds
    $phases += Measure-Phase $core 'hidden-after-shown'
} finally {
    if (-not $core.HasExited) { Stop-Core $core }
}

$startup = for ($run = 0; $run -lt $StartupRuns; $run++) {
    $data = New-DataDir
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $core = Start-Process -FilePath $Exe -ArgumentList @('--data-dir', "`"$data`"", '--pipe-name', $pipe) -PassThru
    try {
        $null = Wait-Until {
            $h = [TlWin]::Find($core.Id, 'Window Class'); $h -ne [IntPtr]::Zero -and [TlWin]::IsWindowVisible($h)
        } 30000 '冷启动窗口'
        $visible = $watch.ElapsedMilliseconds
        Start-Sleep -Seconds 3
        $core.Refresh()
        [pscustomobject]@{
            visible_ms         = $visible
            private_after_3s   = [Math]::Round($core.PrivateMemorySize64 / 1MB, 2)
            peak_working_set   = [Math]::Round($core.PeakWorkingSet64 / 1MB, 2)
        }
    } finally {
        if (-not $core.HasExited) { Stop-Core $core }
    }
}
Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue

$result = [pscustomobject]@{
    label     = $Label
    exe       = $Exe
    sha256    = (Get-FileHash -LiteralPath $Exe -Algorithm SHA256).Hash
    measured  = (Get-Date).ToString('o')
    logical_processors = [Environment]::ProcessorCount
    phases    = $phases
    startup   = $startup
}
$result | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $Out -Encoding utf8
$phases | Format-Table -AutoSize | Out-String -Width 200
$startup | Format-Table -AutoSize | Out-String -Width 200
