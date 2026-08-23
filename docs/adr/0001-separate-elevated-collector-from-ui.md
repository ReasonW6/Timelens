# Separate the elevated collector from the UI

Timelens will request administrator consent during installation and register a narrow collector to start at user logon with the required elevated rights, while its tray UI, timeline, reports, and AI client run without elevation. This preserves the requested no-recurring-UAC experience and access to elevated application windows without giving the much larger interactive and network-facing surface administrator authority.
