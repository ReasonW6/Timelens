---
status: accepted
---

# Use an offline elevated update broker

Timelens installs `Timelens.Updater.exe` and a highest-privilege on-demand task during the initial UAC-approved installation so later signed updates do not prompt again. The normal core alone checks the network and downloads into a fixed staging directory; the narrow updater has no networking and accepts no arbitrary command, URL, or destination, applying only a non-downgrade package whose Authenticode publisher, manifest hash, version, and fixed installation root pass validation.

## Considered Options

Prompting for UAC on every upgrade was rejected because the product requirement is no UAC after initial installation. A permanently running privileged updater and allowing the elevated collector to download or replace programs were rejected because either would add a network-facing administrator surface. MSIX was rejected for V1 because it conflicts with the selected custom installation path and highest-privilege task shape.

## Consequences

Inno Setup provides the initial selectable fixed-drive installation. Automatic updates apply only while the UI is hidden and no AI job is active; the collector buffers during replacement. The previous signed package is retained until the new core passes database-open, IPC, and tray health checks, then removed by the updater's bounded cleanup policy. Failure restores the previous version and reports locally. An irreversible database migration cannot install silently and requires a user-confirmed backup path.
