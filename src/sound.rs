use crate::config::Config;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy)]
pub enum Chime {
    Start,
    Stop,
}

impl Chime {
    fn file(self) -> Option<&'static str> {
        match self {
            Chime::Start => Some("49447089-game-start-317318.mp3"),
            Chime::Stop => None,
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Chime::Start => "start",
            Chime::Stop => "stop",
        }
    }
}

/// assets/ next to the repo the binary was built in (target/release/bolo ->
/// ../../assets), falling back to assets/ under the current directory.
fn asset_path(name: &str) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("../../assets").join(name));
        }
    }
    candidates.push(PathBuf::from("assets").join(name));
    candidates.into_iter().find(|p| p.exists())
}

/// Fire-and-forget chime via afplay on macOS or paplay on Linux; never blocks the caller.
/// spawned with std::process (not tokio) - see ClipboardInjector for why.
pub fn play(cfg: &Config, chime: Chime) {
    if !cfg.daemon.sounds {
        return;
    }
    let Some(file_name) = chime.file() else {
        return;
    };
    let Some(path) = asset_path(file_name) else {
        eprintln!("[sound] {} skipped: assets/{} not found", chime.as_str(), file_name);
        return;
    };
    let player = if cfg!(target_os = "macos") {
        "afplay"
    } else {
        "paplay"
    };

    match std::process::Command::new(player)
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => eprintln!("[sound] {}", chime.as_str()),
        Err(e) => eprintln!("[sound] {} failed: {e}", chime.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chime_mapping() {
        assert_eq!(Chime::Start.file(), Some("49447089-game-start-317318.mp3"));
        assert_eq!(Chime::Stop.file(), None);
    }
}
