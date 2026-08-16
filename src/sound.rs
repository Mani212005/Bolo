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
    if !tmp_path.exists() {
        let _ = fs::write(&tmp_path, START_MP3_BYTES);
    }
    tmp_path
}

/// Fire-and-forget chime via afplay on macOS (at 30% volume) or paplay on Linux; never blocks the caller.
pub fn play(cfg: &Config, chime: Chime) {
    if !cfg.daemon.sounds {
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
        // Reduce volume to 30% (-v 0.3) for a pleasant, non-jarring audio chime
        cmd.arg("-v").arg("0.3");
    }
    cmd.arg(&path);
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    match cmd.spawn() {
        Ok(_) => eprintln!("[sound] playing {} via {player} (30% volume)", chime.as_str()),
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
    }

    #[test]
    fn test_play_stop_is_noop() {
        if let Ok(cfg) = Config::load(std::path::Path::new("config.toml")) {
            play(&cfg, Chime::Stop);
        }
    }
}
