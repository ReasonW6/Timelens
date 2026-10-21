# Timelens

Timelens is a single-user, local-first Windows activity observatory. This glossary fixes the product language used while its V1 specification is being planned.

## Language

**Activity record**:
A time-bounded observation that an application window was opened, displayed, focused, backgrounded, or closed.
_Avoid_: Usage log, process log, process lifetime

**Application**:
A logical program presented as one Timelens card. It groups all observed window instances that belong to the same program.
_Avoid_: Process, window, executable

**Application identity**:
The stable identity that connects an application's windows and history. Display name, icon, or publisher alone never establishes identity, and a user-confirmed merge may connect identities that Timelens conservatively kept separate.
_Avoid_: Application name, executable name

**Application metadata revision**:
One time-bounded version of an application's permitted display name, executable path, and icon, retained so historical views use the metadata observed at that time.
_Avoid_: Application identity, window title, application merge

**Application merge**:
A user-confirmed rule that treats two previously separate application identities as one application and one history.
_Avoid_: Automatic fuzzy match, rename

**Window instance**:
One independently switchable, user-facing top-level window with a taskbar button, belonging to an application. It ends when the window leaves the taskbar, for example by hiding to the notification area. Dialogs, menus, tooltips, tabs, shell surfaces such as Start and Search, and processes without such a window are not window instances.
_Avoid_: Process, application, dialog, tab

**Application session**:
The logical span from an application's first tracked window opening through its last window or inferred notification-area continuation. Sleep remains inside the span but is excluded from its elapsed-time metrics, and overlapping windows never multiply its duration.
_Avoid_: Process lifetime, summed window time

**Inferred tray continuation**:
A background-only continuation after an application's last window disappears while the originating application remains alive. No longer created: notification-area time is not counted. Continuations recorded earlier remain in history and end with that originating application.
_Avoid_: Window instance, auxiliary-process activity

**Timeline segment**:
One wall-clock interval that may carry states for several applications at once. Its elapsed time is counted once regardless of how many application states overlap it.
_Avoid_: Summed application time

**Displayed interval**:
A period in which an application has at least one unminimized, unhidden window eligible to appear on a monitor in the current virtual desktop. Several applications may be displayed at once, and a fully occluded window remains displayed because Timelens does not calculate pixel-level occlusion.
_Avoid_: Focus interval, active time

**Focus interval**:
A period in which an application owns the single Windows foreground window that receives keyboard input. A minimized foreground window is not focused. Focus is always part of a displayed interval, so focused time never exceeds displayed time. Input counts are attributed to this application.
_Avoid_: Displayed interval, foreground time

**Background interval**:
A period in which an application session continues without a displayed window. It includes minimized, other-virtual-desktop, and locked-session time while the window keeps its taskbar button, and may overlap other applications' intervals. Notification-area time is not background.
_Avoid_: Sleep interval, desktop idle interval

**Desktop idle interval**:
A period in an unlocked session with no displayed ordinary application window on the current virtual desktop. Lack of mouse or keyboard input alone never creates a desktop idle interval.
_Avoid_: Inactivity, away time

**Lock interval**:
A period during which the Windows session is locked.
_Avoid_: Desktop idle interval, sleep interval

**Sleep interval**:
A period bounded by the device entering and resuming from a suspended power state. It is excluded from application displayed, focused, background, and elapsed-time metrics.
_Avoid_: Shutdown, desktop idle interval

**System end marker**:
A point-in-time record that observation ended because Windows shut down or restarted normally; Timelens does not claim to distinguish the two. It does not imply activity while the device was off.
_Avoid_: Shutdown interval, sleep interval

**Monitoring gap**:
An explicit interval from the last durable observation to resumed observation whose activity facts may be incomplete because Timelens could not observe or durably retain them. It reports missing knowledge and never invents or bridges an application state across the interval.
_Avoid_: Desktop idle interval, sleep interval, background interval

**Activity exclusion**:
An application-specific rule that prevents the application's identity and activity records from being retained while leaving an anonymous excluded-activity interval so the time is not misclassified as desktop idle.
_Avoid_: Snapshot exclusion, input exclusion, global pause

**Input count**:
An aggregate count of physical mouse or keyboard events. It never contains reconstructed text, clipboard contents, or other typed content.
_Avoid_: Keystroke log, keylog

**Minute input bucket**:
The per-minute aggregate of physical keyboard presses and left, middle, and right mouse-button presses, optionally attributed to the focused application but never broken down by individual key.
_Avoid_: Raw input event, daily key frequency, lifetime total

**Daily key frequency**:
An application-independent count for each physical keyboard position over one local calendar date, without minute, ordering, or chord information.
_Avoid_: Minute input bucket, typed text, key sequence

**Input exclusion**:
An application-specific rule that reduces input observed while that application is focused to anonymous daily and lifetime totals, without minute, application, or individual-key attribution.
_Avoid_: Activity exclusion, snapshot exclusion

**Collection pause**:
An explicit user-controlled interval during which Timelens retains no application or input observations and creates only paused snapshot omissions. It is known missing activity, not desktop idle or a monitoring failure.
_Avoid_: Monitoring gap, snapshot omission, application exclusion

**Clock discontinuity**:
A point-in-time marker that the Windows wall clock or time-zone interpretation changed while monotonic elapsed-time measurement continued.
_Avoid_: Monitoring gap, system end marker

**Snapshot**:
A periodically captured composite image of one display in the unlocked local Windows session, retained in the local dataset. It is a record of the presented display, not evidence of every application's unobscured window contents.
_Avoid_: Recording, screen recording

**Active display**:
The single display selected for a snapshot: the display containing most of the focused window, then the pointer's display when no window is focused, and finally the primary display as fallback.
_Avoid_: All displays, focused application

**Snapshot slot**:
A scheduled capture opportunity for one target display. It contains either one snapshot or one explicit snapshot omission, never a delayed catch-up image.
_Avoid_: Snapshot, retry

**Snapshot omission**:
A local record that a snapshot slot intentionally or technically produced no image, together with a non-content reason such as pause, idle, lock, remote session, exclusion, or capture failure.
_Avoid_: Snapshot, monitoring gap

**Snapshot exclusion**:
An application-specific rule that suppresses a target display's snapshot while that application is displayed there. It affects neither activity records nor input attribution and is independent of any activity exclusion.
_Avoid_: Activity exclusion, global pause

**Local dataset**:
The authoritative Timelens data stored on the monitored Windows device for one Windows user.
_Avoid_: Account, cloud profile

**Lifetime total**:
An application-independent cumulative input count derived from the lifetime counter ledger and retained across automatic detail cleanup until the user explicitly clears the local dataset.
_Avoid_: Daily aggregate, retained detail

**Lifetime counter ledger**:
The permanent sequence of anonymous daily keyboard and left, middle, and right mouse totals from which lifetime totals can be verified and rebuilt.
_Avoid_: Minute input bucket, daily key frequency, application statistic

**Data availability record**:
A compact permanent record of which data classes are present, cleaned, paused, excluded, missing, or corrupt for a time range, allowing statistics and exports to disclose coverage without inventing facts.
_Avoid_: Activity record, monitoring gap, cleanup log

**Projection**:
A disposable view, index, cache, or aggregate that can be rebuilt from authoritative local-dataset facts without changing their meaning.
_Avoid_: Activity record, lifetime counter ledger

**Timelens backup**:
A manually created portable ZIP archive of selected local-dataset classes that excludes diagnostic logs and is restored by replacing, never merging, a local dataset. Password encryption is optional, and restoration never depends on the originating Windows user or device.
_Avoid_: Export, Windows-bound archive, cloud sync
