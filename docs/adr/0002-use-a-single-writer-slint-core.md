---
status: accepted
---

# Use a single-writer Slint core with a bounded collector spool

Timelens V1 uses one normal-privilege Rust, windows-rs, and Slint core as its tray process, UI host, query coordinator, and exclusive SQLite WAL writer. A separately elevated collector sends versioned, length-bounded Protobuf event batches over a mutually authenticated local named pipe and retains unacknowledged aggregates in a 32 MiB checksummed ring spool; overflow preserves the newest observations and creates an explicit monitoring gap. Authoritative activity intervals, application sessions, minute input buckets, and system states are stored directly, while totals and timeline projections remain rebuildable.

## Considered Options

A separate headless core plus UI was rejected because the elevated collector already supplies the necessary crash boundary and another resident process would add memory and IPC. Shared database access, file exchange, and localhost HTTP were rejected because they weaken single-writer ownership or enlarge the privileged protocol. Permanent low-level event sourcing was rejected because it retains unnecessary event detail and increases storage cost; summary-only storage was rejected because reports and projections could not be repaired.

## Consequences

`Timelens.exe` is a single normal per-user instance with an asynchronous SQLite writer and typed UI queries. Snapshot capture runs on a bounded worker thread, while AI work runs as an on-demand normal child process. `Timelens.Collector.exe` has no database, UI, update, AI, or network capability. The normal core and elevated collector start through separate logon tasks, restart after crashes, and both stop on an explicit user exit. Only one Windows session for a user may own the local dataset at a time.
