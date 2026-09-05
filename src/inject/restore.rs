#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardItem {
    pub mime_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardSnapshot {
    pub items: Vec<ClipboardItem>,
    pub change_count: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreState {
    Idle,
    Snapshotted,
    Pasted { expected_change_count: Option<i64> },
    Restored,
    Skipped,
}

#[derive(Debug, Clone)]
pub struct ClipboardStateMachine {
    state: RestoreState,
    snapshot: Option<ClipboardSnapshot>,
}

impl ClipboardStateMachine {
    pub fn new() -> Self {
        Self {
            state: RestoreState::Idle,
            snapshot: None,
        }
    }

    #[allow(dead_code)]
    pub fn state(&self) -> RestoreState {
        self.state
    }

    pub fn snapshot(&self) -> Option<&ClipboardSnapshot> {
        self.snapshot.as_ref()
    }

    pub fn record_snapshot(&mut self, snapshot: Option<ClipboardSnapshot>) {
        if let Some(snap) = snapshot {
            if !snap.items.is_empty() {
                self.snapshot = Some(snap);
                self.state = RestoreState::Snapshotted;
                return;
            }
        }
        self.snapshot = None;
        self.state = RestoreState::Skipped;
    }

    pub fn record_paste(&mut self, post_change_count: Option<i64>) {
        if matches!(
            self.state,
            RestoreState::Snapshotted | RestoreState::Pasted { .. }
        ) {
            self.state = RestoreState::Pasted {
                expected_change_count: post_change_count,
            };
        }
    }

    pub fn should_restore(&self, current_change_count: Option<i64>) -> bool {
        match self.state {
            RestoreState::Pasted {
                expected_change_count,
            } => {
                if self.snapshot.is_none() {
                    return false;
                }
                match (expected_change_count, current_change_count) {
                    (Some(exp), Some(curr)) => exp == curr,
                    _ => true,
                }
            }
            _ => false,
        }
    }

    pub fn record_restored(&mut self) {
        if matches!(self.state, RestoreState::Pasted { .. }) {
            self.state = RestoreState::Restored;
        }
    }

    pub fn record_skipped(&mut self) {
        self.state = RestoreState::Skipped;
    }
}

pub fn snapshot_clipboard() -> Option<ClipboardSnapshot> {
    #[cfg(target_os = "macos")]
    {
        macos_pasteboard::snapshot()
    }
    #[cfg(target_os = "linux")]
    {
        linux_clipboard::snapshot()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

pub fn restore_clipboard(snapshot: &ClipboardSnapshot) -> bool {
    #[cfg(target_os = "macos")]
    {
        macos_pasteboard::restore(snapshot)
    }
    #[cfg(target_os = "linux")]
    {
        linux_clipboard::restore(snapshot)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = snapshot;
        false
    }
}

pub fn get_clipboard_change_count() -> Option<i64> {
    #[cfg(target_os = "macos")]
    {
        macos_pasteboard::get_change_count()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
pub mod macos_pasteboard {
    use super::{ClipboardItem, ClipboardSnapshot};
    use std::ffi::{c_void, CStr};
    use std::mem::transmute;

    type Id = *mut c_void;
    type Sel = *mut c_void;

    #[link(name = "AppKit", kind = "framework")]
    extern "C" {}
    #[link(name = "Foundation", kind = "framework")]
    extern "C" {
        fn objc_getClass(name: *const i8) -> Id;
        fn sel_registerName(name: *const i8) -> Sel;
        fn objc_msgSend();
    }

    pub fn get_change_count() -> Option<i64> {
        unsafe {
            let cls_pb = objc_getClass(b"NSPasteboard\0".as_ptr() as *const i8);
            let sel_gen = sel_registerName(b"generalPasteboard\0".as_ptr() as *const i8);
            let msg_send_id: unsafe extern "C" fn(Id, Sel) -> Id =
                transmute(objc_msgSend as unsafe extern "C" fn());
            let pb: Id = msg_send_id(cls_pb, sel_gen);
            if pb.is_null() {
                return None;
            }
            let sel_cc = sel_registerName(b"changeCount\0".as_ptr() as *const i8);
            let msg_send_int: unsafe extern "C" fn(Id, Sel) -> isize =
                transmute(objc_msgSend as unsafe extern "C" fn());
            let cc: isize = msg_send_int(pb, sel_cc);
            Some(cc as i64)
        }
    }

    pub fn snapshot() -> Option<ClipboardSnapshot> {
        unsafe {
            let cls_pb = objc_getClass(b"NSPasteboard\0".as_ptr() as *const i8);
            let sel_gen = sel_registerName(b"generalPasteboard\0".as_ptr() as *const i8);
            let msg_send_id: unsafe extern "C" fn(Id, Sel) -> Id =
                transmute(objc_msgSend as unsafe extern "C" fn());
            let pb: Id = msg_send_id(cls_pb, sel_gen);
            if pb.is_null() {
                return None;
            }

            let change_count = get_change_count();

            let sel_items = sel_registerName(b"pasteboardItems\0".as_ptr() as *const i8);
            let items_arr: Id = msg_send_id(pb, sel_items);
            if items_arr.is_null() {
                return None;
            }

            let sel_count = sel_registerName(b"count\0".as_ptr() as *const i8);
            let msg_send_uint: unsafe extern "C" fn(Id, Sel) -> usize =
                transmute(objc_msgSend as unsafe extern "C" fn());
            let count: usize = msg_send_uint(items_arr, sel_count);
            if count == 0 {
                return None;
            }

            let sel_obj_at = sel_registerName(b"objectAtIndex:\0".as_ptr() as *const i8);
            let sel_types = sel_registerName(b"types\0".as_ptr() as *const i8);
            let sel_utf8 = sel_registerName(b"UTF8String\0".as_ptr() as *const i8);
            let sel_data_for_type = sel_registerName(b"dataForType:\0".as_ptr() as *const i8);
            let sel_length = sel_registerName(b"length\0".as_ptr() as *const i8);
            let sel_bytes = sel_registerName(b"bytes\0".as_ptr() as *const i8);

            let msg_send_obj_at: unsafe extern "C" fn(Id, Sel, usize) -> Id =
                transmute(objc_msgSend as unsafe extern "C" fn());
            let msg_send_cstr: unsafe extern "C" fn(Id, Sel) -> *const i8 =
                transmute(objc_msgSend as unsafe extern "C" fn());
            let msg_send_id_arg: unsafe extern "C" fn(Id, Sel, Id) -> Id =
                transmute(objc_msgSend as unsafe extern "C" fn());
            let msg_send_bytes: unsafe extern "C" fn(Id, Sel) -> *const u8 =
                transmute(objc_msgSend as unsafe extern "C" fn());

            let mut captured_items = Vec::new();

            for i in 0..count {
                let item: Id = msg_send_obj_at(items_arr, sel_obj_at, i);
                if item.is_null() {
                    continue;
                }

                let types_arr: Id = msg_send_id(item, sel_types);
                if types_arr.is_null() {
                    continue;
                }

                let types_count: usize = msg_send_uint(types_arr, sel_count);

                for j in 0..types_count {
                    let type_obj: Id = msg_send_obj_at(types_arr, sel_obj_at, j);
                    if type_obj.is_null() {
                        continue;
                    }

                    let type_str_ptr: *const i8 = msg_send_cstr(type_obj, sel_utf8);
                    if type_str_ptr.is_null() {
                        continue;
                    }

                    let mime_type = CStr::from_ptr(type_str_ptr).to_string_lossy().to_string();

                    let data_obj: Id = msg_send_id_arg(item, sel_data_for_type, type_obj);
                    if data_obj.is_null() {
                        continue;
                    }

                    let len: usize = msg_send_uint(data_obj, sel_length);
                    if len == 0 {
                        continue;
                    }

                    let bytes_ptr: *const u8 = msg_send_bytes(data_obj, sel_bytes);
                    if bytes_ptr.is_null() {
                        continue;
                    }

                    let data = std::slice::from_raw_parts(bytes_ptr, len).to_vec();
                    captured_items.push(ClipboardItem { mime_type, data });
                }
            }

            if captured_items.is_empty() {
                None
            } else {
                Some(ClipboardSnapshot {
                    items: captured_items,
                    change_count,
                })
            }
        }
    }

    pub fn restore(snapshot: &ClipboardSnapshot) -> bool {
        unsafe {
            let cls_pb = objc_getClass(b"NSPasteboard\0".as_ptr() as *const i8);
            let sel_gen = sel_registerName(b"generalPasteboard\0".as_ptr() as *const i8);
            let msg_send_id: unsafe extern "C" fn(Id, Sel) -> Id =
                transmute(objc_msgSend as unsafe extern "C" fn());
            let pb: Id = msg_send_id(cls_pb, sel_gen);
            if pb.is_null() {
                return false;
            }

            let sel_clear = sel_registerName(b"clearContents\0".as_ptr() as *const i8);
            let msg_send_int: unsafe extern "C" fn(Id, Sel) -> isize =
                transmute(objc_msgSend as unsafe extern "C" fn());
            msg_send_int(pb, sel_clear);

            let cls_item = objc_getClass(b"NSPasteboardItem\0".as_ptr() as *const i8);
            let cls_nsstr = objc_getClass(b"NSString\0".as_ptr() as *const i8);
            let cls_nsdata = objc_getClass(b"NSData\0".as_ptr() as *const i8);

            let sel_alloc = sel_registerName(b"alloc\0".as_ptr() as *const i8);
            let sel_init = sel_registerName(b"init\0".as_ptr() as *const i8);
            let sel_str_utf8 = sel_registerName(b"stringWithUTF8String:\0".as_ptr() as *const i8);
            let sel_data_bytes = sel_registerName(b"dataWithBytes:length:\0".as_ptr() as *const i8);
            let sel_set_data = sel_registerName(b"setData:forType:\0".as_ptr() as *const i8);

            let item_alloc = msg_send_id(cls_item, sel_alloc);
            let pb_item: Id = msg_send_id(item_alloc, sel_init);

            let msg_send_str: unsafe extern "C" fn(Id, Sel, *const i8) -> Id =
                transmute(objc_msgSend as unsafe extern "C" fn());
            let msg_send_data: unsafe extern "C" fn(Id, Sel, *const u8, usize) -> Id =
                transmute(objc_msgSend as unsafe extern "C" fn());
            let msg_send_set_data: unsafe extern "C" fn(Id, Sel, Id, Id) -> bool =
                transmute(objc_msgSend as unsafe extern "C" fn());

            for item in &snapshot.items {
                let type_c_str = std::ffi::CString::new(item.mime_type.as_str()).unwrap();
                let type_nsstr: Id = msg_send_str(cls_nsstr, sel_str_utf8, type_c_str.as_ptr());

                let data_ns: Id = msg_send_data(
                    cls_nsdata,
                    sel_data_bytes,
                    item.data.as_ptr(),
                    item.data.len(),
                );

                msg_send_set_data(pb_item, sel_set_data, data_ns, type_nsstr);
            }

            let cls_arr = objc_getClass(b"NSMutableArray\0".as_ptr() as *const i8);
            let sel_arr = sel_registerName(b"array\0".as_ptr() as *const i8);
            let arr: Id = msg_send_id(cls_arr, sel_arr);

            let sel_add = sel_registerName(b"addObject:\0".as_ptr() as *const i8);
            let msg_send_add: unsafe extern "C" fn(Id, Sel, Id) =
                transmute(objc_msgSend as unsafe extern "C" fn());
            msg_send_add(arr, sel_add, pb_item);

            let sel_write = sel_registerName(b"writeObjects:\0".as_ptr() as *const i8);
            let msg_send_write: unsafe extern "C" fn(Id, Sel, Id) -> bool =
                transmute(objc_msgSend as unsafe extern "C" fn());
            let ok: bool = msg_send_write(pb, sel_write, arr);

            ok
        }
    }
}

#[cfg(target_os = "linux")]
pub mod linux_clipboard {
    use super::{ClipboardItem, ClipboardSnapshot};
    use std::io::Write;
    use std::process::{Command, Stdio};

    pub fn snapshot() -> Option<ClipboardSnapshot> {
        if let Ok(output) = Command::new("wl-paste").arg("--list-types").output() {
            if output.status.success() {
                let types_str = String::from_utf8_lossy(&output.stdout);
                let types: Vec<&str> = types_str
                    .lines()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect();

                let mut items = Vec::new();
                for t in types {
                    if let Ok(out) = Command::new("wl-paste").args(&["--type", t]).output() {
                        if out.status.success() && !out.stdout.is_empty() {
                            items.push(ClipboardItem {
                                mime_type: t.to_string(),
                                data: out.stdout,
                            });
                        }
                    }
                }

                if !items.is_empty() {
                    return Some(ClipboardSnapshot {
                        items,
                        change_count: None,
                    });
                }
            }
        }

        if let Ok(output) = Command::new("xclip")
            .args(&["-selection", "clipboard", "-o"])
            .output()
        {
            if output.status.success() && !output.stdout.is_empty() {
                return Some(ClipboardSnapshot {
                    items: vec![ClipboardItem {
                        mime_type: "text/plain".to_string(),
                        data: output.stdout,
                    }],
                    change_count: None,
                });
            }
        }

        None
    }

    pub fn restore(snapshot: &ClipboardSnapshot) -> bool {
        for item in &snapshot.items {
            let wl_res = Command::new("timeout")
                .arg("3")
                .args(&["wl-copy", "--type", &item.mime_type])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .and_then(|mut child| {
                    if let Some(mut stdin) = child.stdin.take() {
                        let _ = stdin.write_all(&item.data);
                    }
                    child.wait()
                });

            if let Ok(status) = wl_res {
                if status.success() {
                    continue;
                }
            }

            let x_res = Command::new("timeout")
                .arg("2")
                .args(&[
                    "xclip",
                    "-selection",
                    "clipboard",
                    "-target",
                    &item.mime_type,
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .and_then(|mut child| {
                    if let Some(mut stdin) = child.stdin.take() {
                        let _ = stdin.write_all(&item.data);
                    }
                    child.wait()
                });

            if let Ok(status) = x_res {
                if status.success() {
                    continue;
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clipboard_state_machine_flow() {
        let mut sm = ClipboardStateMachine::new();
        assert_eq!(sm.state(), RestoreState::Idle);

        // Record empty snapshot -> Skipped
        sm.record_snapshot(None);
        assert_eq!(sm.state(), RestoreState::Skipped);
        assert!(!sm.should_restore(Some(1)));

        // Record valid snapshot
        let dummy_snap = ClipboardSnapshot {
            items: vec![ClipboardItem {
                mime_type: "text/plain".to_string(),
                data: b"hello".to_vec(),
            }],
            change_count: Some(10),
        };
        sm.record_snapshot(Some(dummy_snap));
        assert_eq!(sm.state(), RestoreState::Snapshotted);

        // After paste, change count becomes 11
        sm.record_paste(Some(11));
        assert_eq!(
            sm.state(),
            RestoreState::Pasted {
                expected_change_count: Some(11)
            }
        );

        // Check matching change count -> should restore
        assert!(sm.should_restore(Some(11)));

        // Check mismatched change count (e.g. user copied new data) -> should not restore
        assert!(!sm.should_restore(Some(12)));

        // Complete restore
        sm.record_restored();
        assert_eq!(sm.state(), RestoreState::Restored);
    }
}
