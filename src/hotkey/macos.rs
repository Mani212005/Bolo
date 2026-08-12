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
        
        let cb = callback.clone();
        
        std::thread::spawn(move || {
            let callback_loop = move |event: Event| {
                match event.event_type {
                    EventType::KeyPress(key) => {
                        match key {
                            Key::ControlLeft | Key::ControlRight => ctrl_down.store(true, Ordering::SeqCst),
                            Key::Alt | Key::AltGr => alt_down.store(true, Ordering::SeqCst),
                            Key::Space => {
                                if ctrl_down.load(Ordering::SeqCst) {
                                    cb("toggle");
                                }
                            }
                            Key::KeyP => {
                                if alt_down.load(Ordering::SeqCst) {
                                    cb("pause");
                                }
                            }
                            Key::KeyI => {
                                if alt_down.load(Ordering::SeqCst) {
                                    cb("insert-last");
                                }
                            }
                            Key::KeyV => {
                                if alt_down.load(Ordering::SeqCst) {
                                    cb("quick-splice");
                                }
                            }
                            Key::KeyC => {
                                if alt_down.load(Ordering::SeqCst) {
                                    cb("copy-splice");
                                }
                            }
                            _ => {}
                        }
                    }
                    EventType::KeyRelease(key) => {
                        match key {
                            Key::ControlLeft | Key::ControlRight => ctrl_down.store(false, Ordering::SeqCst),
                            Key::Alt | Key::AltGr => alt_down.store(false, Ordering::SeqCst),
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
