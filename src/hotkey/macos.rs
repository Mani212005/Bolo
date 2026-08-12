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

/// Direct hardware state query via macOS CoreGraphics:
/// Keycodes on macOS:
/// - 59: Left Control
/// - 62: Right Control
/// - 58: Left Option / Alt
/// - 61: Right Option / Alt
#[inline]
fn is_control_down() -> bool {
    unsafe {
        CGEventSourceKeyState(COMBINED_SESSION_STATE, 59) || 
        CGEventSourceKeyState(COMBINED_SESSION_STATE, 62)
    }
}

#[inline]
fn is_option_down() -> bool {
    unsafe {
        CGEventSourceKeyState(COMBINED_SESSION_STATE, 58) || 
        CGEventSourceKeyState(COMBINED_SESSION_STATE, 61)
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
        
        let space_down = Arc::new(AtomicBool::new(false));
        let v_down = Arc::new(AtomicBool::new(false));
        let p_down = Arc::new(AtomicBool::new(false));
        let i_down = Arc::new(AtomicBool::new(false));
        
        let cb = callback.clone();
        
        std::thread::spawn(move || {
            let callback_loop = move |event: Event| {
                match event.event_type {
                    EventType::KeyPress(key) => {
                        match key {
                            Key::Space => {
                                // Only fire toggle if Space transitioned from up->down AND Control is physically held!
                                if !space_down.swap(true, Ordering::SeqCst) {
                                    if is_control_down() {
                                        cb("toggle");
                                    }
                                }
                            }
                            Key::KeyP => {
                                if !p_down.swap(true, Ordering::SeqCst) {
                                    if is_option_down() {
                                        cb("pause");
                                    }
                                }
                            }
                            Key::KeyI => {
                                if !i_down.swap(true, Ordering::SeqCst) {
                                    if is_option_down() {
                                        cb("insert-last");
                                    }
                                }
                            }
                            Key::KeyV => {
                                if !v_down.swap(true, Ordering::SeqCst) {
                                    if is_option_down() {
                                        cb("quick-splice");
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    EventType::KeyRelease(key) => {
                        match key {
                            Key::Space => space_down.store(false, Ordering::SeqCst),
                            Key::KeyP => p_down.store(false, Ordering::SeqCst),
                            Key::KeyI => i_down.store(false, Ordering::SeqCst),
                            Key::KeyV => v_down.store(false, Ordering::SeqCst),
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
