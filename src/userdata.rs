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

// ---- dictation history (jsonl, one {ts, kind, text} per line) ----

fn history_path() -> PathBuf {
    data_dir().join("history.jsonl")
}

pub fn append_history(kind: &str, text: &str) {
    let _ = std::fs::create_dir_all(data_dir());
    let line = serde_json::json!({
        "ts": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "kind": kind,
        "text": text,
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
    let all: Vec<serde_json::Value> =
        text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let skip = all.len().saturating_sub(max);
    all.into_iter().skip(skip).collect()
}

pub fn clear_history() {
    let _ = std::fs::remove_file(history_path());
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
