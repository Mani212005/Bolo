use super::HotkeyListener;
use anyhow::Result;
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
    callback: Box<dyn Fn(&str) + Send + Sync>,
    port: CFMachPortRef,
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

    if event_type == K_CG_EVENT_MOUSE_MOVED
        || event_type == K_CG_EVENT_LEFT_MOUSE_DRAGGED
        || event_type == K_CG_EVENT_RIGHT_MOUSE_DRAGGED
    {
        let loc = CGEventGetLocation(event);
        (ctx.callback)(&format!("mouse {:.1} {:.1}", loc.x, loc.y));
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
    fn start(&self, callback: Box<dyn Fn(&str) + Send + Sync>) -> Result<()> {
        std::thread::spawn(move || {
            unsafe {
                // Event mask: listen for KeyDown and Mouse movements/drags
                let event_mask: u64 = (1 << K_CG_EVENT_KEY_DOWN)
                    | (1 << K_CG_EVENT_MOUSE_MOVED)
                    | (1 << K_CG_EVENT_LEFT_MOUSE_DRAGGED)
                    | (1 << K_CG_EVENT_RIGHT_MOUSE_DRAGGED);

                let ctx_box = Box::new(TapContext {
                    callback,
                    port: std::ptr::null_mut(),
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
}
