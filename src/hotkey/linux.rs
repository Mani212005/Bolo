use super::HotkeyListener;
use anyhow::Result;

pub struct LinuxHotkeyListener;

impl LinuxHotkeyListener {
    pub fn new() -> Self {
        Self
    }
}

impl HotkeyListener for LinuxHotkeyListener {
    fn start(&self, _callback: Box<dyn Fn(&str) + Send + Sync>) -> Result<()> {
        // On Linux, hotkeys are handled via GNOME custom keybindings which directly
        // invoke the `bolo` CLI commands (e.g., `bolo toggle`).
        // Therefore, the listener inside the daemon does not need to actively poll or listen.
        Ok(())
    }
}
