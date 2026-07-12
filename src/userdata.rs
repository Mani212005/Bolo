//! User-editable data files under ~/.config/bolo — the "settings box" until
//! a real UI exists. Read fresh on every use so edits apply without a
//! daemon restart.

use std::path::PathBuf;

pub fn config_dir() -> PathBuf {
    // HOME-based, not XDG_CONFIG_HOME: snap-spawned shells pollute XDG vars.
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config/bolo")
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
