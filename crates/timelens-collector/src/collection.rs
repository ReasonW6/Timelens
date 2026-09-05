use std::{
    path::Path,
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
    checked: Instant,
    modified: Option<(SystemTime, u64)>,
    system_kind: String,
    system_start: i64,
    system_mono: u64,
    last_utc: i64,
    last_awake: u64,
    last_interrupt: u64,
    last_offset: i32,
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
            checked: Instant::now() - Duration::from_secs(2),
            modified: None,
            system_kind: "active".into(),
            system_start: now,
            system_mono: mono,
            last_utc: now,
            last_awake: awake_ms(),
            last_interrupt: interrupt_ms(),
            last_offset: 0,
        }
    }
    pub fn refresh(&mut self, directory: &Path) -> bool {
        if self.checked.elapsed() < Duration::from_secs(1) {
            return false;
        }
        self.checked = Instant::now();
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
        self.policy = policy;
        changed
    }
    pub fn reconcile(
        &mut self,
        raw: &[WindowObservation],
        tray: &[TrayPresence],
        now: i64,
        mono: u64,
        offset: i32,
    ) -> (
        Vec<WindowTransition>,
        Vec<TrayTransition>,
        Vec<CollectorEvent>,
    ) {
        let awake = awake_ms();
        let interrupt = interrupt_ms();
        let (sleeping, clock_changed) = clock_gap(
            now.saturating_sub(self.last_utc),
            interrupt.saturating_sub(self.last_interrupt),
            awake.saturating_sub(self.last_awake),
        );
        let discontinuity = clock_changed || self.last_offset != offset && self.last_offset != 0;
        let session = self.session;
        self.blocked = self.policy.paused || session.is_some();
        let excluded = raw
            .iter()
            .any(|w| w.displayed && self.policy.activity.contains(&w.application_identity));
        let kind = if self.policy.paused {
            "global_pause"
        } else if let Some(kind) = session {
            kind
        } else if excluded {
            "privacy_exclusion"
        } else if !raw.iter().any(|w| w.displayed) {
            "desktop_idle"
        } else {
            "active"
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
        let windows = if self.blocked {
            vec![]
        } else {
            raw.iter()
                .filter(|w| !self.policy.activity.contains(&w.application_identity))
                .cloned()
                .collect::<Vec<_>>()
        };
        let tray = if self.blocked {
            vec![]
        } else {
            tray.iter()
                .filter(|w| !self.policy.activity.contains(&w.application_identity))
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut transitions = vec![];
        let mut tray_transitions = vec![];
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
        self.windows = windows;
        self.tray = tray;
        self.last_utc = now;
        self.last_awake = awake;
        self.last_interrupt = interrupt;
        self.last_offset = offset;
        (transitions, tray_transitions, systems)
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
