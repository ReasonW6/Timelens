# Wallpaper Engine-style elevated startup

## What Wallpaper Engine actually does

Wallpaper Engine's official FAQ says its normal startup option launches the app quietly in the background with Windows, while the **high-priority** option registers a **Windows service**, which starts earlier than ordinary startup apps. The same page warns that this mode can be less reliable because antivirus products may block it. It also says that forcing Wallpaper Engine executables to "run as administrator" can prevent automatic startup from working. Therefore, the documented Wallpaper Engine mechanism is a service, not a highest-privilege Scheduled Task, and it is not evidence that every interactive Wallpaper Engine process runs elevated. ([Wallpaper Engine official FAQ](https://help.wallpaperengine.io/en/functionality/automaticstartup.html))

Wallpaper Engine does not publicly document the service account, process token layout, launcher/watchdog behavior, or IPC design. Those details remain uncertain and should not be copied by assumption.

## Windows constraints relevant to Timelens

- Installing a Windows service is an administrative operation: Microsoft says only administrator-privileged processes can obtain the Service Control Manager access needed to create a service. This supports a one-time UAC consent during installation or privilege-component setup, rather than a prompt at every sign-in. ([Microsoft: Service Security and Access Rights](https://learn.microsoft.com/en-us/windows/win32/services/service-security-and-access-rights))
- A service cannot directly act as Timelens's interactive UI or desktop sensor. Since Windows Vista, services cannot directly interact with users; services run in session 0, and Microsoft recommends a separate process in the signed-in user's session communicating with the service through secured IPC such as an ACL-restricted named pipe. ([Microsoft: Interactive Services](https://learn.microsoft.com/en-us/windows/win32/services/interactive-services))
- Marking the whole application `requireAdministrator` would produce a UAC consent/credential prompt before each ordinary launch. Microsoft explicitly recommends keeping the main app at `asInvoker` and separating only the operations that require elevation, because unnecessary elevation expands attack surface. ([Microsoft: Running with Administrator Privileges](https://learn.microsoft.com/en-us/windows/win32/secbp/running-with-administrator-privileges))
- If a service is used, it should run with the minimum account and privileges that satisfy its actual duties. `LocalService` has minimal local privileges and anonymous network credentials; Microsoft says `LocalSystem` should be used only when administrative or operating-system-level rights are genuinely necessary. ([Microsoft: LocalService Account](https://learn.microsoft.com/en-us/windows/win32/services/localservice-account), [Microsoft: service logon account guidance](https://learn.microsoft.com/en-us/windows/win32/ad/guidelines-for-selecting-a-service-logon-account))
- Windows Task Scheduler is a valid alternative for a narrow collector that must run elevated inside the signed-in user's session: a task registered by an elevated installer can use `TASK_RUNLEVEL_HIGHEST`, and Task Scheduler then runs it at that stored privilege level. The no-repeat-prompt behavior is an inference from this documented execution model, not a statement about Wallpaper Engine. ([Microsoft: Security Contexts for Tasks](https://learn.microsoft.com/en-us/windows/win32/taskschd/security-contexts-for-running-tasks))

## Recommendation for Timelens

Choose **Q8 = C: privilege separation**.

For V1, use an installer that requests UAC once and registers a **narrow elevated collector as a highest-privilege, at-logon Scheduled Task**. Keep the tray/UI, timeline renderer, settings, report viewer, AI client, and all network-facing code as ordinary `asInvoker` processes. This gives the requested no-UAC-on-every-start UX while keeping desktop/input collection in the interactive user session and avoiding elevation of the much larger UI/AI surface.

Use authenticated, ACL-restricted local IPC between the collector and UI. The elevated collector should accept only a small fixed command set and should not execute arbitrary paths, shell commands, SQL, or model-generated input. Updates, task changes, repair, and uninstall may legitimately require UAC again.

Do **not** add a Windows service in V1 merely to imitate Wallpaper Engine. A service becomes justified later only if Timelens needs pre-login lifecycle markers, cross-session coordination, or watchdog/self-recovery behavior. If added, it should remain a minimal broker and still use a separate user-session sensor because session-0 services cannot directly capture or interact with the user's desktop.

### Residual uncertainty to verify in a prototype

The elevated collector should be tested against ordinary apps, elevated apps, UAC secure-desktop transitions, lock/unlock, sleep/resume, and multiple signed-in sessions. The available official Wallpaper Engine documentation does not establish that its full application runs elevated, and it does not disclose an implementation that Timelens can safely reproduce byte-for-byte.
