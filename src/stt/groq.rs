use super::{SttProvider, Transcript};
use crate::config::{GroqConfig, PIPELINE_SAMPLE_RATE};
use anyhow::{anyhow, Context};
use std::io::Cursor;
use std::time::Instant;

const ENDPOINT: &str = "https://api.groq.com/openai/v1/audio/transcriptions";
pub const CAPTURE_DUMP_PATH: &str = "/tmp/bolo_capture.wav";

pub struct GroqStt {
    client: reqwest::Client,
    api_key: String,
    config: GroqConfig,
}

impl GroqStt {
    pub fn new(config: GroqConfig) -> anyhow::Result<Self> {
        let api_key = std::env::var("GROQ_API_KEY")
            .map_err(|_| anyhow!("GROQ_API_KEY is not set (export it; never put it in config)"))?;
        Ok(Self { client: reqwest::Client::new(), api_key, config })
    }

    fn form(&self, wav_bytes: Vec<u8>) -> anyhow::Result<reqwest::multipart::Form> {
        let mut form = reqwest::multipart::Form::new()
            .part(
                "file",
                reqwest::multipart::Part::bytes(wav_bytes)
                    .file_name("audio.wav")
                    .mime_str("audio/wav")?,
            )
            .text("model", self.config.model.clone())
            .text("response_format", "json")
            .text("language", self.config.language.clone())
            .text("temperature", self.config.temperature.to_string());
        // Bias the model toward the user's vocabulary (names, jargon).
        if let Some(vocab) = crate::userdata::vocabulary_prompt() {
            eprintln!("[groq]    vocabulary prompt: {vocab}");
            form = form.text("prompt", vocab);
        }
        Ok(form)
    }

    async fn request_once(&self, wav_bytes: Vec<u8>) -> anyhow::Result<Transcript> {
        let t0 = Instant::now();
        let response = self
            .client
            .post(ENDPOINT)
            .bearer_auth(&self.api_key)
            .multipart(self.form(wav_bytes)?)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        let latency_ms = t0.elapsed().as_millis();

        if !status.is_success() {
            let hint = match status.as_u16() {
                401 => "bad or missing API key (check GROQ_API_KEY)",
                413 => "file too big (Groq caps uploads at 25 MB)",
                429 => "rate limited (free tier ~2,000 audio requests/day)",
                _ => "unexpected Groq error",
            };
            return Err(anyhow!("groq {status}: {hint}\nraw body: {body}"));
        }

        let json: serde_json::Value =
            serde_json::from_str(&body).context("groq returned non-JSON body")?;
        let text = json
            .get("text")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow!("no `text` field in groq response: {body}"))?
            .trim()
            .to_string();
        eprintln!(
            "[groq]    status={} model={} latency_ms={}",
            status.as_u16(),
            self.config.model,
            latency_ms
        );
        Ok(Transcript { text, raw_json: body, latency_ms })
    }
}

#[async_trait::async_trait]
impl SttProvider for GroqStt {
    async fn transcribe(&self, wav_bytes: Vec<u8>) -> anyhow::Result<Transcript> {
        match self.request_once(wav_bytes.clone()).await {
            Ok(t) => Ok(t),
            // Retry once only on transport-level failures, not API errors.
            Err(e) if e.downcast_ref::<reqwest::Error>().is_some() => {
                eprintln!("[groq]    network error ({e}), retrying once…");
                self.request_once(wav_bytes).await
            }
            Err(e) => Err(e),
        }
    }
}

/// Encode 16kHz mono i16 PCM into an in-memory WAV, and dump the exact bytes
/// to /tmp/bolo_capture.wav so the payload can be independently curl'd.
pub fn encode_wav(samples: &[i16]) -> anyhow::Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: PIPELINE_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec)?;
        for &s in samples {
            writer.write_sample(s)?;
        }
        writer.finalize()?;
    }
    let bytes = cursor.into_inner();
    std::fs::write(CAPTURE_DUMP_PATH, &bytes)
        .with_context(|| format!("failed to write {CAPTURE_DUMP_PATH}"))?;
    Ok(bytes)
}
