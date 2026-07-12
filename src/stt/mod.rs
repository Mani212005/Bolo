pub mod fasterwhisper;
pub mod groq;
pub mod whisper;

use crate::config::{Config, SttBackend};
use std::sync::Arc;

#[async_trait::async_trait]
pub trait SttProvider: Send + Sync {
    /// Takes 16kHz mono i16 PCM as a WAV byte blob, returns transcript text.
    async fn transcribe(&self, wav_bytes: Vec<u8>) -> anyhow::Result<Transcript>;
}

pub struct Transcript {
    pub text: String,
    pub raw_json: String,
    pub latency_ms: u128,
}

/// Build the configured STT backend. Groq needs GROQ_API_KEY; the local
/// whisper backend needs no key and no network (after the one-time model
/// download).
pub fn make_provider(cfg: &Config) -> anyhow::Result<Arc<dyn SttProvider>> {
    crate::userdata::ensure_starter_files();
    match cfg.stt.provider {
        SttBackend::Groq => Ok(Arc::new(groq::GroqStt::new(cfg.groq.clone())?)),
        SttBackend::Whisper => Ok(Arc::new(whisper::WhisperStt::new(
            &cfg.stt.whisper,
            &cfg.groq.language,
        )?)),
        // Shares the [stt.whisper] model knob: one "local model" setting.
        SttBackend::FasterWhisper => {
            Ok(Arc::new(fasterwhisper::FasterWhisperStt::new(&cfg.stt.whisper.model)?))
        }
    }
}
