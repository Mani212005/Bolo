use super::HotkeyListener;
use anyhow::Result;
use std::cell::Cell;
use std::os::raw::c_void;

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

/// Follows the Control key through the tap's events. Pressing Control while
/// dictating freezes the screen for a capture ([`crate::vision::freeze`]);
/// releasing it thaws it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ControlHold {
    held: bool,
    frozen: bool,
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
        self.set_held(is_mouse_modifier_active(flags))
    }

    /// The daemon froze the screen for this press.
    pub(crate) fn set_frozen(&mut self) {
        self.frozen = self.held;
    }

    /// A pointer move, and a Control release the tap missed: a move without
    /// Control while it is thought held (the tap was briefly disabled, or
    /// the release went to a secure-input field).
    pub(crate) fn pointer_moved(&mut self, flags: u64) -> (PointerMove, Option<ControlEdge>) {
        let control = is_mouse_modifier_active(flags);
        let edge = if control { None } else { self.set_held(false) };
        let action = match (control, self.frozen) {
            (false, _) => PointerMove::Pass,
            (true, false) => PointerMove::Watch,
            (true, true) => PointerMove::WatchAndHold,
        };
        (action, edge)
    }

    fn set_held(&mut self, held: bool) -> Option<ControlEdge> {
        if held == self.held {
            return None;
        }
        self.held = held;
        self.frozen = false;
        Some(if held {
            ControlEdge::Pressed
        } else {
            ControlEdge::Released
        })
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
        CGEventTapEnable(ctx.port, true);
        return event;
    }

    if event_type == K_CG_EVENT_FLAGS_CHANGED {
        let mut hold = ctx.hold.get();
        match hold.flags_changed(CGEventGetFlags(event)) {
            Some(ControlEdge::Pressed) => {
                let loc = CGEventGetLocation(event);
                if (ctx.callback)(&format!("freeze {:.1} {:.1}", loc.x, loc.y)) {
                    hold.set_frozen();
                }
            }
            Some(ControlEdge::Released) => {
                (ctx.callback)("thaw");
            }
            None => {}
        }
        ctx.hold.set(hold);
    } else if event_type == K_CG_EVENT_MOUSE_MOVED
        || event_type == K_CG_EVENT_LEFT_MOUSE_DRAGGED
        || event_type == K_CG_EVENT_RIGHT_MOUSE_DRAGGED
    {
        let mut hold = ctx.hold.get();
        let (action, edge) = hold.pointer_moved(CGEventGetFlags(event));
        ctx.hold.set(hold);
        if edge == Some(ControlEdge::Released) {
            (ctx.callback)("thaw");
        }
        if action != PointerMove::Pass {
            let loc = CGEventGetLocation(event);
            (ctx.callback)(&format!("mouse {:.1} {:.1}", loc.x, loc.y));
        }
        // Only plain moves are held back: a drag is the user acting.
        if action == PointerMove::WatchAndHold && event_type == K_CG_EVENT_MOUSE_MOVED {
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

                if port.is_null() {
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

                eprintln!("[macos-hotkey] native CGEventTap active with kernel-level autorepeat filtering (Ctrl+Space, Opt+V, Opt+P, Opt+I)");

                CFRunLoopRun();
            }
        });

        Ok(())
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

    /// `(event type, flags)` of each recorded tap event.
    fn recorded_circle() -> Vec<(u32, u64)> {
        include_str!("testdata/control_circle_events.txt")
            .lines()
            .filter(|line| !line.starts_with('#'))
            .map(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                let flags = u64::from_str_radix(fields[2].trim_start_matches("0x"), 16).unwrap();
                (fields[1].parse().unwrap(), flags)
            })
            .collect()
    }

    /// Plays events through a `ControlHold` the way the tap does, with the
    /// daemon answering `frozen` to each press.
    fn replay(events: &[(u32, u64)], frozen: bool) -> (Vec<ControlEdge>, Vec<PointerMove>) {
        let mut hold = ControlHold::default();
        let (mut edges, mut moves) = (Vec::new(), Vec::new());
        for &(event_type, flags) in events {
            if event_type == K_CG_EVENT_FLAGS_CHANGED {
                let edge = hold.flags_changed(flags);
                if edge == Some(ControlEdge::Pressed) && frozen {
                    hold.set_frozen();
                }
                edges.extend(edge);
            } else {
                let (action, edge) = hold.pointer_moved(flags);
                edges.extend(edge);
                moves.push(action);
            }
        }
        (edges, moves)
    }

    #[test]
    fn a_recorded_hold_freezes_once_and_holds_back_its_moves() {
        let events = recorded_circle();
        let (edges, moves) = replay(&events, true);
        // Shift going down and up mid-hold is not another press.
        assert_eq!(edges, vec![ControlEdge::Pressed, ControlEdge::Released]);
        assert_eq!(moves.first(), Some(&PointerMove::Pass), "before the press");
        assert_eq!(moves.last(), Some(&PointerMove::Pass), "after the release");
        let held = &moves[1..moves.len() - 1];
        assert_eq!(held.len(), 77);
        assert!(held.iter().all(|m| *m == PointerMove::WatchAndHold));
    }

    #[test]
    fn moves_reach_the_apps_when_the_daemon_did_not_freeze() {
        // Not dictating: the circle is watched as before and nothing is held back.
        let (edges, moves) = replay(&recorded_circle(), false);
        assert_eq!(edges, vec![ControlEdge::Pressed, ControlEdge::Released]);
        assert!(!moves.contains(&PointerMove::WatchAndHold));
        assert_eq!(
            moves.iter().filter(|m| **m == PointerMove::Watch).count(),
            77
        );
    }

    #[test]
    fn a_missed_release_thaws_at_the_next_plain_move() {
        // The recorded hold with its release lost (the tap was disabled
        // or the release went to a secure-input field).
        let events: Vec<_> = recorded_circle()
            .into_iter()
            .filter(|&(t, flags)| {
                !(t == K_CG_EVENT_FLAGS_CHANGED && !is_mouse_modifier_active(flags))
            })
            .collect();
        let (edges, moves) = replay(&events, true);
        assert_eq!(edges, vec![ControlEdge::Pressed, ControlEdge::Released]);
        assert_eq!(moves.last(), Some(&PointerMove::Pass));
        // And the next press freezes again.
        let mut again = events.clone();
        again.extend(recorded_circle());
        let (edges, _) = replay(&again, true);
        assert_eq!(
            edges.iter().filter(|e| **e == ControlEdge::Pressed).count(),
            2
        );
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
