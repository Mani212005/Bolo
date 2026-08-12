use super::HotkeyListener;
use anyhow::{anyhow, Result};
use std::os::raw::c_void;
use std::sync::Arc;

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
const K_CG_EVENT_TAP_OPTION_LISTEN_ONLY: u32 = 1;

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
    if event_type == K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT || event_type == K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT {
        CGEventTapEnable(ctx.port, true);
        return event;
    }

    if event_type == K_CG_EVENT_KEY_DOWN {
        let keycode = CGEventGetIntegerValueField(event, K_CG_KEYBOARD_EVENT_KEYCODE);
        let flags = CGEventGetFlags(event);
        let autorepeat = CGEventGetIntegerValueField(event, K_CG_KEYBOARD_EVENT_AUTOREPEAT);

        // Kernel-level hardware repeat filtering:
        // When autorepeat != 0, it is an OS repeat while key is held -> DROP IMMEDIATELY!
        if autorepeat == 0 {
            let has_ctrl = (flags & K_CG_EVENT_FLAG_MASK_CONTROL) != 0;
            let has_opt = (flags & K_CG_EVENT_FLAG_MASK_ALTERNATE) != 0;
            let has_cmd = (flags & K_CG_EVENT_FLAG_MASK_COMMAND) != 0;

            // 1. Ctrl + Space (with NO Command held) -> Toggle Start / Stop
            if keycode == KEY_SPACE && has_ctrl && !has_cmd {
                eprintln!("[macos-hotkey] Ctrl+Space -> toggle");
                (ctx.callback)("toggle");
            }
            // 2. Option + V -> Quick-Splice Clipboard into dictation
            else if keycode == KEY_V && has_opt && !has_cmd {
                eprintln!("[macos-hotkey] Option+V -> quick-splice");
                (ctx.callback)("quick-splice");
            }
            // 3. Option + P -> Pause / Resume Recording
            else if keycode == KEY_P && has_opt && !has_cmd {
                eprintln!("[macos-hotkey] Option+P -> pause");
                (ctx.callback)("pause");
            }
            // 4. Option + I -> Re-Type Last Transcription
            else if keycode == KEY_I && has_opt && !has_cmd {
                eprintln!("[macos-hotkey] Option+I -> insert-last");
                (ctx.callback)("insert-last");
            }
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
                // Event mask: listen for KeyDown (1 << 10)
                let event_mask: u64 = 1 << K_CG_EVENT_KEY_DOWN;

                let ctx_box = Box::new(TapContext {
                    callback,
                    port: std::ptr::null_mut(),
                });
                let ctx_raw = Box::into_raw(ctx_box);

                let port = CGEventTapCreate(
                    K_CG_SESSION_EVENT_TAP,
                    K_CG_HEAD_INSERT_EVENT_TAP,
                    K_CG_EVENT_TAP_OPTION_LISTEN_ONLY,
                    event_mask,
                    event_tap_callback,
                    ctx_raw as *mut c_void,
                );

                if port.is_null() {
                    eprintln!("[macos-hotkey] CGEventTapCreate failed — ensure Accessibility permissions are granted to Bolo in macOS System Settings");
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
