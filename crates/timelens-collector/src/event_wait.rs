use std::time::{Duration, Instant};
use timelens_observer::WindowEvents;

/// Ignore message-pump wakeups that produced no window events, without moving
/// the caller's minute, reconciliation or control-check deadline.
pub fn until(deadline: Instant, wait: impl FnMut(Duration) -> WindowEvents) -> WindowEvents {
    until_with_clock(deadline, wait, Instant::now)
}

fn until_with_clock(
    deadline: Instant,
    mut wait: impl FnMut(Duration) -> WindowEvents,
    mut now: impl FnMut() -> Instant,
) -> WindowEvents {
    loop {
        let events = wait(deadline.saturating_duration_since(now()));
        if events.reconcile || !events.events.is_empty() || now() >= deadline {
            return events;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use timelens_observer::{WindowEvent, WindowEventKind};

    #[test]
    fn empty_wakeups_keep_the_original_absolute_deadline() {
        let start = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let mut waits = vec![];
        let events = until_with_clock(
            start + Duration::from_millis(100),
            |remaining| {
                waits.push(remaining.as_millis());
                elapsed.set(elapsed.get() + remaining.min(Duration::from_millis(30)));
                WindowEvents::default()
            },
            || start + elapsed.get(),
        );
        assert!(events.events.is_empty());
        assert_eq!(waits, [100, 70, 40, 10]);
        assert_eq!(elapsed.get(), Duration::from_millis(100));
    }

    #[test]
    fn a_real_event_or_reconciliation_request_returns_immediately() {
        for reconcile in [false, true] {
            let start = Instant::now();
            let calls = Cell::new(0);
            let events = until_with_clock(
                start + Duration::from_secs(1),
                |_| {
                    calls.set(calls.get() + 1);
                    WindowEvents {
                        reconcile,
                        events: if reconcile {
                            vec![]
                        } else {
                            vec![WindowEvent {
                                window_id: 42,
                                kind: WindowEventKind::Foreground,
                            }]
                        },
                    }
                },
                || start,
            );
            assert_eq!(calls.get(), 1);
            assert_eq!(events.reconcile, reconcile);
            assert_eq!(events.events.len(), usize::from(!reconcile));
        }
    }

    #[test]
    fn due_minute_and_periodic_checks_still_pump_pending_events_once() {
        let now = Instant::now();
        let mut calls = 0;
        until_with_clock(
            now,
            |remaining| {
                calls += 1;
                assert_eq!(remaining, Duration::ZERO);
                WindowEvents::default()
            },
            || now,
        );
        assert_eq!(calls, 1);
    }
}
