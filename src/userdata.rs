//! User-editable data files under ~/.config/bolo — the "settings box" until
//! a real UI exists. Read fresh on every use so edits apply without a
//! daemon restart.

use std::path::PathBuf;

pub fn config_dir() -> PathBuf {
    // HOME-based, not XDG_CONFIG_HOME: snap-spawned shells pollute XDG vars.
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config/bolo")
}

/// ~/.local/share/bolo — models, venv, history, scratchpad.
pub fn data_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share/bolo")
}

// ---- dictation history (jsonl, one {id, ts, kind, text, audio_id, duration_s} per line) ----

fn history_path() -> PathBuf {
    data_dir().join("history.jsonl")
}

pub fn recordings_dir() -> PathBuf {
    data_dir().join("recordings")
}

pub fn save_recording_wav(id: &str, wav_bytes: &[u8]) -> std::io::Result<PathBuf> {
    let dir = recordings_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{id}.wav"));
    std::fs::write(&path, wav_bytes)?;
    Ok(path)
}

pub fn read_recording_wav(id: &str) -> Option<Vec<u8>> {
    let safe_id: String = id
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
        .collect();
    let filename = if safe_id.ends_with(".wav") {
        safe_id
    } else {
        format!("{safe_id}.wav")
    };
    let path = recordings_dir().join(filename);
    std::fs::read(path).ok()
}

pub fn append_history(kind: &str, text: &str, audio_id: Option<&str>, duration_s: Option<f64>) {
    let _ = std::fs::create_dir_all(data_dir());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let id = audio_id
        .map(String::from)
        .unwrap_or_else(|| format!("rec_{now_ms}"));
    let line = serde_json::json!({
        "id": id,
        "ts": now,
        "kind": kind,
        "text": text,
        "audio_id": audio_id,
        "duration_s": duration_s,
    });
    use std::io::Write;
    if let Ok(mut f) =
        std::fs::File::options().create(true).append(true).open(history_path())
    {
        let _ = writeln!(f, "{line}");
    }
}

pub fn read_history(max: usize) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(history_path()).unwrap_or_default();
    let all: Vec<serde_json::Value> = text
        .lines()
        .enumerate()
        .filter_map(|(i, l)| {
            let mut val: serde_json::Value = serde_json::from_str(l).ok()?;
            if val.get("id").is_none() || val["id"].is_null() {
                let ts = val.get("ts").and_then(|t| t.as_u64()).unwrap_or(0);
                val["id"] = serde_json::json!(format!("hist_{ts}_{i}"));
            }
            Some(val)
        })
        .collect();
    let skip = all.len().saturating_sub(max);
    all.into_iter().skip(skip).collect()
}

pub fn delete_history_item(id: &str) -> bool {
    let text = std::fs::read_to_string(history_path()).unwrap_or_default();
    let mut found = false;
    let remaining: Vec<String> = text
        .lines()
        .enumerate()
        .filter_map(|(i, l)| {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(l) {
                let item_id = val.get("id").and_then(|v| v.as_str()).map(String::from).unwrap_or_else(|| {
                    let ts = val.get("ts").and_then(|t| t.as_u64()).unwrap_or(0);
                    format!("hist_{ts}_{i}")
                });
                if item_id == id {
                    found = true;
                    let audio_path = recordings_dir().join(format!("{id}.wav"));
                    let _ = std::fs::remove_file(audio_path);
                    return None;
                }
            }
            Some(l.to_string())
        })
        .collect();

    if found {
        let _ = std::fs::write(history_path(), remaining.join("\n") + "\n");
    }
    found
}

pub fn clear_history() {
    let _ = std::fs::remove_file(history_path());
    let _ = std::fs::remove_dir_all(recordings_dir());
}

// ---- scratchpad ----

fn scratchpad_path() -> PathBuf {
    data_dir().join("scratchpad.md")
}

pub fn read_scratchpad() -> String {
    std::fs::read_to_string(scratchpad_path()).unwrap_or_default()
}

pub fn write_scratchpad(text: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir())?;
    std::fs::write(scratchpad_path(), text)
}

/// Rewrite a userdata file's body while keeping its leading # comment header.
pub fn write_keeping_comments(name: &str, body: &str) -> std::io::Result<()> {
    let path = config_dir().join(name);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let header: Vec<&str> =
        existing.lines().take_while(|l| l.trim_start().starts_with('#')).collect();
    let mut out = header.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(body.trim_end());
    out.push('\n');
    std::fs::write(&path, out)
}

const VOCAB_STARTER: &str = "\
# Bolo vocabulary — one term per line, exact spelling.
# These are fed to the speech-to-text engine so names and jargon come out
# right (e.g. Groq, Claude). Lines starting with # are ignored.
Groq
Claude
Bolo
";

const ENHANCE_STARTER: &str = "\
# Bolo enhance prompt — everything below the comment lines becomes the
# system prompt for the Enhance button. Describe the structure you want
# your enhanced prompts to follow. Leave only comments (or delete the
# file) to use the built-in default.
";

/// Create the starter files on first run so the user can discover and edit
/// them.
pub fn ensure_starter_files() {
    let dir = config_dir();
    let _ = std::fs::create_dir_all(&dir);
    for (name, content) in [("vocabulary.txt", VOCAB_STARTER), ("enhance_prompt.txt", ENHANCE_STARTER)] {
        let path = dir.join(name);
        if !path.exists() {
            if std::fs::write(&path, content).is_ok() {
                eprintln!("[userdata] created {}", path.display());
            }
        }
    }
}

fn read_uncommented(name: &str) -> Option<String> {
    let text = std::fs::read_to_string(config_dir().join(name)).ok()?;
    let body: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    if body.is_empty() {
        None
    } else {
        Some(body.join("\n"))
    }
}

/// The glossary handed to every STT backend as a biasing prompt. Formatted
/// as a bare term list (reads like prior transcript, not an instruction):
/// whisper-style models can echo prompt text into the output on near-silent
/// audio, and a plain list keeps that failure mode small.
pub fn vocabulary_prompt() -> Option<String> {
    read_uncommented("vocabulary.txt")
        .map(|terms| format!("{}.", terms.lines().collect::<Vec<_>>().join(", ")))
}

/// Custom system prompt for Enhance, if the user wrote one.
pub fn enhance_prompt() -> Option<String> {
    read_uncommented("enhance_prompt.txt")
}
