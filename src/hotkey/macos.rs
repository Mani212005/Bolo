use super::HotkeyListener;
use anyhow::Result;
use rdev::{listen, Event, EventType, Key};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceKeyState(stateID: i32, key: u16) -> bool;
}

const COMBINED_SESSION_STATE: i32 = 0;
const HID_SYSTEM_STATE: i32 = 1;

/// Queries physical keyboard modifier state from macOS kernel.
/// Keycodes on macOS:
/// - 59: Left Control, 62: Right Control
/// - 58: Left Option,  61: Right Option
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
        
        let last_toggle = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(10)));
        let last_splice = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(10)));
        let last_pause = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(10)));
        let last_insert = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(10)));
        
        let cb = callback.clone();
        
        std::thread::spawn(move || {
            let callback_loop = move |event: Event| {
                match event.event_type {
                    EventType::KeyPress(key) => {
                        match key {
                            Key::ControlLeft | Key::ControlRight => ctrl_down.store(true, Ordering::SeqCst),
                            Key::Alt | Key::AltGr => alt_down.store(true, Ordering::SeqCst),
                            Key::Space => {
                                if ctrl_down.load(Ordering::SeqCst) || is_control_down() {
                                    let mut last = last_toggle.lock().unwrap();
                                    if last.elapsed() >= Duration::from_millis(300) {
                                        *last = Instant::now();
                                        cb("toggle");
                                    }
                                }
                            }
                            Key::KeyP => {
                                if alt_down.load(Ordering::SeqCst) || is_option_down() {
                                    let mut last = last_pause.lock().unwrap();
                                    if last.elapsed() >= Duration::from_millis(300) {
                                        *last = Instant::now();
                                        cb("pause");
                                    }
                                }
                            }
                            Key::KeyI => {
                                if alt_down.load(Ordering::SeqCst) || is_option_down() {
                                    let mut last = last_insert.lock().unwrap();
                                    if last.elapsed() >= Duration::from_millis(300) {
                                        *last = Instant::now();
                                        cb("insert-last");
                                    }
                                }
                            }
                            Key::KeyV => {
                                if alt_down.load(Ordering::SeqCst) || is_option_down() {
                                    let mut last = last_splice.lock().unwrap();
                                    if last.elapsed() >= Duration::from_millis(300) {
                                        *last = Instant::now();
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
