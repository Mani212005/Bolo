use super::HotkeyListener;
use anyhow::Result;
use rdev::{listen, Event, EventType, Key};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceKeyState(stateID: i32, key: u16) -> bool;
}

const COMBINED_SESSION_STATE: i32 = 0;
const HID_SYSTEM_STATE: i32 = 1;

/// Queries physical keyboard key state directly from macOS kernel:
/// Keycodes on macOS:
/// - 59: Left Control, 62: Right Control
/// - 58: Left Option,  61: Right Option
/// - 49: Space
/// - 9:  V
/// - 35: P
/// - 34: I
#[inline]
fn is_control_down() -> bool {
    unsafe {
        CGEventSourceKeyState(COMBINED_SESSION_STATE, 59)
            || CGEventSourceKeyState(COMBINED_SESSION_STATE, 62)
            || CGEventSourceKeyState(HID_SYSTEM_STATE, 59)
            || CGEventSourceKeyState(HID_SYSTEM_STATE, 62)
    }
}

#[inline]
fn is_option_down() -> bool {
    unsafe {
        CGEventSourceKeyState(COMBINED_SESSION_STATE, 58)
            || CGEventSourceKeyState(COMBINED_SESSION_STATE, 61)
            || CGEventSourceKeyState(HID_SYSTEM_STATE, 58)
            || CGEventSourceKeyState(HID_SYSTEM_STATE, 61)
    }
}

#[inline]
fn is_space_down() -> bool {
    unsafe {
        CGEventSourceKeyState(COMBINED_SESSION_STATE, 49)
            || CGEventSourceKeyState(HID_SYSTEM_STATE, 49)
    }
}

#[inline]
fn is_v_down() -> bool {
    unsafe {
        CGEventSourceKeyState(COMBINED_SESSION_STATE, 9)
            || CGEventSourceKeyState(HID_SYSTEM_STATE, 9)
    }
}

#[inline]
fn is_p_down() -> bool {
    unsafe {
        CGEventSourceKeyState(COMBINED_SESSION_STATE, 35)
            || CGEventSourceKeyState(HID_SYSTEM_STATE, 35)
    }
}

#[inline]
fn is_i_down() -> bool {
    unsafe {
        CGEventSourceKeyState(COMBINED_SESSION_STATE, 34)
            || CGEventSourceKeyState(HID_SYSTEM_STATE, 34)
    }
}

pub struct MacOsHotkeyListener;

impl MacOsHotkeyListener {
    pub fn new() -> Self {
        Self
    }
}

impl HotkeyListener for MacOsHotkeyListener {
    fn start(&self, callback: Box<dyn Fn(&str) + Send + Sync>) -> Result<()> {
        let callback = Arc::new(callback);
        
        let ctrl_down = Arc::new(AtomicBool::new(false));
        let alt_down = Arc::new(AtomicBool::new(false));
        let space_armed = Arc::new(AtomicBool::new(true));
        let v_armed = Arc::new(AtomicBool::new(true));
        let p_armed = Arc::new(AtomicBool::new(true));
        let i_armed = Arc::new(AtomicBool::new(true));
        
        let cb = callback.clone();
        
        std::thread::spawn(move || {
            let callback_loop = move |event: Event| {
                match event.event_type {
                    EventType::KeyPress(key) => {
                        match key {
                            Key::ControlLeft | Key::ControlRight => {
                                ctrl_down.store(true, Ordering::SeqCst);
                            }
                            Key::Alt | Key::AltGr => {
                                alt_down.store(true, Ordering::SeqCst);
                            }
                            Key::Space => {
                                // Self-healing check: if hardware says Space is up, re-arm
                                if !is_space_down() {
                                    space_armed.store(true, Ordering::SeqCst);
                                }
                                
                                // Only fire if Space transitioned from Released -> Pressed (blocks all OS key-repeats)
                                if space_armed.swap(false, Ordering::SeqCst) {
                                    if ctrl_down.load(Ordering::SeqCst) || is_control_down() {
                                        cb("toggle");
                                    }
                                }
                            }
                            Key::KeyP => {
                                if !is_p_down() {
                                    p_armed.store(true, Ordering::SeqCst);
                                }
                                if p_armed.swap(false, Ordering::SeqCst) {
                                    if alt_down.load(Ordering::SeqCst) || is_option_down() {
                                        cb("pause");
                                    }
                                }
                            }
                            Key::KeyI => {
                                if !is_i_down() {
                                    i_armed.store(true, Ordering::SeqCst);
                                }
                                if i_armed.swap(false, Ordering::SeqCst) {
                                    if alt_down.load(Ordering::SeqCst) || is_option_down() {
                                        cb("insert-last");
                                    }
                                }
                            }
                            Key::KeyV => {
                                if !is_v_down() {
                                    v_armed.store(true, Ordering::SeqCst);
                                }
                                if v_armed.swap(false, Ordering::SeqCst) {
                                    if alt_down.load(Ordering::SeqCst) || is_option_down() {
                                        cb("quick-splice");
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    EventType::KeyRelease(key) => {
                        match key {
                            Key::ControlLeft | Key::ControlRight => {
                                if !is_control_down() {
                                    ctrl_down.store(false, Ordering::SeqCst);
                                }
                            }
                            Key::Alt | Key::AltGr => {
                                if !is_option_down() {
                                    alt_down.store(false, Ordering::SeqCst);
                                }
                            }
                            Key::Space => {
                                space_armed.store(true, Ordering::SeqCst);
                            }
                            Key::KeyP => {
                                p_armed.store(true, Ordering::SeqCst);
                            }
                            Key::KeyI => {
                                i_armed.store(true, Ordering::SeqCst);
                            }
                            Key::KeyV => {
                                v_armed.store(true, Ordering::SeqCst);
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            };
            
            if let Err(e) = listen(callback_loop) {
                eprintln!("[macos-hotkey] rdev listen error: {:?}", e);
            }
        });
        Ok(())
    }
}
