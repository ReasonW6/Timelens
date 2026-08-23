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
One independently switchable, user-facing top-level window belonging to an application. Dialogs, menus, tooltips, tabs, and processes without such a window are not window instances.
_Avoid_: Process, application, dialog, tab

**Application session**:
The logical span from an application's first tracked window opening through its last window or inferred notification-area continuation. Sleep remains inside the span but is excluded from its elapsed-time metrics, and overlapping windows never multiply its duration.
_Avoid_: Process lifetime, summed window time

**Inferred tray continuation**:
A background-only continuation after an application's last window disappears while the originating application remains alive. It ends with that originating application and never guesses a handoff to an unrelated helper.
_Avoid_: Window instance, auxiliary-process activity

**Timeline segment**:
One wall-clock interval that may carry states for several applications at once. Its elapsed time is counted once regardless of how many application states overlap it.
_Avoid_: Summed application time

**Displayed interval**:
A period in which an application has at least one unminimized, unhidden window eligible to appear on a monitor in the current virtual desktop. Several applications may be displayed at once, and a fully occluded window remains displayed because Timelens does not calculate pixel-level occlusion.
_Avoid_: Focus interval, active time

**Focus interval**:
A period in which an application owns the single Windows foreground window that receives keyboard input. Input counts are attributed to this application.
_Avoid_: Displayed interval, foreground time

**Background interval**:
A period in which an application session continues without a displayed window. It includes minimized, notification-area, other-virtual-desktop, and locked-session time, and may overlap other applications' intervals.
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

**Snapshot authorization**:
User consent to send the individually previewed snapshots selected for one manual AI request. It expires with that request and never applies to scheduled summaries.
_Avoid_: Capture permission, permanent consent

**Usage report**:
A persisted deterministic result generated for a user-selected time range, including its calculation version and data coverage. It is a retained derived artifact governed by automatic cleanup, not an authoritative activity fact.
_Avoid_: Export, AI summary

**AI summary**:
A persisted natural-language analysis generated from an AI context snapshot for a scheduled or manually selected time range. Each successful regeneration is a separate summary version with its own conversation branch.
_Avoid_: Usage report

**Manual summary**:
An AI summary explicitly requested for an arbitrary user-selected time range.
_Avoid_: Scheduled summary, usage report

**Daily summary schedule**:
An optional AI schedule that runs at a chosen local time for the preceding complete local calendar date. Its default run time is 22:00.
_Avoid_: Interval summary schedule, manual summary

**Interval summary schedule**:
An optional AI schedule over contiguous, non-overlapping duration windows anchored when the schedule is enabled, with a minimum interval of one hour.
_Avoid_: Daily summary schedule, rolling summary

**Provider profile**:
A saved AI connection choice containing a native or compatible protocol adapter, preset or custom endpoint, credential reference, available models, model capabilities, and selected default model. It is ordinary configuration, not consent to send snapshots.
_Avoid_: Model, AI job, snapshot authorization

**Model capability**:
A provider-reported, built-in, or user-overridden declaration that a model accepts images, exposes reasoning content, or supports another request feature. Unknown capability is distinct from unsupported capability.
_Avoid_: Provider profile, inferred model behavior

**AI job**:
One queued, running, completed, skipped, failed, canceled, or missed attempt to produce an AI summary for an immutable time range and provider-profile snapshot.
_Avoid_: AI summary, schedule, usage report

**Provider configuration snapshot**:
The provider, endpoint host, model, supported parameters, retry policy, and prompt choice fixed for one AI job so later configuration edits do not rewrite its history. It contains no credential.
_Avoid_: Provider profile, API key, model catalog

**Summary version**:
One successful generated answer for a summary range, carrying its provider configuration snapshot, prompt snapshot, AI context snapshot, token usage, and independent conversation branch.
_Avoid_: Failed attempt, conversation branch, usage report

**AI conversation**:
The locally retained message tree for one summary range, including its hidden AI context snapshot, initial answer, and branched follow-up user and assistant messages.
_Avoid_: Usage report, provider profile, raw request log

**AI context snapshot**:
The privacy-filtered structured data envelope encrypted with a summary so later follow-ups can reconstruct their local context without depending on provider-side conversation storage. It excludes prompts, credentials, headers, and raw network requests.
_Avoid_: Local dataset, raw request log, provider conversation ID

**Context compression summary**:
A locally persisted model-generated summary of older messages used to keep an AI conversation within the selected model's context window while the original local message tree remains intact.
_Avoid_: AI summary, deleted conversation, hidden reasoning

**Conversation branch**:
The initial answer for one successful summary version and only the follow-up messages asked from that version. Regeneration creates another branch rather than rewriting or combining prior follow-ups.
_Avoid_: AI summary version, merged chat history

**Exposed reasoning**:
Reasoning content or a reasoning summary explicitly returned by a provider API for display with an assistant message. It never means a hidden chain of thought that the provider did not return.
_Avoid_: Hidden reasoning, progress indicator, reconstructed thought process

**Prompt preset**:
A user-editable, named system-instruction template selected for manual, daily, or interval summaries beneath Timelens's non-editable data and privacy boundary.
_Avoid_: AI data envelope, provider profile, privacy policy

**Prompt snapshot**:
The exact effective editable prompt and template version encrypted with a summary version so its output instructions remain auditable after presets change.
_Avoid_: Prompt preset, hidden safety policy, raw request

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
A compact permanent record of which data classes are present, cleaned, paused, excluded, missing, or corrupt for a time range, allowing reports to disclose coverage without inventing facts.
_Avoid_: Activity record, monitoring gap, cleanup log

**AI data envelope**:
The structured, privacy-filtered facts approved for one AI summary request; it excludes executable paths, icons, internal identities, minute input buckets, excluded application identities, diagnostics, and credentials.
_Avoid_: Local dataset, usage report, prompt log

**Projection**:
A disposable view, index, cache, or aggregate that can be rebuilt from authoritative local-dataset facts without changing their meaning.
_Avoid_: Activity record, AI summary, lifetime counter ledger

**Timelens backup**:
A manually created portable ZIP archive of selected local-dataset classes that excludes AI credentials and diagnostic logs and is restored by replacing, never merging, a local dataset. Password encryption is optional, and restoration never depends on the originating Windows user or device.
_Avoid_: Usage report, Windows-bound archive, cloud sync
