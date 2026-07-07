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
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub inject: InjectConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    pub notifications: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self { notifications: true }
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
        Self { method: InjectMethod::Portal, type_delay_ms: 2 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InjectMethod {
    Portal,
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
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
        Ok(toml::from_str(&text)?)
    }
}
