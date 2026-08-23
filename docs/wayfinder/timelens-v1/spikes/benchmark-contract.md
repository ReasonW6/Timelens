# Comparable architecture spike contract

## Decision question

Which application/UI foundation can satisfy Timelens's functional boundary while remaining within the accepted CPU, memory, package-size, and responsiveness budgets?

## Fixed workload

Each candidate run must execute the same operations:

1. Observe top-level window and focus changes through the shared Windows workload driver.
2. Aggregate deterministic mouse and keyboard events into minute buckets without retaining raw order.
3. Create a scratch SQLite database, insert 10,000 timeline segments in one transaction, and query the visible range.
4. Load the same 10,000 segments in the UI and execute the same deterministic pan sequence.
5. Hide to and restore from the notification area once.
6. Capture one display frame and encode one 1280-pixel SDR WebP through the shared workload driver.

## Measurement environment

- Clean 64-bit Windows 11 VM with all updates applied.
- Same VM snapshot, virtual hardware, display resolution, power plan, and WebView/.NET runtime state for every candidate.
- Release configuration; no debugger, profiler, development server, or hot reload.
- Three cold runs and three warm runs per candidate.
- Record OS build, CPU allocation, memory allocation, display configuration, power plan, compiler/runtime versions, and dependency lockfiles.

## Metrics

- Aggregate CPU time and average CPU percentage for the complete process group.
- Context switches/wakeups and disk I/O through WPR/WPA.
- Steady and peak working set, private bytes, and reference set for the complete process group.
- Process count, cold start to first rendered timeline, and deterministic pan-loop duration.
- Release binaries, installer bytes, first-install network download, and installed disk footprint.
- Scratch database and WebP bytes.

## Acceptance budgets

- Idle average CPU no greater than 0.5%.
- Normal collection average CPU no greater than 1%.
- Total resident memory no greater than 100 MB.
- Installer no greater than 20 MB, excluding the Windows-provided WebView2 runtime only.
- Thirty-day non-image data projection no greater than 100 MB.

## Validity rules

- A candidate with missing functionality is not comparable.
- A failed or unavailable capture is recorded as a failure, not replaced with synthetic success.
- Framework-dependent runtime downloads and installed footprint remain visible even when excluded from installer bytes.
- Host measurements may find obvious failures but cannot select the architecture; only the clean-VM repetitions are decision-grade.

## User-approved local-host fallback

On 2026-08-22 the user confirmed that no VM is available and directed the decision to continue on the current host. The fallback may select the V1 foundation only when all candidates are interleaved on the unchanged host with the same Release workload and power plan, three 60-second repetitions are retained, and the report explicitly labels startup and cache results as current-host rather than clean-VM evidence. One elevated WPR GeneralProfile capture per candidate retains raw scheduling and disk evidence; installer results continue to use the already executed identical IExpress shells.
