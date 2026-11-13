use super::HotkeyListener;
use anyhow::Result;
use std::collections::HashSet;
use std::io::Read;
use std::path::PathBuf;

/// On Linux the dictation hotkeys are GNOME custom keybindings that run the
/// `bolo` CLI (`bolo toggle`, ...). The daemon itself only watches for the
/// screenshot trigger: Control tapped on its own. A keybinding cannot be a
/// bare modifier, so the keyboards are read directly from evdev, which needs
/// read access to `/dev/input/event*` (the `input` group). Reading is passive:
/// keys still reach the focused app and no shortcut is taken away.
pub struct LinuxHotkeyListener;

impl LinuxHotkeyListener {
    pub fn new() -> Self {
        Self
    }
}

impl HotkeyListener for LinuxHotkeyListener {
    fn start(&self, callback: Box<dyn Fn(&str) + Send + Sync>) -> Result<()> {
        let devices = std::fs::read_to_string("/proc/bus/input/devices")
            .map(|text| keyboard_event_devices(&text))
            .unwrap_or_default();
        let callback = std::sync::Arc::new(callback);
        let mut watching = 0;
        for device in devices {
            match std::fs::File::open(&device) {
                Ok(file) => {
                    watching += 1;
                    let callback = std::sync::Arc::clone(&callback);
                    std::thread::spawn(move || watch_keyboard(file, &device, &*callback));
                }
                Err(e) => eprintln!("[linux-hotkey] cannot read {}: {e}", device.display()),
            }
        }
        if watching == 0 {
            eprintln!(
                "[linux-hotkey] no readable keyboard, so tapping Ctrl will not capture the screen. \
                 Add yourself to the input group (sudo usermod -aG input $USER, then log in again), \
                 or bind `bolo capture` to a key in GNOME Settings > Keyboard."
            );
        } else {
            eprintln!("[linux-hotkey] watching {watching} keyboard(s): tap Ctrl while dictating to capture the screen");
        }
        Ok(())
    }
}

fn watch_keyboard(
    mut file: std::fs::File,
    device: &std::path::Path,
    callback: &(dyn Fn(&str) + Send + Sync),
) {
    let mut tap = ControlTap::default();
    let mut buf = [0u8; INPUT_EVENT_SIZE * 64];
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) => {
                eprintln!("[linux-hotkey] stopped reading {}: {e}", device.display());
                return;
            }
        };
        for raw in buf[..n].chunks_exact(INPUT_EVENT_SIZE) {
            if let Some(event) = KeyEvent::parse(raw) {
                if tap.feed(event) {
                    callback("capture");
                }
            }
        }
    }
}

/// `struct input_event`: a `timeval` (two native longs), then u16 type,
/// u16 code and i32 value.
const LONG: usize = std::mem::size_of::<std::os::raw::c_long>();
const INPUT_EVENT_SIZE: usize = 2 * LONG + 8;
const EV_KEY: u16 = 0x01;
const KEY_LEFTCTRL: u16 = 29;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_SPACE: u16 = 57;
const KEY_A: u16 = 30;
/// A press held longer than this is a held modifier, not a tap.
const TAP_MAX_MS: u64 = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeyEvent {
    code: u16,
    /// 0 release, 1 press, 2 autorepeat.
    value: i32,
    /// Kernel timestamp, so taps are timed without reading the clock.
    time_ms: u64,
}

impl KeyEvent {
    fn parse(raw: &[u8]) -> Option<Self> {
        if raw.len() != INPUT_EVENT_SIZE {
            return None;
        }
        let long = |i: usize| -> i64 {
            let bytes = &raw[i * LONG..(i + 1) * LONG];
            if LONG == 8 {
                i64::from_ne_bytes(bytes.try_into().unwrap())
            } else {
                i32::from_ne_bytes(bytes.try_into().unwrap()) as i64
            }
        };
        let rest = &raw[2 * LONG..];
        let kind = u16::from_ne_bytes([rest[0], rest[1]]);
        if kind != EV_KEY {
            return None;
        }
        Some(Self {
            code: u16::from_ne_bytes([rest[2], rest[3]]),
            value: i32::from_ne_bytes([rest[4], rest[5], rest[6], rest[7]]),
            time_ms: (long(0).max(0) as u64) * 1000 + (long(1).max(0) as u64) / 1000,
        })
    }
}

/// Recognises Control pressed and released on its own, quickly.
#[derive(Debug, Default)]
struct ControlTap {
    pressed_at: Option<u64>,
    held: HashSet<u16>,
}

impl ControlTap {
    fn feed(&mut self, event: KeyEvent) -> bool {
        let is_ctrl = event.code == KEY_LEFTCTRL || event.code == KEY_RIGHTCTRL;
        match (is_ctrl, event.value) {
            (true, 1) => {
                self.pressed_at = self.held.is_empty().then_some(event.time_ms);
                self.held.insert(event.code);
                false
            }
            (true, 0) => {
                self.held.remove(&event.code);
                self.pressed_at
                    .take()
                    .filter(|_| self.held.is_empty())
                    .is_some_and(|t| event.time_ms.saturating_sub(t) <= TAP_MAX_MS)
            }
            (true, _) => false,
            (false, 1) => {
                self.pressed_at = None;
                self.held.insert(event.code);
                false
            }
            (false, 0) => {
                self.held.remove(&event.code);
                false
            }
            (false, _) => false,
        }
    }
}

/// The `/dev/input/eventN` nodes of real keyboards in
/// `/proc/bus/input/devices`: devices with a Control key, a Space bar and
/// letter keys (power buttons and hotkey panels have none).
fn keyboard_event_devices(proc_devices: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for block in proc_devices.split("\n\n") {
        let mut handler = None;
        let mut keys: Option<Vec<u64>> = None;
        for line in block.lines() {
            if let Some(h) = line.strip_prefix("H: Handlers=") {
                handler = h
                    .split_whitespace()
                    .find(|w| w.starts_with("event"))
                    .map(str::to_string);
            } else if let Some(bits) = line.strip_prefix("B: KEY=") {
                // Hex words, most significant first, one per native long.
                let words: Option<Vec<u64>> = bits
                    .split_whitespace()
                    .rev()
                    .map(|w| u64::from_str_radix(w, 16).ok())
                    .collect();
                keys = words;
            }
        }
        let has = |code: u16| {
            let bits = (LONG * 8) as u16;
            keys.as_ref()
                .and_then(|k| k.get((code / bits) as usize))
                .is_some_and(|w| w >> (code % bits) & 1 == 1)
        };
        if let Some(handler) = handler {
            if has(KEY_LEFTCTRL) && has(KEY_SPACE) && has(KEY_A) {
                out.push(PathBuf::from("/dev/input").join(handler));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: u16, value: i32, time_ms: u64) -> KeyEvent {
        KeyEvent {
            code,
            value,
            time_ms,
        }
    }

    fn taps(events: &[KeyEvent]) -> usize {
        let mut tap = ControlTap::default();
        events.iter().filter(|e| tap.feed(**e)).count()
    }

    #[test]
    fn a_quick_lone_control_is_a_tap() {
        assert_eq!(
            taps(&[key(KEY_LEFTCTRL, 1, 1000), key(KEY_LEFTCTRL, 0, 1120)]),
            1
        );
        assert_eq!(
            taps(&[key(KEY_RIGHTCTRL, 1, 0), key(KEY_RIGHTCTRL, 0, 90)]),
            1
        );
    }

    #[test]
    fn control_chords_and_long_holds_are_not_taps() {
        // Ctrl+Space (Bolo's own toggle) and Ctrl+C.
        assert_eq!(
            taps(&[
                key(KEY_LEFTCTRL, 1, 0),
                key(KEY_SPACE, 1, 50),
                key(KEY_SPACE, 0, 90),
                key(KEY_LEFTCTRL, 0, 120),
            ]),
            0
        );
        // Held with autorepeat past the tap limit.
        assert_eq!(
            taps(&[
                key(KEY_LEFTCTRL, 1, 0),
                key(KEY_LEFTCTRL, 2, 500),
                key(KEY_LEFTCTRL, 0, 900),
            ]),
            0
        );
        // A release with no press seen (daemon started mid-press).
        assert_eq!(taps(&[key(KEY_LEFTCTRL, 0, 10)]), 0);
    }

    #[test]
    fn control_tap_is_rejected_when_another_key_is_held() {
        assert_eq!(
            taps(&[
                key(KEY_SPACE, 1, 0),
                key(KEY_LEFTCTRL, 1, 20),
                key(KEY_LEFTCTRL, 0, 60),
                key(KEY_SPACE, 0, 90),
            ]),
            0
        );
        assert_eq!(
            taps(&[
                key(KEY_LEFTCTRL, 1, 0),
                key(KEY_RIGHTCTRL, 1, 20),
                key(KEY_RIGHTCTRL, 0, 60),
                key(KEY_LEFTCTRL, 0, 90),
            ]),
            0
        );
    }

    #[test]
    fn taps_repeat_after_a_chord() {
        assert_eq!(
            taps(&[
                key(KEY_LEFTCTRL, 1, 0),
                key(KEY_SPACE, 1, 30),
                key(KEY_LEFTCTRL, 0, 60),
                key(KEY_LEFTCTRL, 1, 1000),
                key(KEY_LEFTCTRL, 0, 1100),
            ]),
            1
        );
    }

    #[test]
    fn raw_input_events_parse() {
        let mut raw = Vec::new();
        let push_long = |raw: &mut Vec<u8>, v: i64| {
            if LONG == 8 {
                raw.extend_from_slice(&v.to_ne_bytes());
            } else {
                raw.extend_from_slice(&(v as i32).to_ne_bytes());
            }
        };
        push_long(&mut raw, 12);
        push_long(&mut raw, 345_678);
        raw.extend_from_slice(&EV_KEY.to_ne_bytes());
        raw.extend_from_slice(&KEY_LEFTCTRL.to_ne_bytes());
        raw.extend_from_slice(&1i32.to_ne_bytes());
        assert_eq!(KeyEvent::parse(&raw), Some(key(KEY_LEFTCTRL, 1, 12_345)));
        // Non-key events (EV_SYN, EV_MSC) are skipped.
        let at = 2 * LONG;
        raw[at..at + 2].copy_from_slice(&4u16.to_ne_bytes());
        assert_eq!(KeyEvent::parse(&raw), None);
        assert_eq!(KeyEvent::parse(&raw[1..]), None);
    }

    /// Excerpt of `/proc/bus/input/devices` from a 64-bit laptop.
    const PROC_DEVICES: &str = "\
I: Bus=0019 Vendor=0000 Product=0001 Version=0000
N: Name=\"Power Button\"
H: Handlers=kbd event2
B: PROP=0
B: EV=3
B: KEY=8000 10000000000000 0

I: Bus=0011 Vendor=0001 Product=0001 Version=ab83
N: Name=\"AT Translated Set 2 keyboard\"
H: Handlers=sysrq kbd event3 leds
B: PROP=0
B: EV=120013
B: KEY=20000 20 0 0 1500f02100002 3803078f900d401 feffffdfffefffff fffffffffffffffe
B: MSC=10
B: LED=7

I: Bus=0018 Vendor=04f3 Product=315d Version=0100
N: Name=\"ELAN0790:00 04F3:315D Touchpad\"
H: Handlers=mouse1 event5
B: PROP=5
B: EV=1b
B: KEY=e520 10000 0 0 0 0
B: ABS=2e0800000000003
B: MSC=20
";

    #[test]
    fn only_full_keyboards_are_watched() {
        if LONG != 8 {
            return; // the fixture's words are 64-bit
        }
        assert_eq!(
            keyboard_event_devices(PROC_DEVICES),
            vec![PathBuf::from("/dev/input/event3")]
        );
    }
}
