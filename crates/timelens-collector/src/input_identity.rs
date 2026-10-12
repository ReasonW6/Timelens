use std::collections::HashMap;
use timelens_observer::{InputSample, WindowObservation};

type Key = (u64, u32);
#[derive(Clone)]
enum Attribution {
    Known(String),
    Ambiguous(String, String),
}

/// Only live windows persist. Previous versions survive one input drain.
#[derive(Default)]
pub(crate) struct InputIdentities {
    current: HashMap<Key, Attribution>,
    pending: Option<HashMap<Key, Attribution>>,
}

impl InputIdentities {
    pub(crate) fn from_windows(windows: &[WindowObservation]) -> Self {
        Self {
            current: windows
                .iter()
                .map(|window| {
                    (
                        (window.window_id, window.process_id),
                        Attribution::Known(window.application_identity.clone()),
                    )
                })
                .collect(),
            pending: None,
        }
    }

    pub(crate) fn refresh(&mut self, windows: &[WindowObservation]) {
        let next = Self::from_windows(windows).current;
        let mut pending = std::mem::replace(&mut self.current, next);
        for (&key, value) in &self.current {
            let combined = match (pending.get(&key), value) {
                (Some(Attribution::Known(old)), Attribution::Known(new)) if old != new => {
                    Attribution::Ambiguous(old.clone(), new.clone())
                }
                _ => value.clone(),
            };
            pending.insert(key, combined);
        }
        self.pending = Some(pending);
    }

    fn attribution(&self, sample: &InputSample) -> Option<&Attribution> {
        self.pending
            .as_ref()
            .unwrap_or(&self.current)
            .get(&(sample.window_id, sample.window_process_id))
    }

    pub(crate) fn identity(&self, sample: &InputSample) -> Option<&str> {
        match self.attribution(sample) {
            Some(Attribution::Known(identity)) => Some(identity),
            _ => None,
        }
    }

    pub(crate) fn anonymous_only(
        &self,
        sample: &InputSample,
        policy: &timelens_ipc::privacy::Policy,
    ) -> bool {
        match self.attribution(sample) {
            Some(Attribution::Known(identity)) => policy.input.contains(identity),
            Some(Attribution::Ambiguous(old, new)) => {
                policy.input.contains(old) || policy.input.contains(new)
            }
            None => false,
        }
    }

    pub(crate) fn settled(&mut self) {
        self.pending = None;
    }

    #[cfg(test)]
    pub(crate) fn for_test(identities: &HashMap<u64, String>) -> Self {
        Self {
            current: identities
                .iter()
                .map(|(&window, identity)| ((window, 0), Attribution::Known(identity.clone())))
                .collect(),
            pending: None,
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
        ids.refresh(&[window(2, "new")]);
        assert_eq!(ids.identity(&sample(1)), Some("old"));
        assert_eq!(ids.identity(&sample(2)), Some("new"));
        ids.settled();
        assert_eq!(ids.identity(&sample(1)), None);
        assert_eq!(ids.identity(&sample(2)), Some("new"));
        for pid in 3..1000 {
            ids.refresh(&[window(pid, "new")]);
            ids.settled();
            assert_eq!(ids.current.len(), 1);
            assert!(ids.pending.is_none());
        }
    }

    #[test]
    fn ambiguous_versions_do_not_guess_identity_or_expose_excluded_keys() {
        let mut ids = InputIdentities::from_windows(&[window(1, "old")]);
        ids.refresh(&[window(1, "new")]);
        let mut policy = timelens_ipc::privacy::Policy::default();
        policy.input.insert("old".into());
        assert_eq!(ids.identity(&sample(1)), None);
        assert!(ids.anonymous_only(&sample(1), &policy));
        ids.settled();
        assert_eq!(ids.identity(&sample(1)), Some("new"));
        assert!(!ids.anonymous_only(&sample(1), &policy));
    }
}
