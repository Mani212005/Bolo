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
        Self { provider: SttBackend::Groq, whisper: WhisperConfig::default() }
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
        Self { model: "large-v3-turbo".to_string() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EnhanceConfig {
    pub model: String,
}

impl Default for EnhanceConfig {
    fn default() -> Self {
        Self { model: "llama-3.3-70b-versatile".to_string() }
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
        Self { notifications: true, sounds: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct InjectConfig {
    pub method: InjectMethod,
    pub type_delay_ms: u64,
}

impl Default for InjectConfig {
    fn default() -> Self {
        Self { method: InjectMethod::Paste, type_delay_ms: 2 }
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
