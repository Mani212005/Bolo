use serde::Deserialize;
use std::path::Path;

/// Single source of truth for the pipeline sample rate (grill item #3).
/// The resampler target, the VAD input rate, and the WAV header all read this.
pub const PIPELINE_SAMPLE_RATE: u32 = 16_000;

/// Silero VAD chunk size at 16kHz. The crate docs mandate exactly 512
/// samples per chunk for a 16kHz stream (grill item #2).
pub const VAD_CHUNK_SIZE: usize = 512;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub groq: GroqConfig,
    pub vad: VadConfig,
    #[serde(default)]
    pub stt: SttConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub inject: InjectConfig,
    #[serde(default)]
    pub enhance: EnhanceConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub vocab: VocabConfig,
    #[serde(default)]
    pub vision: VisionConfig,
    #[serde(default)]
    pub formatting: FormattingConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FormattingConfig {
    /// Detect pasted code and put it in code blocks.
    pub smart_code: bool,
    /// Break long dictated prose into paragraphs.
    pub paragraphs: bool,
    /// Format lists when asked by name ("todo list", "bullet points") or,
    /// with Jev's confirmation, when enumerated ("first... second...").
    pub list_cues: bool,
    /// Apps (name or bundle id substrings) that get bare code, no fences.
    pub raw_code_apps: Vec<String>,
    /// Apps that get ``` fences even if Bolo would treat them as editors.
    pub fenced_code_apps: Vec<String>,
    #[serde(default)]
    pub jev: JevConfig,
}

impl Default for FormattingConfig {
    fn default() -> Self {
        Self {
            smart_code: true,
            paragraphs: true,
            list_cues: true,
            raw_code_apps: Vec::new(),
            fenced_code_apps: Vec::new(),
            jev: JevConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct JevConfig {
    pub enabled: bool,
    /// `typesafe` or `openrouter`; when unset, inferred from whichever key is found.
    pub provider: Option<crate::jev::JevProvider>,
    /// Model id; empty means the provider's default.
    pub model: String,
    pub timeout_ms: u64,
    pub api_key: Option<String>,
}

impl Default for JevConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: None,
            model: String::new(),
            timeout_ms: crate::jev::DEFAULT_TIMEOUT_MS,
            api_key: None,
        }
    }
}

/// A fully resolved Jev target: where to send, with which key and model.
#[derive(Debug, Clone, PartialEq)]
pub struct JevTarget {
    pub provider: crate::jev::JevProvider,
    pub api_key: String,
    pub model: String,
}

impl JevConfig {
    /// Resolves provider, API key, and model. The key comes from, in order:
    /// 1. Config `api_key` (provider inferred from its shape when unset)
    /// 2. The provider's env var (`TYPESAFE_API_KEY` / `OPENROUTER_API_KEY`)
    /// 3. For OpenRouter, the saved key in ~/.config/bolo/openrouter_api_key.txt
    /// 4. The provider's variable in ~/.env
    ///
    /// With no provider configured, TypeSafe sources are tried before OpenRouter.
    pub fn resolve(&self) -> Option<JevTarget> {
        use crate::jev::JevProvider;
        let config_key = self
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty());
        let (provider, api_key) = match (self.provider, config_key) {
            (Some(p), Some(k)) => (p, k.to_string()),
            (None, Some(k)) => (JevProvider::for_key(k), k.to_string()),
            (Some(p), None) => (p, provider_key(p)?),
            (None, None) => [JevProvider::TypeSafe, JevProvider::OpenRouter]
                .into_iter()
                .find_map(|p| provider_key(p).map(|k| (p, k)))?,
        };
        Some(JevTarget {
            provider,
            api_key,
            model: self.model_for(provider),
        })
    }

    /// The configured model, unless it is empty or an OpenRouter-style
    /// `typesafe/...` id that the TypeSafe API rejects as unknown.
    fn model_for(&self, provider: crate::jev::JevProvider) -> String {
        let model = self.model.trim();
        let incompatible =
            provider == crate::jev::JevProvider::TypeSafe && model.starts_with("typesafe/");
        if model.is_empty() || incompatible {
            provider.default_model().to_string()
        } else {
            model.to_string()
        }
    }
}

/// Looks up a provider's key outside the config file.
fn provider_key(provider: crate::jev::JevProvider) -> Option<String> {
    let var = provider.api_key_env();
    if let Ok(key) = std::env::var(var) {
        let trimmed = key.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if provider == crate::jev::JevProvider::OpenRouter {
        if let Some(key) = crate::userdata::read_saved_openrouter_api_key() {
            return Some(key);
        }
    }
    let home = std::env::var_os("HOME")?;
    let content = std::fs::read_to_string(std::path::PathBuf::from(home).join(".env")).ok()?;
    let prefix = format!("{var}=");
    content.lines().find_map(|line| {
        let clean = line
            .trim()
            .strip_prefix(&prefix)?
            .trim_matches('"')
            .trim_matches('\'')
            .trim();
        (!clean.is_empty()).then(|| clean.to_string())
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VocabConfig {
    pub enabled: bool,
}

impl Default for VocabConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VisionConfig {
    /// Enable hover-only pointer-guided visual screen context capture.
    pub enabled: bool,
    /// Minimum circle angle in degrees (default: 315).
    pub min_angle_degrees: f64,
}

impl Default for VisionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_angle_degrees: 315.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// Port for the settings app, served on 127.0.0.1 only.
    pub port: u16,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self { port: 4525 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SttConfig {
    pub provider: SttBackend,
    pub whisper: WhisperConfig,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            provider: SttBackend::Groq,
            whisper: WhisperConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SttBackend {
    Groq,
    Whisper,
    #[serde(rename = "faster-whisper")]
    FasterWhisper,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WhisperConfig {
    /// ggml model name as published in ggerganov/whisper.cpp on Hugging Face
    /// (e.g. "large-v3-turbo", "small.en", "base.en").
    pub model: String,
}

impl Default for WhisperConfig {
    fn default() -> Self {
        Self {
            model: "large-v3-turbo".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EnhanceConfig {
    pub model: String,
}

impl Default for EnhanceConfig {
    fn default() -> Self {
        Self {
            model: "llama-3.3-70b-versatile".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    pub notifications: bool,
    pub sounds: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            notifications: true,
            sounds: true,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_restore_delay_ms() -> u64 {
    300
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct InjectConfig {
    pub method: InjectMethod,
    pub type_delay_ms: u64,
    #[serde(default = "default_true")]
    pub restore_clipboard: bool,
    #[serde(default = "default_restore_delay_ms")]
    pub restore_delay_ms: u64,
    pub terminal: TerminalPasteConfig,
}

impl Default for InjectConfig {
    fn default() -> Self {
        Self {
            method: InjectMethod::Paste,
            type_delay_ms: 2,
            restore_clipboard: true,
            restore_delay_ms: 300,
            terminal: TerminalPasteConfig::default(),
        }
    }
}

/// How dictation is pasted into terminals. Terminal agents collapse a large
/// paste into a placeholder, so Bolo sends several small pastes instead.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct TerminalPasteConfig {
    /// Paste into terminals as several small pastes so agents show the text.
    pub split_paste: bool,
    /// Per piece, in UTF-16 units (Claude Code collapses above 800, agy above 1000).
    pub max_paste_chars: usize,
    /// Line breaks per piece (Claude Code collapses above 2; keep >= 2 for blank lines).
    pub max_paste_newlines: usize,
    /// Pause between pieces so the terminal reads one before the next is copied.
    pub settle_ms: u64,
    /// Above this many text pieces, paste once as before.
    pub max_pieces: usize,
    /// Also treat these apps (name or bundle id substrings) as terminals.
    pub extra_apps: Vec<String>,
    /// Never treat these apps as terminals; wins over everything.
    pub exclude_apps: Vec<String>,
}

impl Default for TerminalPasteConfig {
    fn default() -> Self {
        Self {
            split_paste: true,
            max_paste_chars: 800,
            max_paste_newlines: 2,
            settle_ms: 50,
            max_pieces: 40,
            extra_apps: Vec::new(),
            exclude_apps: Vec::new(),
        }
    }
}

impl TerminalPasteConfig {
    /// The per-piece budget, clamped so every piece can hold at least one unit.
    pub fn budget(&self) -> crate::inject::split::PasteBudget {
        crate::inject::split::PasteBudget {
            max_units: self.max_paste_chars.max(1),
            max_breaks: self.max_paste_newlines.max(1),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InjectMethod {
    /// Clipboard + portal-synthesized Ctrl+V: whole text appears at once.
    Paste,
    /// Portal keystroke typing, char by char.
    Portal,
    /// Clipboard only; the user pastes manually.
    Clipboard,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GroqConfig {
    pub model: String,
    pub language: String,
    pub temperature: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VadConfig {
    pub speech_threshold: f32,
    pub endpoint_silence_ms: u64,
    pub min_speech_ms: u64,
    pub preroll_ms: u64,
    pub max_utterance_ms: u64,
    /// When false, trailing silence never ends a recording — only
    /// Ctrl+Space, Alt+P, or the max_utterance_ms cap do.
    #[serde(default = "default_false")]
    pub auto_endpoint: bool,
}

fn default_false() -> bool {
    false
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
        Ok(toml::from_str(&text)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inject_from(toml_text: &str) -> InjectConfig {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            inject: InjectConfig,
        }
        toml::from_str::<Wrapper>(toml_text).unwrap().inject
    }

    #[test]
    fn terminal_paste_defaults_when_section_is_absent() {
        let cfg = inject_from("[inject]\nmethod = \"paste\"\n");
        assert_eq!(cfg.terminal, TerminalPasteConfig::default());
        assert!(cfg.terminal.split_paste);
        assert_eq!(cfg.terminal.max_paste_chars, 800);
        assert_eq!(cfg.terminal.max_paste_newlines, 2);
        assert_eq!(cfg.terminal.settle_ms, 50);
        assert_eq!(cfg.terminal.max_pieces, 40);
        assert!(cfg.terminal.extra_apps.is_empty());
        assert!(cfg.terminal.exclude_apps.is_empty());
        assert_eq!(inject_from("").terminal, TerminalPasteConfig::default());
        assert_eq!(
            InjectConfig::default().terminal,
            TerminalPasteConfig::default()
        );
    }

    #[test]
    fn terminal_paste_custom_values_parse() {
        let cfg = inject_from(
            r#"
            [inject.terminal]
            split_paste = false
            max_paste_chars = 150
            max_paste_newlines = 1
            settle_ms = 100
            max_pieces = 10
            extra_apps = ["com.microsoft.VSCode"]
            exclude_apps = ["Warp"]
            "#,
        );
        assert!(!cfg.terminal.split_paste);
        assert_eq!(cfg.terminal.max_paste_chars, 150);
        assert_eq!(cfg.terminal.max_paste_newlines, 1);
        assert_eq!(cfg.terminal.settle_ms, 100);
        assert_eq!(cfg.terminal.max_pieces, 10);
        assert_eq!(cfg.terminal.extra_apps, vec!["com.microsoft.VSCode"]);
        assert_eq!(cfg.terminal.exclude_apps, vec!["Warp"]);
        // The rest of [inject] keeps its defaults.
        assert!(cfg.restore_clipboard);
        assert_eq!(cfg.restore_delay_ms, 300);
    }

    #[test]
    fn terminal_paste_partial_section_keeps_other_defaults() {
        let cfg = inject_from("[inject.terminal]\nsettle_ms = 75\n");
        assert_eq!(cfg.terminal.settle_ms, 75);
        assert_eq!(cfg.terminal.max_paste_chars, 800);
        assert!(cfg.terminal.split_paste);
    }

    #[test]
    fn terminal_paste_budget_is_clamped_to_at_least_one() {
        let cfg = inject_from("[inject.terminal]\nmax_paste_chars = 0\nmax_paste_newlines = 0\n");
        let budget = cfg.terminal.budget();
        assert_eq!(budget.max_units, 1);
        assert_eq!(budget.max_breaks, 1);

        let default = TerminalPasteConfig::default().budget();
        assert_eq!(default.max_units, 800);
        assert_eq!(default.max_breaks, 2);
    }
}
