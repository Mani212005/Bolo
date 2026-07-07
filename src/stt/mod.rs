pub mod groq;

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
