//! Supervises `bolo-pill`, the native macOS helper (src/ui/BoloPill.swift) that
//! draws the recording pill. It runs as its own process, so a crash or hang in
//! the pill can never stop dictation: the daemon only restarts it, with a
//! budget, and falls back to today's chime and banners if it keeps dying.
//!
//! The supervisor follows the live pill style (`EventHub::pill`): switching to
//! Hidden stops the helper, switching back starts it, no daemon restart needed.

use crate::config::{PillConfig, PillStyle};
use crate::config_edit::ConfigDoc;
use crate::events::EventHub;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const HELPER: &str = "bolo-pill";
/// The helper may exit this many times per `RESTART_WINDOW` before we give up.
const MAX_STARTS: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(250);

/// What `bolo` tells the user when it cannot show the pill.
pub const INSTALL_HINT: &str =
    "run ./install.sh from the Bolo repo to build bolo-pill next to bolo (or put bolo-pill on PATH)";

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
pub fn locate_helper() -> Option<PathBuf> {
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

/// The one supervisor of this daemon, so `stop()` can reach it.
static ACTIVE: Mutex<Option<Arc<Supervisor>>> = Mutex::new(None);

/// What the supervisor does about the helper right now.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Start,
    Stop,
    Nothing,
}

fn decide(wanted: bool, running: bool) -> Action {
    match (wanted, running) {
        (true, false) => Action::Start,
        (false, true) => Action::Stop,
        _ => Action::Nothing,
    }
}

pub struct Supervisor {
    child: Mutex<Option<Child>>,
    stopping: AtomicBool,
}

impl Supervisor {
    /// Kills the helper and ends the supervisor thread.
    fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.kill_child();
    }

    fn kill_child(&self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Reaps the helper if it exited. Some(status) when it just did.
    fn reap(&self) -> Option<std::process::ExitStatus> {
        let mut guard = self.child.lock().unwrap();
        match guard.as_mut().map(Child::try_wait) {
            Some(Ok(Some(status))) => {
                *guard = None;
                Some(status)
            }
            _ => None,
        }
    }

    fn running(&self) -> bool {
        self.child.lock().unwrap().is_some()
    }
}

/// Starts the supervisor thread when this platform has a pill. Whether the
/// helper runs follows the live style, so a daemon started with the pill
/// hidden can show it later. Anything else keeps today's chime and banners.
pub fn spawn_supervisor(hub: &EventHub) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let supervisor = start(hub.clone(), locate_helper, POLL);
    *ACTIVE.lock().unwrap() = Some(supervisor);
}

fn start(
    hub: EventHub,
    locate: impl Fn() -> Option<PathBuf> + Send + 'static,
    poll: Duration,
) -> Arc<Supervisor> {
    let supervisor = Arc::new(Supervisor {
        child: Mutex::new(None),
        stopping: AtomicBool::new(false),
    });
    let worker = Arc::clone(&supervisor);
    std::thread::spawn(move || supervise(&worker, &hub, &locate, poll));
    supervisor
}

fn supervise(
    sup: &Supervisor,
    hub: &EventHub,
    locate: &impl Fn() -> Option<PathBuf>,
    poll: Duration,
) {
    let mut budget = RestartBudget::new(MAX_STARTS, RESTART_WINDOW);
    let mut gave_up = false;
    let mut warned_missing = false;
    while !sup.stopping.load(Ordering::SeqCst) {
        if let Some(status) = sup.reap() {
            if hub.pill().style != PillStyle::Hidden {
                eprintln!("[pill] {HELPER} exited ({status}); restarting");
            }
        }
        let wanted = hub.pill().style != PillStyle::Hidden;
        match decide(wanted, sup.running()) {
            Action::Stop => {
                sup.kill_child();
                eprintln!("[pill] style is hidden; stopped {HELPER}");
            }
            Action::Start if !gave_up => {
                if let Some(helper) = locate() {
                    warned_missing = false;
                    if !budget.try_start(Instant::now()) {
                        gave_up = true;
                        eprintln!(
                            "[pill] {HELPER} exited {MAX_STARTS} times within {}s; giving up, the pill stays hidden",
                            RESTART_WINDOW.as_secs()
                        );
                    } else {
                        match Command::new(&helper).stdin(Stdio::null()).spawn() {
                            Ok(child) => {
                                eprintln!(
                                    "[pill] started {} (pid {})",
                                    helper.display(),
                                    child.id()
                                );
                                *sup.child.lock().unwrap() = Some(child);
                            }
                            Err(e) => {
                                gave_up = true;
                                eprintln!(
                                    "[pill] cannot start {}: {e}; the pill stays hidden",
                                    helper.display()
                                );
                            }
                        }
                    }
                } else if !warned_missing {
                    warned_missing = true;
                    eprintln!(
                        "[pill] {HELPER} not found next to bolo or on PATH; {INSTALL_HINT}. Continuing without the pill."
                    );
                }
            }
            Action::Start | Action::Nothing => {}
        }
        if !wanted {
            // Turning the pill off and on again is a fresh start.
            budget = RestartBudget::new(MAX_STARTS, RESTART_WINDOW);
            gave_up = false;
        }
        std::thread::sleep(poll);
    }
}

/// Writes the given pill settings to `config.toml` (comments kept), reads the
/// result back and applies it live: the supervisor starts or stops the helper
/// and every subscriber gets a `config` event. `None` leaves a setting alone.
pub fn save_settings(
    config_path: &Path,
    hub: &EventHub,
    style: Option<PillStyle>,
    show_when_idle: Option<bool>,
) -> anyhow::Result<PillConfig> {
    let mut doc = ConfigDoc::load(config_path)?;
    if let Some(style) = style {
        doc.set(&["pill", "style"], style.as_str().into());
    }
    if let Some(show) = show_when_idle {
        doc.set(&["pill", "show_when_idle"], show.into());
    }
    doc.save()?;
    let defaults = PillConfig::default();
    let current = PillConfig {
        style: PillStyle::parse(&doc.str_at(&["pill", "style"], defaults.style.as_str()))
            .unwrap_or(defaults.style),
        show_when_idle: doc.bool_at(&["pill", "show_when_idle"], defaults.show_when_idle),
    };
    hub.set_pill(current.clone());
    Ok(current)
}

/// Stops the helper; called when the daemon quits. The helper also exits on
/// its own when the daemon socket closes, so this only makes it immediate.
pub fn stop() {
    if let Some(supervisor) = ACTIVE.lock().unwrap().take() {
        supervisor.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PillConfig;

    fn pill(style: PillStyle) -> PillConfig {
        PillConfig {
            style,
            show_when_idle: true,
        }
    }

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
    fn helper_runs_exactly_when_the_style_draws_something() {
        assert_eq!(decide(true, false), Action::Start);
        assert_eq!(decide(true, true), Action::Nothing);
        assert_eq!(decide(false, true), Action::Stop);
        assert_eq!(decide(false, false), Action::Nothing);
    }

    /// A fresh empty directory under the system temp dir.
    fn scratch_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "bolo-pill-test-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A stand-in for bolo-pill: a script that appends a line to `log` when it
    /// starts, then either sleeps (a healthy helper) or exits (a crashing one).
    fn fake_helper(dir: &std::path::Path, crashes: bool) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let log = dir.join("starts");
        let script = dir.join(if crashes { "crash-pill" } else { "steady-pill" });
        let body = if crashes { "exit 1" } else { "exec sleep 30" };
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho start >> '{}'\n{body}\n", log.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (script, log)
    }

    fn starts(log: &std::path::Path) -> usize {
        std::fs::read_to_string(log).map_or(0, |t| t.lines().count())
    }

    fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ok() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn supervisor_follows_the_live_style_without_a_restart() {
        let dir = scratch_dir("follow");
        let (script, log) = fake_helper(&dir, false);
        let hub = EventHub::spawn(pill(PillStyle::Hidden));
        let sup = start(
            hub.clone(),
            move || Some(script.clone()),
            Duration::from_millis(20),
        );

        // Hidden: no helper at all.
        std::thread::sleep(Duration::from_millis(200));
        assert!(!sup.running());
        assert_eq!(starts(&log), 0);

        // Switching to Large starts it...
        hub.set_pill(pill(PillStyle::Large));
        wait_for("the helper to start", || starts(&log) == 1);
        assert!(sup.running());

        // ...Small keeps the same process...
        hub.set_pill(pill(PillStyle::Small));
        std::thread::sleep(Duration::from_millis(200));
        assert!(sup.running());
        assert_eq!(starts(&log), 1);

        // ...and Hidden stops it again.
        hub.set_pill(pill(PillStyle::Hidden));
        wait_for("the helper to stop", || !sup.running());

        // Back to visible: a fresh start.
        hub.set_pill(pill(PillStyle::Small));
        wait_for("the helper to restart", || starts(&log) == 2);
        sup.shutdown();
        assert!(!sup.running());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn supervisor_gives_up_on_a_helper_that_keeps_crashing_until_the_pill_is_toggled() {
        let dir = scratch_dir("crash");
        let (script, log) = fake_helper(&dir, true);
        let hub = EventHub::spawn(pill(PillStyle::Small));
        let sup = start(
            hub.clone(),
            move || Some(script.clone()),
            Duration::from_millis(20),
        );

        wait_for("three start attempts", || starts(&log) >= 3);
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(starts(&log), 3, "must stop after the restart budget");

        // Hiding and showing the pill is a fresh start with a fresh budget.
        hub.set_pill(pill(PillStyle::Hidden));
        std::thread::sleep(Duration::from_millis(100));
        hub.set_pill(pill(PillStyle::Small));
        wait_for("more attempts", || starts(&log) > 3);
        sup.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_helper_is_not_an_error() {
        let hub = EventHub::spawn(pill(PillStyle::Small));
        let sup = start(hub, || None, Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!sup.running());
        sup.shutdown();
        // And the public entry points stay quiet without a helper on PATH.
        stop();
        spawn_supervisor(&EventHub::default());
        stop();
    }
}
