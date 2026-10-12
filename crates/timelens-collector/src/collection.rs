use std::{
    path::Path,
    sync::{Arc, Weak},
    time::{Duration, Instant, SystemTime},
};
use timelens_ipc::{
    CollectorEvent, SystemInterval, collector_event,
    privacy::{POLICY_FILE, Policy},
};
use timelens_observer::{
    TrayPresence, TrayTransition, TrayTransitionKind, WindowObservation, WindowTransition,
    WindowTransitionKind,
};
use windows_sys::Win32::System::{
    RemoteDesktop::*,
    WindowsProgramming::{QueryInterruptTimePrecise, QueryUnbiasedInterruptTimePrecise},
};

pub struct Filter {
    pub policy: Policy,
    pub windows: Vec<WindowObservation>,
    pub tray: Vec<TrayPresence>,
    pub blocked: bool,
    session: Option<&'static str>,
    checked: timelens_observer::PollInterval,
    modified: Option<(SystemTime, u64)>,
    system_kind: String,
    system_start: i64,
    system_mono: u64,
    last_utc: i64,
    last_awake: u64,
    last_interrupt: u64,
    last_offset: i32,
    raw_windows: Weak<Vec<WindowObservation>>,
    raw_tray: Weak<Vec<TrayPresence>>,
    policy_changed: bool,
    activity_kind: &'static str,
}
impl Filter {
    pub fn new(directory: &Path, now: i64, mono: u64) -> Self {
        let policy = Policy::load(directory).unwrap_or_else(|_| Policy {
            paused: true,
            ..Default::default()
        });
        Self {
            policy,
            windows: vec![],
            tray: vec![],
            blocked: false,
            session: session_kind(),
            checked: timelens_observer::PollInterval::new(Duration::from_secs(1)),
            modified: None,
            system_kind: "active".into(),
            system_start: now,
            system_mono: mono,
            last_utc: now,
            last_awake: awake_ms(),
            last_interrupt: interrupt_ms(),
            last_offset: 0,
            raw_windows: Weak::new(),
            raw_tray: Weak::new(),
            policy_changed: true,
            activity_kind: "desktop_idle",
        }
    }
    /// The system state still in progress, ended at `now`, for a run that stops here.
    pub fn current_system_interval(&self, now: i64, mono: u64, offset: i32) -> CollectorEvent {
        system(
            &self.system_kind,
            self.system_start.min(now),
            now,
            mono,
            mono.saturating_sub(self.system_mono),
            offset,
        )
    }
    pub fn refresh(&mut self, directory: &Path) -> bool {
        if !self.checked.is_due(Instant::now()) {
            return false;
        }
        self.checked.mark(Instant::now());
        self.session = session_kind();
        let info = std::fs::metadata(directory.join(POLICY_FILE))
            .ok()
            .and_then(|m| m.modified().ok().map(|t| (t, m.len())));
        if self.modified == info {
            return false;
        }
        self.modified = info;
        let policy = Policy::load(directory).unwrap_or_else(|_| Policy {
            paused: true,
            ..Default::default()
        });
        let changed = policy != self.policy;
        self.policy_changed |= changed;
        self.policy = policy;
        changed
    }
    pub fn reconcile(
        &mut self,
        raw: &Arc<Vec<WindowObservation>>,
        tray: &Arc<Vec<TrayPresence>>,
        now: i64,
        mono: u64,
        offset: i32,
    ) -> (
        Vec<WindowTransition>,
        Vec<TrayTransition>,
        Vec<CollectorEvent>,
        bool,
    ) {
        let awake = awake_ms();
        let interrupt = interrupt_ms();
        self.reconcile_at(raw, tray, now, mono, offset, awake, interrupt)
    }

    #[allow(clippy::too_many_arguments)]
    fn reconcile_at(
        &mut self,
        raw: &Arc<Vec<WindowObservation>>,
        tray: &Arc<Vec<TrayPresence>>,
        now: i64,
        mono: u64,
        offset: i32,
        awake: u64,
        interrupt: u64,
    ) -> (
        Vec<WindowTransition>,
        Vec<TrayTransition>,
        Vec<CollectorEvent>,
        bool,
    ) {
        let (sleeping, clock_changed) = clock_gap(
            now.saturating_sub(self.last_utc),
            interrupt.saturating_sub(self.last_interrupt),
            awake.saturating_sub(self.last_awake),
        );
        let discontinuity = clock_changed || self.last_offset != offset && self.last_offset != 0;
        let session = self.session;
        let previously_blocked = self.blocked;
        self.blocked = self.policy.paused || session.is_some();
        let raw_windows = Arc::downgrade(raw);
        let raw_tray = Arc::downgrade(tray);
        let windows_changed = self.policy_changed || !self.raw_windows.ptr_eq(&raw_windows);
        let tray_changed = self.policy_changed || !self.raw_tray.ptr_eq(&raw_tray);
        if windows_changed {
            self.activity_kind = if raw
                .iter()
                .any(|w| w.displayed && self.policy.activity.contains(&w.application_identity))
            {
                "privacy_exclusion"
            } else if raw.iter().any(|w| w.displayed) {
                "active"
            } else {
                "desktop_idle"
            };
        }
        let kind = if self.policy.paused {
            "global_pause"
        } else if let Some(kind) = session {
            kind
        } else {
            self.activity_kind
        };
        let mut systems = vec![];
        if sleeping || discontinuity {
            systems.push(system(
                if sleeping {
                    "sleep"
                } else {
                    "clock_discontinuity"
                },
                self.last_utc.min(now),
                now,
                mono,
                now.saturating_sub(self.last_utc).max(0) as u64,
                offset,
            ));
        }
        if kind != self.system_kind
            || now.saturating_sub(self.system_start) >= 30000
            || sleeping
            || discontinuity
        {
            if !sleeping && !discontinuity {
                systems.push(system(
                    &self.system_kind,
                    self.system_start.min(now),
                    now,
                    mono,
                    mono.saturating_sub(self.system_mono),
                    offset,
                ));
            }
            self.system_kind = kind.into();
            self.system_start = now;
            self.system_mono = mono;
        }
        let refilter = previously_blocked != self.blocked || sleeping || discontinuity;
        let mut transitions = vec![];
        let mut tray_transitions = vec![];
        if windows_changed || refilter {
            let windows = if self.blocked {
                vec![]
            } else {
                raw.iter()
                    .filter(|w| !self.policy.activity.contains(&w.application_identity))
                    .cloned()
                    .collect::<Vec<_>>()
            };
            for old in &self.windows {
                if sleeping || discontinuity || !windows.iter().any(|w| same_window(w, old)) {
                    transitions.push(WindowTransition {
                        kind: WindowTransitionKind::Closed,
                        window: old.clone(),
                        observed_at_utc_ms: if sleeping { self.last_utc } else { now },
                        monotonic_ms: mono,
                    });
                }
            }
            for w in &windows {
                let old = if sleeping || discontinuity {
                    None
                } else {
                    self.windows.iter().find(|old| same_window(w, old))
                };
                if old != Some(w) {
                    transitions.push(WindowTransition {
                        kind: if old.is_some() {
                            WindowTransitionKind::Updated
                        } else {
                            WindowTransitionKind::Opened
                        },
                        window: w.clone(),
                        observed_at_utc_ms: now,
                        monotonic_ms: mono,
                    });
                }
            }
            self.windows = windows;
        }
        let mut tray_state_changed = false;
        if tray_changed || refilter {
            let tray = if self.blocked {
                vec![]
            } else {
                tray.iter()
                    .filter(|w| !self.policy.activity.contains(&w.application_identity))
                    .cloned()
                    .collect::<Vec<_>>()
            };
            for old in &self.tray {
                if !tray.contains(old) || sleeping || discontinuity {
                    tray_transitions.push(TrayTransition {
                        kind: TrayTransitionKind::Ended,
                        observed_at_utc_ms: if sleeping { self.last_utc } else { now },
                        monotonic_ms: mono,
                        application_identity: old.application_identity.clone(),
                        process_id: old.process_id,
                        process_started_at_100ns: old.process_started_at_100ns,
                    });
                }
            }
            for w in &tray {
                if !self.tray.contains(w) || sleeping || discontinuity {
                    tray_transitions.push(TrayTransition {
                        kind: TrayTransitionKind::Started,
                        observed_at_utc_ms: now,
                        monotonic_ms: mono,
                        application_identity: w.application_identity.clone(),
                        process_id: w.process_id,
                        process_started_at_100ns: w.process_started_at_100ns,
                    });
                }
            }
            tray_state_changed = self.tray != tray;
            self.tray = tray;
        }
        self.raw_windows = raw_windows;
        self.raw_tray = raw_tray;
        self.policy_changed = false;
        self.last_utc = now;
        self.last_awake = awake;
        self.last_interrupt = interrupt;
        self.last_offset = offset;
        (transitions, tray_transitions, systems, tray_state_changed)
    }
}
fn same_window(a: &WindowObservation, b: &WindowObservation) -> bool {
    a.window_id == b.window_id
        && a.process_id == b.process_id
        && a.process_started_at_100ns == b.process_started_at_100ns
        && a.application_identity == b.application_identity
}
fn system(
    kind: &str,
    start: i64,
    end: i64,
    mono: u64,
    duration: u64,
    offset: i32,
) -> CollectorEvent {
    CollectorEvent {
        observed_at_utc_ms: end,
        monotonic_ms: mono,
        body: Some(collector_event::Body::SystemInterval(SystemInterval {
            kind: kind.into(),
            started_utc_ms: start,
            duration_ms: duration,
            timezone_offset_minutes: offset,
        })),
    }
}
fn awake_ms() -> u64 {
    let mut n = 0;
    unsafe { QueryUnbiasedInterruptTimePrecise(&mut n) };
    n / 10000
}
fn interrupt_ms() -> u64 {
    let mut n = 0;
    unsafe { QueryInterruptTimePrecise(&mut n) };
    n / 10000
}
fn clock_gap(utc_elapsed: i64, interrupt_elapsed: u64, awake_elapsed: u64) -> (bool, bool) {
    (
        interrupt_elapsed.saturating_sub(awake_elapsed) > 5000,
        utc_elapsed
            .saturating_sub(interrupt_elapsed.min(i64::MAX as u64) as i64)
            .unsigned_abs()
            > 2000,
    )
}
fn session_kind() -> Option<&'static str> {
    unsafe {
        let mut data = std::ptr::null_mut();
        let mut len = 0;
        if WTSQuerySessionInformationW(
            std::ptr::null_mut(),
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut data,
            &mut len,
        ) == 0
        {
            return Some("secure_desktop");
        }
        let result = if len as usize >= std::mem::size_of::<WTSINFOEXW>() {
            let info = std::ptr::read_unaligned(data.cast::<WTSINFOEXW>());
            if info.Level == 1 {
                let s = info.Data.WTSInfoExLevel1;
                if s.SessionState != WTSActive {
                    Some("session_disconnected")
                } else if s.SessionFlags == WTS_SESSIONSTATE_LOCK as i32 {
                    Some("locked")
                } else {
                    None
                }
            } else {
                Some("secure_desktop")
            }
        } else {
            Some("secure_desktop")
        };
        WTSFreeMemory(data.cast());
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> WindowObservation {
        WindowObservation {
            window_id: 10,
            process_id: 20,
            process_started_at_100ns: 30,
            application_identity: "fixture-app".into(),
            identity_source: timelens_observer::IdentitySource::ExecutablePath,
            executable_path: Some("C:\\Fixture\\app.exe".into()),
            app_user_model_id: None,
            package_identity: None,
            displayed: true,
            focused: true,
            on_current_virtual_desktop: Some(true),
            virtual_desktop_id: None,
        }
    }

    fn filter(directory: &Path) -> Filter {
        let mut filter = Filter::new(directory, 0, 0);
        filter.session = None;
        filter.last_awake = 0;
        filter.last_interrupt = 0;
        filter
    }

    #[test]
    fn unchanged_sources_reuse_filtered_storage_and_keep_system_heartbeats() {
        let directory = tempfile::tempdir().unwrap();
        let mut filter = filter(directory.path());
        let raw = Arc::new(vec![window()]);
        let tray = Arc::new(vec![TrayPresence {
            application_identity: "fixture-tray".into(),
            process_id: 21,
            process_started_at_100ns: 31,
        }]);
        let first = filter.reconcile_at(&raw, &tray, 0, 0, 60, 0, 0);
        assert_eq!((first.0.len(), first.1.len(), first.3), (1, 1, true));
        let window_buffer = filter.windows.as_ptr();
        let identity_buffer = filter.windows[0].application_identity.as_ptr();
        let tray_buffer = filter.tray.as_ptr();
        for now in [1_000, 29_999, 30_000, 31_000] {
            let (windows, tray_events, systems, changed) =
                filter.reconcile_at(&raw, &tray, now, now as u64, 60, now as u64, now as u64);
            assert!(windows.is_empty() && tray_events.is_empty() && !changed);
            assert_eq!(systems.len(), usize::from(now == 30_000));
            assert_eq!(filter.windows.as_ptr(), window_buffer);
            assert_eq!(
                filter.windows[0].application_identity.as_ptr(),
                identity_buffer
            );
            assert_eq!(filter.tray.as_ptr(), tray_buffer);
        }
        assert_eq!(Arc::strong_count(&raw), 1);
        assert_eq!(Arc::strong_count(&tray), 1);
    }

    #[test]
    fn encrypted_policy_refresh_and_unreadable_policy_invalidate_cached_results() {
        let directory = tempfile::tempdir().unwrap();
        let mut filter = filter(directory.path());
        let raw = Arc::new(vec![window()]);
        let tray = Arc::new(vec![]);
        filter.reconcile_at(&raw, &tray, 0, 0, 60, 0, 0);
        let mut policy = Policy::default();
        policy.activity.insert("fixture-app".into());
        policy.save(directory.path()).unwrap();
        filter.checked.reset();
        assert!(filter.refresh(directory.path()));
        filter.session = None;
        assert_eq!(
            filter
                .reconcile_at(&raw, &tray, 1_000, 1_000, 60, 1_000, 1_000)
                .0[0]
                .kind,
            WindowTransitionKind::Closed
        );
        std::fs::write(directory.path().join(POLICY_FILE), b"unreadable").unwrap();
        filter.checked.reset();
        assert!(filter.refresh(directory.path()));
        filter.reconcile_at(&raw, &tray, 2_000, 2_000, 60, 2_000, 2_000);
        assert!(filter.policy.paused && filter.blocked);
        assert!(filter.windows.is_empty());
        assert_eq!(filter.system_kind, "global_pause");
    }

    #[test]
    fn policy_and_session_changes_invalidate_unchanged_sources() {
        let directory = tempfile::tempdir().unwrap();
        let mut filter = filter(directory.path());
        let raw = Arc::new(vec![window()]);
        let tray = Arc::new(vec![]);
        filter.reconcile_at(&raw, &tray, 0, 0, 60, 0, 0);
        filter.policy.activity.insert("fixture-app".into());
        filter.policy_changed = true;
        let result = filter.reconcile_at(&raw, &tray, 1_000, 1_000, 60, 1_000, 1_000);
        assert_eq!(result.0[0].kind, WindowTransitionKind::Closed);
        assert_eq!(filter.system_kind, "privacy_exclusion");
        assert!(filter.windows.is_empty());
        filter.policy.activity.clear();
        filter.policy_changed = true;
        assert_eq!(
            filter
                .reconcile_at(&raw, &tray, 2_000, 2_000, 60, 2_000, 2_000)
                .0[0]
                .kind,
            WindowTransitionKind::Opened
        );
        filter.session = Some("locked");
        assert_eq!(
            filter
                .reconcile_at(&raw, &tray, 3_000, 3_000, 60, 3_000, 3_000)
                .0[0]
                .kind,
            WindowTransitionKind::Closed
        );
        assert!(filter.blocked);
        filter.session = None;
        assert_eq!(
            filter
                .reconcile_at(&raw, &tray, 4_000, 4_000, 60, 4_000, 4_000)
                .0[0]
                .kind,
            WindowTransitionKind::Opened
        );
        assert!(!filter.blocked);
    }

    #[test]
    fn changed_sources_and_clock_gaps_still_publish_transitions() {
        let directory = tempfile::tempdir().unwrap();
        let mut filter = filter(directory.path());
        let raw = Arc::new(vec![window()]);
        let tray = Arc::new(vec![]);
        filter.reconcile_at(&raw, &tray, 0, 0, 60, 0, 0);
        let updated = Arc::new(vec![WindowObservation {
            focused: false,
            ..window()
        }]);
        assert_eq!(
            filter
                .reconcile_at(&updated, &tray, 1_000, 1_000, 60, 1_000, 1_000)
                .0[0]
                .kind,
            WindowTransitionKind::Updated
        );
        let result = filter.reconcile_at(&updated, &tray, 21_000, 21_000, 60, 2_000, 21_000);
        assert_eq!(result.0.len(), 2);
        assert_eq!(result.0[0].kind, WindowTransitionKind::Closed);
        assert_eq!(result.0[0].observed_at_utc_ms, 1_000);
        assert_eq!(result.0[1].kind, WindowTransitionKind::Opened);
        assert!(matches!(&result.2[0].body,
            Some(collector_event::Body::SystemInterval(interval)) if interval.kind == "sleep"));
        let result = filter.reconcile_at(&updated, &tray, 22_000, 22_000, 120, 3_000, 22_000);
        assert_eq!(result.0.len(), 2);
        assert!(matches!(&result.2[0].body,
            Some(collector_event::Body::SystemInterval(interval)) if interval.kind == "clock_discontinuity"));
    }

    #[test]
    fn cached_sources_do_not_keep_retired_window_payloads_alive() {
        let directory = tempfile::tempdir().unwrap();
        let mut filter = filter(directory.path());
        let raw = Arc::new(vec![window()]);
        let tray = Arc::new(vec![]);
        filter.reconcile_at(&raw, &tray, 0, 0, 60, 0, 0);
        let retired = Arc::downgrade(&raw);
        drop(raw);
        assert!(retired.upgrade().is_none());
        let reused = Arc::new(vec![WindowObservation {
            process_started_at_100ns: 40,
            ..window()
        }]);
        let result = filter.reconcile_at(&reused, &tray, 1_000, 1_000, 60, 1_000, 1_000);
        assert_eq!(result.0.len(), 2);
        assert_eq!(filter.windows[0].process_started_at_100ns, 40);
    }
    #[test]
    fn the_system_state_in_progress_can_end_a_run() {
        let directory = tempfile::tempdir().unwrap();
        let filter = Filter::new(directory.path(), 1_000, 5);
        let event = filter.current_system_interval(21_000, 25, 60);
        assert_eq!((event.observed_at_utc_ms, event.monotonic_ms), (21_000, 25));
        let Some(collector_event::Body::SystemInterval(interval)) = event.body else {
            panic!("expected a system interval");
        };
        assert_eq!(interval.kind, "active");
        assert_eq!(interval.started_utc_ms, 1_000);
        assert_eq!(interval.duration_ms, 20);
        assert_eq!(interval.timezone_offset_minutes, 60);
    }
    #[test]
    fn wall_clock_edits_are_distinct_from_measured_suspend_time() {
        assert_eq!(clock_gap(21_000, 1_000, 1_000), (false, true));
        assert_eq!(clock_gap(-19_000, 1_000, 1_000), (false, true));
        assert_eq!(clock_gap(21_000, 21_000, 1_000), (true, false));
        assert_eq!(clock_gap(41_000, 21_000, 1_000), (true, true));
        assert_eq!(clock_gap(1_000, 1_000, 1_000), (false, false));
    }
    #[test]
    fn typed_exclusions_do_not_leak_other_rule_categories() {
        let mut p = Policy::default();
        p.activity.insert("private-app".into());
        assert!(!p.input.contains("private-app"));
        assert!(!p.paused);
        assert!(p.validate().is_ok());
    }
}
