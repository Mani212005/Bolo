//! Supervises `bolo-pill`, the native macOS helper (src/ui/BoloPill.swift) that
//! draws the recording pill. It runs as its own process, so a crash or hang in
//! the pill can never stop dictation: the daemon only restarts it, with a
//! budget, and falls back to today's chime and banners if it keeps dying.

use crate::config::{PillConfig, PillStyle};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const HELPER: &str = "bolo-pill";
/// The helper may exit this many times per `RESTART_WINDOW` before we give up.
const MAX_STARTS: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(250);

static CHILD: Mutex<Option<Child>> = Mutex::new(None);
static STOPPING: AtomicBool = AtomicBool::new(false);

/// Allows at most `max` starts within a sliding `window`.
struct RestartBudget {
    max: usize,
    window: Duration,
    starts: VecDeque<Instant>,
}

impl RestartBudget {
    fn new(max: usize, window: Duration) -> Self {
        Self {
            max,
            window,
            starts: VecDeque::new(),
        }
    }

    /// Records a start at `now` and returns true, or false when the budget is spent.
    fn try_start(&mut self, now: Instant) -> bool {
        while let Some(&first) = self.starts.front() {
            if now.duration_since(first) >= self.window {
                self.starts.pop_front();
            } else {
                break;
            }
        }
        if self.starts.len() >= self.max {
            return false;
        }
        self.starts.push_back(now);
        true
    }
}

/// `bolo-pill` next to the running `bolo`, else on PATH.
fn locate_helper() -> Option<PathBuf> {
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(PathBuf::from))
    {
        let candidate = dir.join(HELPER);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(HELPER))
        .find(|candidate| candidate.is_file())
}

/// Starts the supervisor thread when this platform has a pill and the config
/// asks for one. Anything else keeps today's chime and banners.
pub fn spawn_supervisor(cfg: &PillConfig) {
    if !cfg!(target_os = "macos") || cfg.style == PillStyle::Hidden {
        return;
    }
    let Some(helper) = locate_helper() else {
        eprintln!(
            "[pill] {HELPER} not found next to bolo or on PATH; run ./install.sh to build it. Continuing without the pill."
        );
        return;
    };
    std::thread::spawn(move || supervise(&helper));
}

fn supervise(helper: &PathBuf) {
    let mut budget = RestartBudget::new(MAX_STARTS, RESTART_WINDOW);
    loop {
        if STOPPING.load(Ordering::SeqCst) {
            return;
        }
        if !budget.try_start(Instant::now()) {
            eprintln!(
                "[pill] {HELPER} exited {MAX_STARTS} times within {}s; giving up, the pill stays hidden",
                RESTART_WINDOW.as_secs()
            );
            return;
        }
        match Command::new(helper).stdin(Stdio::null()).spawn() {
            Ok(child) => {
                eprintln!("[pill] started {} (pid {})", helper.display(), child.id());
                *CHILD.lock().unwrap() = Some(child);
            }
            Err(e) => {
                eprintln!(
                    "[pill] cannot start {}: {e}; the pill stays hidden",
                    helper.display()
                );
                return;
            }
        }
        // Poll instead of blocking in wait() so stop() can take the child.
        let status = loop {
            std::thread::sleep(POLL);
            let mut guard = CHILD.lock().unwrap();
            match guard.as_mut().map(Child::try_wait) {
                Some(Ok(Some(status))) => {
                    *guard = None;
                    break Some(status);
                }
                Some(Ok(None)) => {}
                _ => break None, // stop() took the child, or waiting failed
            }
        };
        if STOPPING.load(Ordering::SeqCst) {
            return;
        }
        eprintln!("[pill] {HELPER} exited ({status:?}); restarting");
    }
}

/// Stops the helper; called when the daemon quits. The helper also exits on
/// its own when the daemon socket closes, so this only makes it immediate.
pub fn stop() {
    STOPPING.store(true, Ordering::SeqCst);
    if let Some(mut child) = CHILD.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_budget_allows_three_starts_per_window() {
        let t0 = Instant::now();
        let mut budget = RestartBudget::new(3, Duration::from_secs(60));
        assert!(budget.try_start(t0));
        assert!(budget.try_start(t0 + Duration::from_secs(5)));
        assert!(budget.try_start(t0 + Duration::from_secs(10)));
        assert!(!budget.try_start(t0 + Duration::from_secs(20)));
        // The window slides: the first start has aged out by t0 + 61s.
        assert!(budget.try_start(t0 + Duration::from_secs(61)));
        assert!(!budget.try_start(t0 + Duration::from_secs(62)));
    }

    #[test]
    fn hidden_style_and_missing_helper_do_not_panic() {
        spawn_supervisor(&PillConfig {
            style: PillStyle::Hidden,
            show_when_idle: true,
        });
    }
}
