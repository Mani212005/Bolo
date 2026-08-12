use super::HotkeyListener;
use anyhow::Result;
use std::os::raw::c_void;
use std::sync::Arc;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

type EventTargetRef = *mut c_void;
type EventHandlerRef = *mut c_void;
type EventHotKeyRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;
type EventRef = *mut c_void;
type OSStatus = i32;

type EventHandlerProc = unsafe extern "C" fn(
    next_handler: EventHandlerCallRef,
    the_event: EventRef,
    user_data: *mut c_void,
) -> OSStatus;

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn InstallEventHandler(
        target: EventTargetRef,
        handler: EventHandlerProc,
        num_types: u32,
        list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut EventHandlerRef,
    ) -> OSStatus;
    fn RegisterEventHotKey(
        hot_key_code: u32,
        hot_key_modifiers: u32,
        hot_key_id: EventHotKeyID,
        target: EventTargetRef,
        options: u32,
        out_ref: *mut EventHotKeyRef,
    ) -> OSStatus;
    fn GetEventParameter(
        the_event: EventRef,
        param_name: u32,
        desired_type: u32,
        out_actual_type: *mut u32,
        buffer_size: usize,
        out_actual_size: *mut usize,
        out_data: *mut c_void,
    ) -> OSStatus;
    fn ReceiveNextEvent(
        in_num_types: u32,
        in_list: *const EventTypeSpec,
        in_timeout: f64,
        in_pull_event: bool,
        out_event: *mut EventRef,
    ) -> OSStatus;
    fn SendEventToEventTarget(in_event: EventRef, in_target: EventTargetRef) -> OSStatus;
    fn ReleaseEvent(in_event: EventRef);
}

// FourCC constants for Carbon events
const FOURCC_KEYB: u32 = u32::from_be_bytes(*b"keyb"); // kEventClassKeyboard
const FOURCC_DIRECT: u32 = u32::from_be_bytes(*b"----"); // kEventParamDirectObject
const FOURCC_HKID: u32 = u32::from_be_bytes(*b"hkid"); // typeEventHotKeyID
const FOURCC_BOLO: u32 = u32::from_be_bytes(*b"BOLO"); // signature

const EVENT_HOTKEY_PRESSED: u32 = 1; // kEventHotKeyPressed

// Carbon Modifier Keys
const MOD_CONTROL: u32 = 0x1000; // controlKey (4096)
const MOD_OPTION: u32 = 0x0800; // optionKey (2048)

// Carbon Keycodes
const KEY_SPACE: u32 = 49;
const KEY_V: u32 = 9;
const KEY_P: u32 = 35;
const KEY_I: u32 = 34;

// Hotkey IDs
const ID_TOGGLE: u32 = 1;
const ID_QUICK_SPLICE: u32 = 2;
const ID_PAUSE: u32 = 3;
const ID_INSERT_LAST: u32 = 4;

unsafe extern "C" fn hotkey_event_handler(
    _next_handler: EventHandlerCallRef,
    the_event: EventRef,
    user_data: *mut c_void,
) -> OSStatus {
    let callback = &*(user_data as *const Arc<Box<dyn Fn(&str) + Send + Sync>>);
    
    let mut hkid = EventHotKeyID::default();
    let mut actual_size: usize = 0;
    
    let status = GetEventParameter(
        the_event,
        FOURCC_DIRECT,
        FOURCC_HKID,
        std::ptr::null_mut(),
        std::mem::size_of::<EventHotKeyID>(),
        &mut actual_size,
        &mut hkid as *mut EventHotKeyID as *mut c_void,
    );
    
    if status == 0 {
        match hkid.id {
            ID_TOGGLE => {
                eprintln!("[macos-hotkey] Ctrl+Space received -> toggle");
                callback("toggle");
            }
            ID_QUICK_SPLICE => {
                eprintln!("[macos-hotkey] Option+V received -> quick-splice");
                callback("quick-splice");
            }
            ID_PAUSE => {
                eprintln!("[macos-hotkey] Option+P received -> pause");
                callback("pause");
            }
            ID_INSERT_LAST => {
                eprintln!("[macos-hotkey] Option+I received -> insert-last");
                callback("insert-last");
            }
            _ => {}
        }
    }
    
    0 // noErr
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
        
        std::thread::spawn(move || {
            unsafe {
                let target = GetApplicationEventTarget();
                
                let event_type = EventTypeSpec {
                    event_class: FOURCC_KEYB,
                    event_kind: EVENT_HOTKEY_PRESSED,
                };
                
                let user_data = Box::into_raw(Box::new(callback.clone())) as *mut c_void;
                let mut handler_ref: EventHandlerRef = std::ptr::null_mut();
                
                InstallEventHandler(
                    target,
                    hotkey_event_handler,
                    1,
                    &event_type,
                    user_data,
                    &mut handler_ref,
                );
                
                // 1. Ctrl + Space -> Toggle Start / Stop
                let mut ref_toggle: EventHotKeyRef = std::ptr::null_mut();
                RegisterEventHotKey(
                    KEY_SPACE,
                    MOD_CONTROL,
                    EventHotKeyID { signature: FOURCC_BOLO, id: ID_TOGGLE },
                    target,
                    0,
                    &mut ref_toggle,
                );
                
                // 2. Option + V -> Quick-Splice Clipboard
                let mut ref_splice: EventHotKeyRef = std::ptr::null_mut();
                RegisterEventHotKey(
                    KEY_V,
                    MOD_OPTION,
                    EventHotKeyID { signature: FOURCC_BOLO, id: ID_QUICK_SPLICE },
                    target,
                    0,
                    &mut ref_splice,
                );
                
                // 3. Option + P -> Pause / Resume Recording
                let mut ref_pause: EventHotKeyRef = std::ptr::null_mut();
                RegisterEventHotKey(
                    KEY_P,
                    MOD_OPTION,
                    EventHotKeyID { signature: FOURCC_BOLO, id: ID_PAUSE },
                    target,
                    0,
                    &mut ref_pause,
                );
                
                // 4. Option + I -> Re-Type Last Transcription
                let mut ref_insert: EventHotKeyRef = std::ptr::null_mut();
                RegisterEventHotKey(
                    KEY_I,
                    MOD_OPTION,
                    EventHotKeyID { signature: FOURCC_BOLO, id: ID_INSERT_LAST },
                    target,
                    0,
                    &mut ref_insert,
                );
                
                eprintln!("[macos-hotkey] Carbon hotkeys registered on ApplicationEventTarget");
                
                // Pump events continuously using ReceiveNextEvent
                let mut event: EventRef = std::ptr::null_mut();
                loop {
                    let status = ReceiveNextEvent(0, std::ptr::null(), 1.0, true, &mut event);
                    if status == 0 && !event.is_null() {
                        SendEventToEventTarget(event, target);
                        ReleaseEvent(event);
                        event = std::ptr::null_mut();
                    }
                }
            }
        });
        
        Ok(())
    }
}
