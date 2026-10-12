use std::time::{Duration, Instant};

/// Keep deferred work dirty while the native window is hidden or minimized.
pub(crate) struct UiRefresh {
    active: bool,
    dirty: bool,
    last_refresh: Instant,
}

impl UiRefresh {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            active: false,
            dirty: true,
            last_refresh: now,
        }
    }

    pub(crate) fn request(&mut self, requested: bool) {
        self.dirty |= requested;
    }

    pub(crate) fn due(&mut self, active: bool, busy: bool, now: Instant) -> bool {
        self.dirty |= active && !self.active;
        self.active = active;
        active
            && !busy
            && (self.dirty || now.duration_since(self.last_refresh) >= Duration::from_secs(2))
    }

    pub(crate) fn refreshed(&mut self, now: Instant) {
        self.dirty = false;
        self.last_refresh = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_and_minimized_views_stay_dirty_until_restored() {
        let now = Instant::now();
        let mut refresh = UiRefresh::new(now);
        assert!(!refresh.due(false, false, now + Duration::from_secs(60)));
        refresh.request(true);
        assert!(!refresh.due(false, false, now + Duration::from_secs(120)));
        assert!(refresh.due(true, false, now + Duration::from_secs(120)));
        refresh.refreshed(now + Duration::from_secs(120));
        assert!(!refresh.due(true, false, now + Duration::from_secs(121)));
        assert!(refresh.due(true, false, now + Duration::from_secs(122)));
    }

    #[test]
    fn restore_during_a_busy_task_retries_without_losing_invalidation() {
        let now = Instant::now();
        let mut refresh = UiRefresh::new(now);
        assert!(refresh.due(true, false, now));
        refresh.refreshed(now);
        assert!(!refresh.due(false, false, now));
        assert!(!refresh.due(true, true, now));
        assert!(refresh.due(true, false, now));
        refresh.refreshed(now);
        assert!(!refresh.due(true, false, now));
    }
}
