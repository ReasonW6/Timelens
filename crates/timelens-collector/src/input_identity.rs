use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use timelens_observer::{InputSample, WindowObservation, WindowTransition, WindowTransitionKind};

type Key = (u64, u32);
#[derive(Clone)]
enum Attribution {
    Known(Arc<str>),
    Ambiguous(Vec<Arc<str>>),
}

struct WindowIdentity {
    identity: Arc<str>,
    process_started_at_100ns: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct ResolvedInput<'a> {
    pub identity: Option<&'a Arc<str>>,
    pub anonymous_only: bool,
}

/// Only live windows persist. Previous versions survive one input drain.
#[derive(Default)]
pub(crate) struct InputIdentities {
    current: HashMap<Key, WindowIdentity>,
    retired: HashMap<Key, Attribution>,
}

impl InputIdentities {
    pub(crate) fn from_windows(windows: &[WindowObservation]) -> Self {
        Self {
            current: windows
                .iter()
                .map(|window| {
                    (
                        (window.window_id, window.process_id),
                        WindowIdentity {
                            identity: Arc::from(window.application_identity.as_str()),
                            process_started_at_100ns: window.process_started_at_100ns,
                        },
                    )
                })
                .collect(),
            retired: HashMap::new(),
        }
    }

    fn retire(&mut self, key: Key, identity: Arc<str>) {
        use std::collections::hash_map::Entry;
        match self.retired.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(Attribution::Known(identity));
            }
            Entry::Occupied(mut entry) => {
                let value = entry.get_mut();
                match value {
                    Attribution::Known(old) if *old != identity => {
                        *value = Attribution::Ambiguous(vec![Arc::clone(old), identity]);
                    }
                    Attribution::Ambiguous(versions) if !versions.contains(&identity) => {
                        versions.push(identity)
                    }
                    _ => {}
                }
            }
        }
    }

    fn upsert(&mut self, window: &WindowObservation) {
        let key = (window.window_id, window.process_id);
        if self.current.get(&key).is_some_and(|old| {
            old.identity.as_ref() == window.application_identity
                && old.process_started_at_100ns == window.process_started_at_100ns
        }) {
            return;
        }
        let identity = self
            .current
            .get(&key)
            .filter(|old| old.identity.as_ref() == window.application_identity)
            .map_or_else(
                || Arc::from(window.application_identity.as_str()),
                |old| Arc::clone(&old.identity),
            );
        if let Some(old) = self.current.insert(
            key,
            WindowIdentity {
                identity: Arc::clone(&identity),
                process_started_at_100ns: window.process_started_at_100ns,
            },
        ) {
            self.retire(key, old.identity);
        }
        if self.retired.contains_key(&key) {
            self.retire(key, identity);
        }
    }

    pub(crate) fn apply_transitions(&mut self, transitions: &[WindowTransition]) {
        for transition in transitions {
            let window = &transition.window;
            let key = (window.window_id, window.process_id);
            match transition.kind {
                WindowTransitionKind::Opened | WindowTransitionKind::Updated => self.upsert(window),
                WindowTransitionKind::Closed => {
                    let matches = self.current.get(&key).is_some_and(|old| {
                        old.identity.as_ref() == window.application_identity
                            && old.process_started_at_100ns == window.process_started_at_100ns
                    });
                    let old = if matches {
                        self.current.remove(&key).unwrap().identity
                    } else {
                        Arc::from(window.application_identity.as_str())
                    };
                    self.retire(key, old);
                    if let Some(current) = self.current.get(&key) {
                        self.retire(key, Arc::clone(&current.identity));
                    }
                }
            }
        }
    }

    pub(crate) fn reconcile(&mut self, windows: &[WindowObservation]) {
        let mut unseen = self.current.keys().copied().collect::<HashSet<_>>();
        for window in windows {
            unseen.remove(&(window.window_id, window.process_id));
            self.upsert(window);
        }
        for key in unseen {
            if let Some(old) = self.current.remove(&key) {
                self.retire(key, old.identity);
            }
        }
    }

    fn known(&self, sample: &InputSample) -> Option<&Arc<str>> {
        let key = (sample.window_id, sample.window_process_id);
        match self.retired.get(&key) {
            Some(Attribution::Known(identity)) => Some(identity),
            Some(Attribution::Ambiguous(_)) => None,
            None => self.current.get(&key).map(|window| &window.identity),
        }
    }

    pub(crate) fn resolve(
        &self,
        sample: &InputSample,
        policy: &timelens_ipc::privacy::Policy,
    ) -> ResolvedInput<'_> {
        if let Some(identity) = self.known(sample) {
            let anonymous_only = policy.input.contains(identity.as_ref());
            ResolvedInput {
                identity: (!anonymous_only && !policy.activity.contains(identity.as_ref()))
                    .then_some(identity),
                anonymous_only,
            }
        } else {
            ResolvedInput { identity: None, anonymous_only: self.retired.get(&(sample.window_id, sample.window_process_id))
                .is_some_and(|retired| matches!(retired, Attribution::Ambiguous(versions) if versions.iter().any(|id| policy.input.contains(id.as_ref())))) }
        }
    }

    #[cfg(test)]
    pub(crate) fn identity(&self, sample: &InputSample) -> Option<&str> {
        self.known(sample).map(AsRef::as_ref)
    }

    #[cfg(test)]
    pub(crate) fn anonymous_only(
        &self,
        sample: &InputSample,
        policy: &timelens_ipc::privacy::Policy,
    ) -> bool {
        self.resolve(sample, policy).anonymous_only
    }

    pub(crate) fn settled(&mut self) {
        if !self.retired.is_empty() {
            self.retired = HashMap::new();
        }
        if self.current.capacity() > self.current.len().saturating_mul(4).max(64) {
            self.current.shrink_to(self.current.len().saturating_mul(2));
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(identities: &HashMap<u64, String>) -> Self {
        Self {
            current: identities
                .iter()
                .map(|(&window, identity)| {
                    (
                        (window, 0),
                        WindowIdentity {
                            identity: Arc::from(identity.as_str()),
                            process_started_at_100ns: 0,
                        },
                    )
                })
                .collect(),
            retired: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use timelens_observer::{IdentitySource, InputSampleKind, MouseButton};

    fn window(pid: u32, name: &str) -> WindowObservation {
        WindowObservation {
            window_id: 10,
            process_id: pid,
            process_started_at_100ns: u64::from(pid),
            identity_source: IdentitySource::ExecutablePath,
            application_identity: name.into(),
            executable_path: None,
            app_user_model_id: None,
            package_identity: None,
            displayed: true,
            focused: true,
            on_current_virtual_desktop: Some(true),
            virtual_desktop_id: None,
        }
    }
    fn sample(pid: u32) -> InputSample {
        InputSample {
            minute_started_at_utc_ms: 1_700_000_000_000,
            window_id: 10,
            window_process_id: pid,
            kind: InputSampleKind::Mouse(MouseButton::Left),
            count: 1,
        }
    }

    #[test]
    fn closed_versions_survive_the_drain_and_reused_handles_keep_their_process() {
        let mut ids = InputIdentities::from_windows(&[window(1, "old")]);
        ids.reconcile(&[window(2, "new")]);
        assert_eq!(ids.identity(&sample(1)), Some("old"));
        assert_eq!(ids.identity(&sample(2)), Some("new"));
        ids.settled();
        assert_eq!(ids.identity(&sample(1)), None);
        assert_eq!(ids.identity(&sample(2)), Some("new"));
        for pid in 3..1000 {
            ids.reconcile(&[window(pid, "new")]);
            ids.settled();
            assert_eq!(ids.current.len(), 1);
            assert!(ids.retired.is_empty());
        }
    }

    #[test]
    fn ambiguous_versions_do_not_guess_identity_or_expose_excluded_keys() {
        let mut ids = InputIdentities::from_windows(&[window(1, "old")]);
        ids.reconcile(&[window(1, "new")]);
        let mut policy = timelens_ipc::privacy::Policy::default();
        policy.input.insert("old".into());
        assert_eq!(ids.identity(&sample(1)), None);
        assert!(ids.anonymous_only(&sample(1), &policy));
        ids.settled();
        assert_eq!(ids.identity(&sample(1)), Some("new"));
        assert!(!ids.anonymous_only(&sample(1), &policy));
    }

    fn transition(kind: WindowTransitionKind, window: WindowObservation) -> WindowTransition {
        WindowTransition {
            kind,
            window,
            observed_at_utc_ms: 1_700_000_000_000,
            monotonic_ms: 1,
        }
    }

    #[test]
    fn focus_and_visibility_updates_keep_the_same_identity_allocation() {
        let original = window(1, "app");
        let mut ids = InputIdentities::from_windows(std::slice::from_ref(&original));
        let identity = Arc::clone(&ids.current[&(10, 1)].identity);
        let mut updated = original;
        updated.focused = false;
        updated.displayed = false;
        ids.apply_transitions(&[transition(WindowTransitionKind::Updated, updated.clone())]);
        ids.reconcile(&[updated]);
        assert!(Arc::ptr_eq(&identity, &ids.current[&(10, 1)].identity));
        assert!(ids.retired.is_empty());
        assert_eq!(Arc::strong_count(&identity), 2);
    }

    #[test]
    fn multiple_versions_and_late_close_keep_all_input_exclusions() {
        let mut ids = InputIdentities::from_windows(&[window(1, "first")]);
        let mut last = window(1, "last");
        last.process_started_at_100ns = 20;
        ids.apply_transitions(&[
            transition(WindowTransitionKind::Updated, window(1, "private")),
            transition(WindowTransitionKind::Opened, last.clone()),
            transition(WindowTransitionKind::Closed, window(1, "first")),
        ]);
        let mut policy = timelens_ipc::privacy::Policy::default();
        policy.input.insert("private".into());
        assert_eq!(ids.identity(&sample(1)), None);
        assert!(ids.resolve(&sample(1), &policy).anonymous_only);
        assert_eq!(ids.current[&(10, 1)].identity.as_ref(), "last");
        ids.settled();
        assert_eq!(ids.identity(&sample(1)), Some("last"));
        assert!(!ids.resolve(&sample(1), &policy).anonymous_only);
    }

    #[test]
    fn a_large_closed_set_releases_retired_entries_and_table_capacity() {
        let windows = (0..1000)
            .map(|pid| {
                let mut value = window(pid, "app");
                value.window_id = u64::from(pid) + 10;
                value
            })
            .collect::<Vec<_>>();
        let mut ids = InputIdentities::from_windows(&windows);
        ids.reconcile(&windows[..1]);
        assert_eq!(ids.retired.len(), 999);
        ids.settled();
        assert!(ids.retired.is_empty());
        assert_eq!(ids.retired.capacity(), 0);
        assert!(ids.current.capacity() <= 64);
    }
}
