use crate::config::Config;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy)]
pub enum Chime {
    Start,
    Stop,
}

impl Chime {
    fn file(self) -> &'static str {
        match self {
            Chime::Start => "start.wav",
            Chime::Stop => "stop.wav",
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Chime::Start => "start",
            Chime::Stop => "stop",
        }
    }
}

/// assets/ next to the repo the binary was built in (target/release/bolo →
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

/// Fire-and-forget chime via paplay; never blocks the caller. paplay is
/// spawned with std::process (not tokio) — see ClipboardInjector for why.
pub fn play(cfg: &Config, chime: Chime) {
    if !cfg.daemon.sounds {
        return;
    }
    let Some(path) = asset_path(chime.file()) else {
        eprintln!("[sound] {} skipped: assets/{} not found", chime.as_str(), chime.file());
        return;
    };
    match std::process::Command::new("paplay")
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => eprintln!("[sound] {}", chime.as_str()),
        Err(e) => eprintln!("[sound] {} failed: {e}", chime.as_str()),
    }
}
