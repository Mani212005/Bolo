use crate::config::WhisperConfig;
use crate::stt::{SttProvider, Transcript};
use anyhow::Context;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// Local whisper.cpp backend. Audio never leaves the machine.
pub struct WhisperStt {
    ctx: Arc<WhisperContext>,
    model: String,
    language: String,
    threads: i32,
}

pub fn models_dir() -> PathBuf {
    // Deliberately HOME-based, not XDG_DATA_HOME: snap-spawned shells (e.g.
    // VS Code's terminal) point XDG_DATA_HOME into ~/snap/<app>/, which would
    // scatter 1.6GB models across per-app dirs.
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join(".local/share/bolo/models")
}

pub fn model_path(name: &str) -> PathBuf {
    models_dir().join(format!("ggml-{name}.bin"))
}

/// Download the ggml model from ggerganov/whisper.cpp on Hugging Face if it
/// isn't on disk yet. Blocking; brings up its own runtime so it can be called
/// from sync startup paths.
pub fn ensure_model_blocking(name: &str) -> anyhow::Result<PathBuf> {
    let path = model_path(name);
    if path.exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(models_dir())?;
    // Stock + quantized (-q5_0/-q8_0/-q5_1) models live in ggerganov's repo;
    // the distilled ones are published by the distil-whisper org under
    // different filenames. Locally everything is saved as ggml-<name>.bin.
    let url = match name {
        "distil-small.en" => {
            "https://huggingface.co/distil-whisper/distil-small.en/resolve/main/ggml-distil-small.en.bin"
                .to_string()
        }
        "distil-medium.en" => {
            "https://huggingface.co/distil-whisper/distil-medium.en/resolve/main/ggml-medium-32-2.en.bin"
                .to_string()
        }
        "distil-large-v3.5" => {
            "https://huggingface.co/distil-whisper/distil-large-v3.5-ggml/resolve/main/ggml-model.bin"
                .to_string()
        }
        _ => format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{name}.bin"),
    };
    eprintln!("[model] downloading {url} -> {}", path.display());
    let tmp = path.with_extension("bin.partial");
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let resp = reqwest::get(&url).await?.error_for_status().with_context(|| {
            format!("model \"{name}\" not found upstream (check [stt.whisper] model name)")
        })?;
        let total = resp.content_length().unwrap_or(0);
        let mut file = std::fs::File::create(&tmp)?;
        let mut resp = resp;
        let mut done: u64 = 0;
        let mut last_pct: u64 = 0;
        while let Some(chunk) = resp.chunk().await? {
            file.write_all(&chunk)?;
            done += chunk.len() as u64;
            if total > 0 {
                let pct = done * 100 / total;
                if pct >= last_pct + 5 {
                    last_pct = pct;
                    eprintln!("[model] {pct}% ({done}/{total} bytes)");
                }
            }
        }
        file.flush()?;
        anyhow::Ok(())
    })?;
    std::fs::rename(&tmp, &path)?;
    eprintln!("[model] saved {}", path.display());
    Ok(path)
}

impl WhisperStt {
    pub fn new(cfg: &WhisperConfig, language: &str) -> anyhow::Result<Self> {
        let path = ensure_model_blocking(&cfg.model)?;
        let t0 = Instant::now();
        let mut ctx_params = WhisperContextParameters::default();
        ctx_params.flash_attn(true);
        let ctx = WhisperContext::new_with_params(
            path.to_str().context("non-utf8 model path")?,
            ctx_params,
        )
        .with_context(|| format!("failed to load whisper model {}", path.display()))?;
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) as i32;
        eprintln!(
            "[whisper] model={} loaded in {}ms threads={}",
            cfg.model,
            t0.elapsed().as_millis(),
            threads
        );
        Ok(Self {
            ctx: Arc::new(ctx),
            model: cfg.model.clone(),
            language: language.to_string(),
            threads,
        })
    }
}

#[async_trait::async_trait]
impl SttProvider for WhisperStt {
    async fn transcribe(&self, wav_bytes: Vec<u8>) -> anyhow::Result<Transcript> {
        let ctx = Arc::clone(&self.ctx);
        let model = self.model.clone();
        let language = self.language.clone();
        let threads = self.threads;
        // whisper.cpp inference is CPU-bound and synchronous.
        tokio::task::spawn_blocking(move || {
            let reader = hound::WavReader::new(std::io::Cursor::new(wav_bytes))?;
            let samples: Vec<f32> = reader
                .into_samples::<i16>()
                .map(|s| s.map(|v| v as f32 / 32768.0))
                .collect::<Result<_, _>>()?;
            let audio_s = samples.len() as f64 / crate::config::PIPELINE_SAMPLE_RATE as f64;

            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            params.set_n_threads(threads);
            params.set_language(Some(&language));
            // Bias toward the user's vocabulary (names, jargon).
            let vocab = crate::userdata::vocabulary_prompt();
            if let Some(v) = vocab.as_deref() {
                eprintln!("[whisper] vocabulary prompt: {v}");
                params.set_initial_prompt(v);
            }
            params.set_print_special(false);
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_timestamps(false);

            let t0 = Instant::now();
            let mut state = ctx.create_state().context("whisper create_state failed")?;
            state.full(params, &samples).context("whisper inference failed")?;
            let latency_ms = t0.elapsed().as_millis();

            let n = state.full_n_segments();
            let mut text = String::new();
            let mut segments = Vec::new();
            for i in 0..n {
                let seg = state.get_segment(i).context("missing segment")?;
                let seg_text = seg.to_str().context("segment text not utf8")?.to_string();
                segments.push(serde_json::json!({
                    "t0": seg.start_timestamp(),
                    "t1": seg.end_timestamp(),
                    "text": seg_text,
                }));
                text.push_str(&seg_text);
            }
            let text = text.trim().to_string();
            let raw_json =
                serde_json::json!({ "model": model, "segments": segments, "text": text })
                    .to_string();
            eprintln!(
                "[whisper] model={} latency_ms={} audio_s={:.2} rtf={:.2}",
                model,
                latency_ms,
                audio_s,
                latency_ms as f64 / 1000.0 / audio_s.max(0.001)
            );
            Ok(Transcript { text, raw_json, latency_ms })
        })
        .await
        .context("whisper task panicked")?
    }
}
