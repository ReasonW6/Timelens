# Timelens architecture spikes

> THROWAWAY BENCHMARK CODE. This directory exists only to answer the architecture ticket and must not become production code.

The candidates share one deterministic workload contract so differences are attributable to the application/UI foundation rather than three unrelated implementations.

## Candidates

- `slint-spike`: Rust + windows-rs + Slint
- `tauri-spike`: Rust + windows-rs + Tauri 2 / WebView2
- `winui-spike`: Rust workload driver + WinUI 3 / .NET UI

## Rules

- Release builds only.
- The same generated 10,000 timeline segments and input buckets feed each UI.
- The same workload driver performs window observation, SQLite writes, and one display capture for each candidate run.
- All processes launched for a candidate count toward its CPU and memory totals.
- Results from this host are preliminary. Architecture selection requires the clean Windows 11 VM protocol in `benchmark-contract.md`.
- No code in this directory may be copied into production without a separate implementation review.

## Build and run

From a normal PowerShell window at this directory:

```powershell
.\scripts\build.ps1
.\scripts\measure-matrix.ps1
.\scripts\aggregate-host-results.ps1
```

For the user-approved three-by-60-second local-host decision run:

```powershell
.\scripts\measure-matrix.ps1 -Repetitions 3 -DurationSeconds 60
.\scripts\aggregate-host-results.ps1 -DurationSeconds 60 -OutputPrefix local-controlled
```

Create and verify the comparable installer shells:

```powershell
.\scripts\build-installers.ps1
$packageRoot = Get-ChildItem .\package-output -Directory | Sort-Object Name -Descending | Select-Object -First 1
.\scripts\verify-installers.ps1 -PackageRoot $packageRoot.FullName
```

WPR evidence must be captured from a genuinely elevated PowerShell window inside the clean Windows 11 VM:

```powershell
.\scripts\capture-wpr.ps1 -Candidate slint
.\scripts\capture-wpr.ps1 -Candidate tauri
.\scripts\capture-wpr.ps1 -Candidate winui
```

To capture all three candidates behind one UAC prompt, start `capture-wpr-matrix.ps1` once from an elevated PowerShell window.

See `evidence-report.md` for the current-host results, package verification, provisional ranking, and the remaining clean-VM gate.
