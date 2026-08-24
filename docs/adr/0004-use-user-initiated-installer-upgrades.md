---
status: accepted
---

# Use user-initiated installer upgrades

Timelens V1 has no code-signing certificate, so it will not install an unattended elevated updater or updater task. Each upgrade is initiated by the user through Inno Setup and requires administrator consent; a package hash delivered beside an unsigned package is not treated as an authenticity boundary. This trades prompt-free upgrades for a smaller privileged surface and avoids silently elevating unauthenticated code.

## Consequences

The installed application remains the ordinary core plus the highest-privilege collector. Runtime startup does not prompt again, but each upgrade does. Automatic elevated updates stay deferred until the project adopts a trustworthy package-authenticity mechanism.
