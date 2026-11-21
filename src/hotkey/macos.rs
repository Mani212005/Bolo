use super::HotkeyListener;
use anyhow::Result;
use std::cell::Cell;
use std::os::raw::c_void;
use std::time::{Duration, Instant};

type CGEventRef = *mut c_void;
type CGEventTapProxy = *mut c_void;
type CFMachPortRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CGEventFlags = u64;

type CGEventTapCallBack = unsafe extern "C" fn(
    proxy: CGEventTapProxy,
    event_type: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef;

// CoreGraphics Constants
const K_CG_SESSION_EVENT_TAP: u32 = 1;
const K_CG_HEAD_INSERT_EVENT_TAP: u32 = 0;
const K_CG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;
const K_CG_EVENT_TAP_OPTION_LISTEN_ONLY: u32 = 1;

const K_CG_EVENT_MOUSE_MOVED: u32 = 5;
const K_CG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
const K_CG_EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
const K_CG_EVENT_KEY_DOWN: u32 = 10;
const K_CG_EVENT_FLAGS_CHANGED: u32 = 12;
const K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFFFFFE;
const K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFFFFFF;

// CGEventField Constants
const K_CG_KEYBOARD_EVENT_AUTOREPEAT: u32 = 49;
const K_CG_KEYBOARD_EVENT_KEYCODE: u32 = 9;

// Modifier Flag Masks (CoreGraphics)
const K_CG_EVENT_FLAG_MASK_CONTROL: u64 = 0x00040000; // Control (1 << 18)
const K_CG_EVENT_FLAG_MASK_ALTERNATE: u64 = 0x00080000; // Option / Alt (1 << 19)
const K_CG_EVENT_FLAG_MASK_COMMAND: u64 = 0x00100000; // Command / Meta (1 << 20)

// Keycodes on macOS
const KEY_SPACE: i64 = 49;
const KEY_V: i64 = 9;
const KEY_P: i64 = 35;
const KEY_I: i64 = 34;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetFlags(event: CGEventRef) -> CGEventFlags;
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: *const c_void);
    fn CFRunLoopRun();
    static kCFRunLoopCommonModes: *const c_void;
}

struct TapContext {
    callback: Box<dyn Fn(&str) -> bool + Send + Sync>,
    port: CFMachPortRef,
    /// Touched only on the tap's own run-loop thread.
    hold: Cell<ControlHold>,
}

/// What a pointer move does while Control may be held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PointerMove {
    /// Not part of a gesture: the move goes on to the apps.
    Pass,
    /// Control is held: Bolo watches the move for a circle.
    Watch,
    /// Control is held over a frozen screen: Bolo watches the move and the
    /// apps never see it, so the hover, tooltip or menu highlight under the
    /// pointer at the press stays on screen, as with Cmd+Shift+4.
    WatchAndHold,
}

/// The longest the apps go without pointer moves in one Control hold. A
/// capture circle takes a second or two; past this, moves reach the apps
/// again and later captures are live, so a stuck key can never leave the
/// user without a pointer.
pub(crate) const MAX_FREEZE: Duration = Duration::from_secs(10);

/// Follows the Control key through the tap's events. Pressing Control while
/// dictating freezes the screen for a capture ([`crate::vision::freeze`]);
/// releasing it thaws it, and so do a release the tap missed, the system
/// disabling the tap, and [`MAX_FREEZE`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ControlHold {
    held: bool,
    /// When the daemon froze the screen for this hold.
    frozen_since: Option<Instant>,
}

/// A change in the Control key, from the events the tap sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlEdge {
    Pressed,
    Released,
}

impl ControlHold {
    /// A flags-changed event: an edge only when Control itself went down or
    /// up, not when another modifier changed while it is held. Modifiers do
    /// not auto-repeat, so a long hold is one press.
    pub(crate) fn flags_changed(&mut self, flags: u64) -> Option<ControlEdge> {
        let held = is_mouse_modifier_active(flags);
        if held == self.held {
            return None;
        }
        self.held = held;
        self.frozen_since = None;
        Some(if held {
            ControlEdge::Pressed
        } else {
            ControlEdge::Released
        })
    }

    /// The daemon froze the screen for this press, at `now`.
    pub(crate) fn set_frozen(&mut self, now: Instant) {
        self.frozen_since = self.held.then_some(now);
    }

    /// The daemon is no longer frozen for this hold (dictation stopped, say):
    /// the moves go back to the apps at once.
    pub(crate) fn stop_holding(&mut self) {
        self.frozen_since = None;
    }

    /// A pointer move at `now`, and whether the screen thaws with it: when
    /// it shows a Control release the tap missed (a move without Control
    /// while it is thought held: the release went to a secure-input field,
    /// say), or when the hold has been frozen for [`MAX_FREEZE`].
    pub(crate) fn pointer_moved(&mut self, flags: u64, now: Instant) -> (PointerMove, bool) {
        let control = is_mouse_modifier_active(flags);
        let mut thaw = false;
        if !control && self.held {
            self.held = false;
            self.frozen_since = None;
            thaw = true;
        }
        if let Some(since) = self.frozen_since {
            if now.saturating_duration_since(since) >= MAX_FREEZE {
                self.frozen_since = None;
                thaw = true;
            }
        }
        let action = match (control, self.frozen_since.is_some()) {
            (false, _) => PointerMove::Pass,
            (true, false) => PointerMove::Watch,
            (true, true) => PointerMove::WatchAndHold,
        };
        (action, thaw)
    }

    /// The system disabled the tap (it was too slow, or user input turned
    /// it off): events skipped it meanwhile, Control may have come up
    /// unseen, so the hold is forgotten. Whether the screen thaws.
    pub(crate) fn tap_disabled(&mut self) -> bool {
        let was_holding = self.held;
        *self = Self::default();
        was_holding
    }
}

pub(crate) fn match_hotkey(keycode: i64, flags: u64, autorepeat: i64) -> Option<&'static str> {
    if autorepeat != 0 {
        return None;
    }
    let has_ctrl = (flags & K_CG_EVENT_FLAG_MASK_CONTROL) != 0;
    let has_opt = (flags & K_CG_EVENT_FLAG_MASK_ALTERNATE) != 0;
    let has_cmd = (flags & K_CG_EVENT_FLAG_MASK_COMMAND) != 0;

    // 1. Ctrl + Space (with NO Command held) -> Toggle Start / Stop
    if keycode == KEY_SPACE && has_ctrl && !has_cmd {
        Some("toggle")
    }
    // 2. Option + V -> Quick-Splice Clipboard into dictation
    else if keycode == KEY_V && has_opt && !has_cmd {
        Some("quick-splice")
    }
    // 3. Option + P -> Pause / Resume Recording
    else if keycode == KEY_P && has_opt && !has_cmd {
        Some("pause")
    }
    // 4. Option + I -> Re-Type Last Transcription
    else if keycode == KEY_I && has_opt && !has_cmd {
        Some("insert-last")
    } else {
        None
    }
}

pub(crate) fn is_mouse_modifier_active(flags: u64) -> bool {
    (flags & K_CG_EVENT_FLAG_MASK_CONTROL) != 0
}

/// The tap's handlers send the daemon a command line and learn whether it
/// acted on it (see [`HotkeyListener::start`]).
///
/// A flags-changed event. Control going down asks the daemon to freeze the
/// screen, and the hold begins only if the daemon says it did.
pub(crate) fn control_changed(
    hold: &mut ControlHold,
    send: &dyn Fn(&str) -> bool,
    flags: u64,
    at: CGPoint,
    now: Instant,
) {
    match hold.flags_changed(flags) {
        Some(ControlEdge::Pressed) => {
            if send(&format!("freeze {:.1} {:.1}", at.x, at.y)) {
                hold.set_frozen(now);
            }
        }
        Some(ControlEdge::Released) => {
            send("thaw");
        }
        None => {}
    }
}

/// A pointer move (`plain`: not a drag). Whether the apps must not see it:
/// only while the daemon keeps the screen frozen, which it answers for each
/// move of the hold; once it does not (dictation stopped), the hold ends and
/// the move goes on.
pub(crate) fn pointer_moved(
    hold: &mut ControlHold,
    send: &dyn Fn(&str) -> bool,
    flags: u64,
    at: CGPoint,
    plain: bool,
    now: Instant,
) -> bool {
    let (action, thaw) = hold.pointer_moved(flags, now);
    if thaw {
        send("thaw");
    }
    if action == PointerMove::Pass {
        return false;
    }
    let still_frozen = send(&format!("mouse {:.1} {:.1}", at.x, at.y));
    if action == PointerMove::WatchAndHold && !still_frozen {
        hold.stop_holding();
        return false;
    }
    action == PointerMove::WatchAndHold && plain
}

/// The system disabled the tap: the hold is forgotten and the screen thaws.
pub(crate) fn tap_disabled(hold: &mut ControlHold, send: &dyn Fn(&str) -> bool) {
    if hold.tap_disabled() {
        send("thaw");
    }
}

unsafe extern "C" fn event_tap_callback(
    _proxy: CGEventTapProxy,
    event_type: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    if user_info.is_null() {
        return event;
    }
    let ctx = &*(user_info as *const TapContext);

    // Auto-recover if macOS temporarily disables event tap on load/timeout
    if event_type == K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT
        || event_type == K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT
    {
        let mut hold = ctx.hold.get();
        tap_disabled(&mut hold, &*ctx.callback);
        ctx.hold.set(hold);
        CGEventTapEnable(ctx.port, true);
        return event;
    }

    if event_type == K_CG_EVENT_FLAGS_CHANGED {
        let mut hold = ctx.hold.get();
        control_changed(
            &mut hold,
            &*ctx.callback,
            CGEventGetFlags(event),
            CGEventGetLocation(event),
            Instant::now(),
        );
        ctx.hold.set(hold);
    } else if event_type == K_CG_EVENT_MOUSE_MOVED
        || event_type == K_CG_EVENT_LEFT_MOUSE_DRAGGED
        || event_type == K_CG_EVENT_RIGHT_MOUSE_DRAGGED
    {
        let mut hold = ctx.hold.get();
        // Only plain moves are held back: a drag is the user acting.
        let held_back = pointer_moved(
            &mut hold,
            &*ctx.callback,
            CGEventGetFlags(event),
            CGEventGetLocation(event),
            event_type == K_CG_EVENT_MOUSE_MOVED,
            Instant::now(),
        );
        ctx.hold.set(hold);
        if held_back {
            return std::ptr::null_mut();
        }
    } else if event_type == K_CG_EVENT_KEY_DOWN {
        let keycode = CGEventGetIntegerValueField(event, K_CG_KEYBOARD_EVENT_KEYCODE);
        let flags = CGEventGetFlags(event);
        let autorepeat = CGEventGetIntegerValueField(event, K_CG_KEYBOARD_EVENT_AUTOREPEAT);

        if let Some(cmd) = match_hotkey(keycode, flags, autorepeat) {
            eprintln!("[macos-hotkey] {cmd} matched -> swallowing event");
            (ctx.callback)(cmd);
            return std::ptr::null_mut();
        }
    }

    event
}

pub struct MacOsHotkeyListener;

impl MacOsHotkeyListener {
    pub fn new() -> Self {
        Self
    }
}

impl HotkeyListener for MacOsHotkeyListener {
    fn start(&self, callback: Box<dyn Fn(&str) -> bool + Send + Sync>) -> Result<()> {
        std::thread::spawn(move || {
            unsafe {
                // Event mask: KeyDown, modifier changes (Control) and mouse movements/drags
                let event_mask: u64 = (1 << K_CG_EVENT_KEY_DOWN)
                    | (1 << K_CG_EVENT_FLAGS_CHANGED)
                    | (1 << K_CG_EVENT_MOUSE_MOVED)
                    | (1 << K_CG_EVENT_LEFT_MOUSE_DRAGGED)
                    | (1 << K_CG_EVENT_RIGHT_MOUSE_DRAGGED);

                let ctx_box = Box::new(TapContext {
                    callback,
                    port: std::ptr::null_mut(),
                    hold: Cell::new(ControlHold::default()),
                });
                let ctx_raw = Box::into_raw(ctx_box);

                let mut port = CGEventTapCreate(
                    K_CG_SESSION_EVENT_TAP,
                    K_CG_HEAD_INSERT_EVENT_TAP,
                    K_CG_EVENT_TAP_OPTION_DEFAULT,
                    event_mask,
                    event_tap_callback,
                    ctx_raw as *mut c_void,
                );

                let listen_only = port.is_null();
                if listen_only {
                    // Without Accessibility: hotkeys are not swallowed and the
                    // Control-press freeze cannot keep moves from the apps.
                    port = CGEventTapCreate(
                        K_CG_SESSION_EVENT_TAP,
                        K_CG_HEAD_INSERT_EVENT_TAP,
                        K_CG_EVENT_TAP_OPTION_LISTEN_ONLY,
                        event_mask,
                        event_tap_callback,
                        ctx_raw as *mut c_void,
                    );
                }

                if port.is_null() {
                    eprintln!("[macos-hotkey] CGEventTapCreate failed - ensure Accessibility permissions are granted to Bolo in macOS System Settings");
                    return;
                }

                (*ctx_raw).port = port;

                let source = CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0);
                if source.is_null() {
                    eprintln!("[macos-hotkey] CFMachPortCreateRunLoopSource failed");
                    return;
                }

                let run_loop = CFRunLoopGetCurrent();
                CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);
                CGEventTapEnable(port, true);

                eprintln!(
                    "[macos-hotkey] native CGEventTap active ({}) with kernel-level autorepeat filtering (Ctrl+Space, Opt+V, Opt+P, Opt+I)",
                    if listen_only { "listen-only" } else { "active" }
                );

                CFRunLoopRun();
            }
        });

        Ok(())
    }
}

/// The recorded hold, and a way to play it through the tap's logic.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// One recorded tap event: milliseconds since the recording began,
    /// event type and flags.
    pub(crate) type Recorded = (u64, u32, u64);

    pub(crate) fn recorded_circle() -> Vec<Recorded> {
        include_str!("testdata/control_circle_events.txt")
            .lines()
            .filter(|line| !line.starts_with('#'))
            .map(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                let flags = u64::from_str_radix(fields[2].trim_start_matches("0x"), 16).unwrap();
                (
                    fields[0].parse().unwrap(),
                    fields[1].parse().unwrap(),
                    flags,
                )
            })
            .collect()
    }

    /// Plays `events` through the tap's handlers, sending to `send`;
    /// `before(i)` runs ahead of event `i`. How many pointer moves the tap
    /// kept from the apps.
    pub(crate) fn play(
        events: &[Recorded],
        send: &dyn Fn(&str) -> bool,
        mut before: impl FnMut(usize),
    ) -> usize {
        let mut hold = ControlHold::default();
        let start = Instant::now();
        let at = CGPoint { x: 522.0, y: 517.0 };
        let mut held_back = 0;
        for (i, &(ms, event_type, flags)) in events.iter().enumerate() {
            before(i);
            let now = start + Duration::from_millis(ms);
            if event_type == K_CG_EVENT_FLAGS_CHANGED {
                control_changed(&mut hold, send, flags, at, now);
            } else if pointer_moved(
                &mut hold,
                send,
                flags,
                at,
                event_type == K_CG_EVENT_MOUSE_MOVED,
                now,
            ) {
                held_back += 1;
            }
        }
        held_back
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hotkey_matching_and_swallowing() {
        // Ctrl+Space -> "toggle"
        assert_eq!(
            match_hotkey(KEY_SPACE, K_CG_EVENT_FLAG_MASK_CONTROL, 0),
            Some("toggle")
        );

        // Option+V -> "quick-splice"
        assert_eq!(
            match_hotkey(KEY_V, K_CG_EVENT_FLAG_MASK_ALTERNATE, 0),
            Some("quick-splice")
        );

        // Option+P -> "pause"
        assert_eq!(
            match_hotkey(KEY_P, K_CG_EVENT_FLAG_MASK_ALTERNATE, 0),
            Some("pause")
        );

        // Option+I -> "insert-last"
        assert_eq!(
            match_hotkey(KEY_I, K_CG_EVENT_FLAG_MASK_ALTERNATE, 0),
            Some("insert-last")
        );

        // Autorepeat should be dropped (None)
        assert_eq!(
            match_hotkey(KEY_SPACE, K_CG_EVENT_FLAG_MASK_CONTROL, 1),
            None
        );

        // Command modifier present should not match bolo hotkeys
        assert_eq!(
            match_hotkey(
                KEY_V,
                K_CG_EVENT_FLAG_MASK_ALTERNATE | K_CG_EVENT_FLAG_MASK_COMMAND,
                0
            ),
            None
        );

        // Unrelated key should not match
        assert_eq!(match_hotkey(12, K_CG_EVENT_FLAG_MASK_CONTROL, 0), None);
    }

    use super::testing::{recorded_circle, Recorded};

    /// What the tap did with a replayed sequence.
    #[derive(Debug, Default)]
    struct Replay {
        edges: Vec<ControlEdge>,
        moves: Vec<PointerMove>,
        /// "thaw" commands sent, from an edge or a move.
        thaws: usize,
    }

    /// Plays events through `hold` the way the tap does, with the daemon
    /// answering `frozen` to each press.
    fn replay_into(hold: &mut ControlHold, events: &[Recorded], frozen: bool, out: &mut Replay) {
        let start = Instant::now();
        for &(ms, event_type, flags) in events {
            let now = start + Duration::from_millis(ms);
            if event_type == K_CG_EVENT_FLAGS_CHANGED {
                let edge = hold.flags_changed(flags);
                match edge {
                    Some(ControlEdge::Pressed) if frozen => hold.set_frozen(now),
                    Some(ControlEdge::Released) => out.thaws += 1,
                    _ => {}
                }
                out.edges.extend(edge);
            } else {
                let (action, thaw) = hold.pointer_moved(flags, now);
                out.thaws += thaw as usize;
                out.moves.push(action);
            }
        }
    }

    fn replay(events: &[Recorded], frozen: bool) -> Replay {
        let mut out = Replay::default();
        replay_into(&mut ControlHold::default(), events, frozen, &mut out);
        out
    }

    fn is_control_release(&(_, event_type, flags): &Recorded) -> bool {
        event_type == K_CG_EVENT_FLAGS_CHANGED && !is_mouse_modifier_active(flags)
    }

    #[test]
    fn a_recorded_hold_freezes_once_and_holds_back_its_moves() {
        let out = replay(&recorded_circle(), true);
        // Shift going down and up mid-hold is not another press.
        assert_eq!(out.edges, vec![ControlEdge::Pressed, ControlEdge::Released]);
        assert_eq!(out.thaws, 1, "Control up thaws");
        assert_eq!(
            out.moves.first(),
            Some(&PointerMove::Pass),
            "before the press"
        );
        assert_eq!(
            out.moves.last(),
            Some(&PointerMove::Pass),
            "after the release"
        );
        let held = &out.moves[1..out.moves.len() - 1];
        assert_eq!(held.len(), 77);
        assert!(held.iter().all(|m| *m == PointerMove::WatchAndHold));
    }

    #[test]
    fn moves_reach_the_apps_when_the_daemon_did_not_freeze() {
        // Not dictating: the circle is watched as before and nothing is held back.
        let out = replay(&recorded_circle(), false);
        assert_eq!(out.edges, vec![ControlEdge::Pressed, ControlEdge::Released]);
        assert!(!out.moves.contains(&PointerMove::WatchAndHold));
        assert_eq!(
            out.moves
                .iter()
                .filter(|m| **m == PointerMove::Watch)
                .count(),
            77
        );
    }

    #[test]
    fn a_missed_release_thaws_at_the_next_plain_move() {
        // The recorded hold with its release lost.
        let events: Vec<_> = recorded_circle()
            .into_iter()
            .filter(|e| !is_control_release(e))
            .collect();
        let out = replay(&events, true);
        assert_eq!(out.edges, vec![ControlEdge::Pressed]);
        assert_eq!(out.thaws, 1, "the plain move after the hold thaws");
        assert_eq!(out.moves.last(), Some(&PointerMove::Pass));
        // And the next press freezes again.
        let mut again = events.clone();
        let offset = events.last().unwrap().0 + 100;
        again.extend(
            recorded_circle()
                .into_iter()
                .map(|(ms, t, f)| (ms + offset, t, f)),
        );
        let out = replay(&again, true);
        assert_eq!(
            out.edges
                .iter()
                .filter(|e| **e == ControlEdge::Pressed)
                .count(),
            2
        );
        assert_eq!(
            out.moves
                .iter()
                .filter(|m| **m == PointerMove::WatchAndHold)
                .count(),
            2 * 77,
            "both circles held back"
        );
        assert_eq!(out.moves.last(), Some(&PointerMove::Pass));
    }

    #[test]
    fn a_hold_longer_than_the_limit_lets_moves_through() {
        // The recorded hold, with Control still down when the same circle
        // is drawn again 10 s later (a stuck key, or a forgotten hold).
        let events = recorded_circle();
        let press = events
            .iter()
            .position(|e| e.1 == K_CG_EVENT_FLAGS_CHANGED)
            .unwrap();
        let release = events.iter().position(is_control_release).unwrap();
        let hold = &events[..release];
        let later: Vec<_> = events[press + 1..release]
            .iter()
            .map(|&(ms, t, f)| (ms + MAX_FREEZE.as_millis() as u64, t, f))
            .collect();
        let mut out = Replay::default();
        let mut state = ControlHold::default();
        replay_into(&mut state, hold, true, &mut out);
        let first = out.moves.len();
        assert!(out.moves[1..]
            .iter()
            .all(|m| *m == PointerMove::WatchAndHold));
        assert_eq!(out.thaws, 0);
        let start = Instant::now();
        for &(ms, t, f) in &later {
            if t != K_CG_EVENT_FLAGS_CHANGED {
                let (action, thaw) = state.pointer_moved(f, start + Duration::from_millis(ms));
                out.thaws += thaw as usize;
                out.moves.push(action);
            }
        }
        assert_eq!(out.thaws, 1, "the limit thaws once");
        assert!(
            out.moves[first..].iter().all(|m| *m == PointerMove::Watch),
            "past the limit the circle is still watched but the apps see it"
        );
    }

    #[test]
    fn the_system_disabling_the_tap_ends_the_hold() {
        let events = recorded_circle();
        let mid = events.len() / 2;
        let mut state = ControlHold::default();
        let mut out = Replay::default();
        replay_into(&mut state, &events[..mid], true, &mut out);
        assert_eq!(out.moves.last(), Some(&PointerMove::WatchAndHold));
        assert!(state.tap_disabled(), "a frozen hold thaws");
        // The rest of the recorded hold, Control still down: watched, not held.
        let rest: Vec<_> = events[mid..]
            .iter()
            .copied()
            .filter(|e| e.1 != K_CG_EVENT_FLAGS_CHANGED)
            .collect();
        let tail = replay_into_moves(&mut state, &rest);
        assert!(!tail.contains(&PointerMove::WatchAndHold));
        assert!(!ControlHold::default().tap_disabled(), "nothing to thaw");
    }

    fn replay_into_moves(state: &mut ControlHold, events: &[Recorded]) -> Vec<PointerMove> {
        let start = Instant::now();
        events
            .iter()
            .map(|&(ms, _, f)| state.pointer_moved(f, start + Duration::from_millis(ms)).0)
            .collect()
    }

    #[test]
    fn test_mouse_modifier_active_flags() {
        // Control key held down -> true
        assert!(is_mouse_modifier_active(K_CG_EVENT_FLAG_MASK_CONTROL));
        // Control + Option -> true
        assert!(is_mouse_modifier_active(
            K_CG_EVENT_FLAG_MASK_CONTROL | K_CG_EVENT_FLAG_MASK_ALTERNATE
        ));
        // Control + Command -> true
        assert!(is_mouse_modifier_active(
            K_CG_EVENT_FLAG_MASK_CONTROL | K_CG_EVENT_FLAG_MASK_COMMAND
        ));

        // No modifier -> false
        assert!(!is_mouse_modifier_active(0));
        // Option only -> false
        assert!(!is_mouse_modifier_active(K_CG_EVENT_FLAG_MASK_ALTERNATE));
        // Command only -> false
        assert!(!is_mouse_modifier_active(K_CG_EVENT_FLAG_MASK_COMMAND));
    }
}
