//! The screen as it was the instant Control went down.
//!
//! On macOS a capture is a Control-held circle, which ends a second or so
//! after the press, with the pointer elsewhere: hovers, tooltips and menu
//! highlights under it are gone by then. So Bolo grabs the display at the
//! press, on its own thread, and every capture of that hold crops from that
//! frame, the way the system screenshot keeps what was on screen when it
//! started.

use std::sync::{Arc, Condvar, Mutex};

/// A grab started at a Control press, finished or still running.
pub struct Pending<F> {
    frame: Mutex<Option<Option<Arc<F>>>>,
    done: Condvar,
}

impl<F: Send + Sync + 'static> Pending<F> {
    /// Runs `grab` on its own thread; [`Pending::wait`] gives its result.
    pub fn start(grab: impl FnOnce() -> Option<F> + Send + 'static) -> Arc<Self> {
        let pending = Arc::new(Self {
            frame: Mutex::new(None),
            done: Condvar::new(),
        });
        let filler = Arc::clone(&pending);
        std::thread::spawn(move || {
            let frame = grab().map(Arc::new);
            *filler.frame.lock().unwrap() = Some(frame);
            filler.done.notify_all();
        });
        pending
    }

    /// The grabbed frame, waiting for the grab when it is still running;
    /// `None` when it failed.
    pub fn wait(&self) -> Option<Arc<F>> {
        let mut frame = self.frame.lock().unwrap();
        loop {
            if let Some(result) = frame.as_ref() {
                return result.clone();
            }
            frame = self.done.wait(frame).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn a_capture_waits_for_a_grab_still_running() {
        let pressed = Instant::now();
        let pending = Pending::start(move || {
            std::thread::sleep(Duration::from_millis(40));
            Some(pressed)
        });
        // The circle finished before the grab did: it still gets that grab.
        assert_eq!(pending.wait().as_deref(), Some(&pressed));
        // Later captures of the same hold get the same frame.
        assert_eq!(pending.wait().as_deref(), Some(&pressed));
    }

    #[test]
    fn a_failed_grab_gives_nothing() {
        let pending = Pending::<u8>::start(|| None);
        assert_eq!(pending.wait(), None);
    }
}
