use super::HotkeyListener;
use anyhow::Result;
use rdev::{listen, Event, EventType, Key};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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
                            Key::ControlLeft | Key::ControlRight => ctrl_down.store(true, Ordering::SeqCst),
                            Key::Alt | Key::AltGr => alt_down.store(true, Ordering::SeqCst),
                            Key::Space => {
                                // Transition from false -> true prevents OS key repeat from sending multiple toggles!
                                if !space_down.swap(true, Ordering::SeqCst) {
                                    if ctrl_down.load(Ordering::SeqCst) {
                                        cb("toggle");
                                    }
                                }
                            }
                            Key::KeyP => {
                                if !p_down.swap(true, Ordering::SeqCst) {
                                    if alt_down.load(Ordering::SeqCst) {
                                        cb("pause");
                                    }
                                }
                            }
                            Key::KeyI => {
                                if !i_down.swap(true, Ordering::SeqCst) {
                                    if alt_down.load(Ordering::SeqCst) {
                                        cb("insert-last");
                                    }
                                }
                            }
                            Key::KeyV => {
                                if !v_down.swap(true, Ordering::SeqCst) {
                                    if alt_down.load(Ordering::SeqCst) {
                                        cb("quick-splice");
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    EventType::KeyRelease(key) => {
                        match key {
                            Key::ControlLeft | Key::ControlRight => ctrl_down.store(false, Ordering::SeqCst),
                            Key::Alt | Key::AltGr => alt_down.store(false, Ordering::SeqCst),
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
