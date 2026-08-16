use crate::config::Config;
use std::fs;
use std::path::PathBuf;

const START_MP3_BYTES: &[u8] = include_bytes!("../assets/49447089-game-start-317318.mp3");

#[derive(Debug, Clone, Copy)]
pub enum Chime {
    Start,
    Stop,
}

impl Chime {
    fn as_str(self) -> &'static str {
        match self {
            Chime::Start => "start",
            Chime::Stop => "stop",
        }
    }
}

/// Ensures the start audio MP3 is extracted from binary bytes to a temporary path.
fn get_start_audio_file() -> PathBuf {
    let tmp_path = std::env::temp_dir().join("bolo_start_chime.mp3");
    let needs_write = !tmp_path.exists()
        || fs::metadata(&tmp_path).map(|m| m.len()).unwrap_or(0) != START_MP3_BYTES.len() as u64;
    if needs_write {
        let _ = fs::write(&tmp_path, START_MP3_BYTES);
    }
    tmp_path
}

/// Fire-and-forget chime via afplay on macOS (at 50% volume) or paplay on Linux; never blocks the caller.
/// Checks the latest `sounds` setting dynamically from active config so UI toggle changes apply live.
pub fn play(cfg: &Config, chime: Chime) {
    // Dynamic real-time lookup of sounds preference from active config.toml
    let sounds_enabled = if let Some(home) = std::env::var_os("HOME") {
        let local_file = std::path::PathBuf::from("config.toml");
        let conf_file = std::path::PathBuf::from(home).join(".config/bolo/config.toml");
        let target = if local_file.exists() { &local_file } else { &conf_file };
        Config::load(target).map(|c| c.daemon.sounds).unwrap_or(cfg.daemon.sounds)
    } else {
        cfg.daemon.sounds
    };

    if !sounds_enabled {
        return;
    }

    let path = match chime {
        Chime::Start => get_start_audio_file(),
        Chime::Stop => return, // Stop chime is disabled as requested by Captain
    };

    let is_macos = cfg!(target_os = "macos");
    let player = if is_macos {
        "afplay"
    } else {
        "paplay"
    };

    let mut cmd = std::process::Command::new(player);
    if is_macos {
        // Set volume to 50% (-v 0.5) for crisp, clear, pleasant audio feedback
        cmd.arg("-v").arg("0.5");
    }
    cmd.arg(&path);
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    match cmd.spawn() {
        Ok(_) => eprintln!("[sound] playing {} via {player} (50% volume, {})", chime.as_str(), path.display()),
        Err(e) => eprintln!("[sound] {} failed with {player}: {e}", chime.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedded_audio_file() {
        let path = get_start_audio_file();
        assert!(path.exists(), "Embedded MP3 should be extracted to temp directory");
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            START_MP3_BYTES.len() as u64,
            "Temp MP3 file size must match embedded byte length"
        );
    }

    #[test]
    fn test_play_stop_is_noop() {
        if let Ok(cfg) = Config::load(std::path::Path::new("config.toml")) {
            play(&cfg, Chime::Stop);
        }
    }
}
