use anyhow::Result;

pub trait HotkeyListener: Send + Sync {
    /// Start listening for global hotkeys. `callback` gets each command and
    /// says whether Bolo acted on it. For `freeze` that is whether the daemon
    /// confirmed the screen is frozen (the grab succeeded); for `mouse`, while
    /// a Control hold is on, whether it is still frozen. The listener keeps
    /// pointer moves from the apps only on a yes, so a declined or failed
    /// freeze, or one that ended with the dictation, never holds them back.
    fn start(&self, callback: Box<dyn Fn(&str) -> bool + Send + Sync>) -> Result<()>;
}

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

pub fn get_listener() -> Box<dyn HotkeyListener> {
    #[cfg(target_os = "linux")]
    return Box::new(linux::LinuxHotkeyListener::new());

    #[cfg(target_os = "macos")]
    return Box::new(macos::MacOsHotkeyListener::new());
}
