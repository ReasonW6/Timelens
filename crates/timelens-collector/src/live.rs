//! Live input previews for the core's screen: the attributed input counted so
//! far in the minute not sealed yet. They go straight to the core and never
//! through the spool, so nothing finer than a sealed minute is ever kept, and
//! only while the core answers that it shows them.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use timelens_ipc::{LiveInput, run_client_live_input};

/// While the core shows input live, the collection loop wakes this often.
pub const WAKE_INTERVAL: Duration = Duration::from_millis(250);
/// While it does not, it is asked again at most this often, and only on input.
const ASK_INTERVAL: Duration = Duration::from_secs(3);

pub struct LivePreview {
    sender: mpsc::Sender<LiveInput>,
    wanted: Arc<AtomicBool>,
    last: Option<LiveInput>,
    asked: Option<Instant>,
}

impl LivePreview {
    pub fn spawn(pipe_name: String, core_names: &'static [&'static str]) -> Self {
        let (sender, receiver) = mpsc::channel::<LiveInput>();
        let wanted = Arc::new(AtomicBool::new(false));
        let answer = wanted.clone();
        thread::spawn(move || {
            while let Ok(mut input) = receiver.recv() {
                // Only the newest preview matters.
                while let Ok(newer) = receiver.try_recv() {
                    input = newer;
                }
                let shown = run_client_live_input(&pipe_name, core_names, &input).unwrap_or(false);
                answer.store(shown, Ordering::Relaxed);
            }
        });
        Self {
            sender,
            wanted,
            last: None,
            asked: None,
        }
    }

    pub fn wanted(&self) -> bool {
        self.wanted.load(Ordering::Relaxed)
    }

    /// Whether a changed preview would be sent now.
    pub fn due(&self) -> bool {
        self.wanted() || self.asked.is_none_or(|at| at.elapsed() >= ASK_INTERVAL)
    }

    pub fn offer(&mut self, input: LiveInput) {
        if !self.due() || self.last.as_ref() == Some(&input) {
            return;
        }
        self.asked = Some(Instant::now());
        self.last = Some(input.clone());
        let _ = self.sender.send(input);
    }
}
