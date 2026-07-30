use anyhow::Result;

pub trait HotkeyListener: Send + Sync {
    /// Start listening for global hotkeys. 
    fn start(&self, callback: Box<dyn Fn(&str) + Send + Sync>) -> Result<()>;
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
