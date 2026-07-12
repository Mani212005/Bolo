use crate::stt::{SttProvider, Transcript};
use anyhow::{anyhow, Context};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Local CTranslate2/INT8 backend via a persistent `faster-whisper` Python
/// sidecar (see fw_server.py). Audio never leaves the machine; the sidecar
/// loads the model once and answers JSON-line requests.
pub struct FasterWhisperStt {
    inner: Arc<Inner>,
}

struct Inner {
    sidecar: Mutex<Option<Sidecar>>,
    model: String,
    threads: usize,
}

struct Sidecar {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

fn data_dir() -> PathBuf {
    // HOME-based on purpose; see whisper::models_dir.
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share/bolo")
}

/// One-time venv setup: python3 -m venv + pip install faster-whisper.
fn ensure_venv() -> anyhow::Result<PathBuf> {
    let venv = data_dir().join("fw-venv");
    let python = venv.join("bin/python");
    if !python.exists() {
        eprintln!("[fw] creating venv + installing faster-whisper (one-time, a few minutes)…");
        let status = Command::new("python3")
            .args(["-m", "venv"])
            .arg(&venv)
            .status()
            .context("python3 not found (needed for the faster-whisper backend)")?;
        anyhow::ensure!(status.success(), "python3 -m venv failed");
        let status = Command::new(venv.join("bin/pip"))
            .args(["install", "--quiet", "faster-whisper"])
            .status()?;
        anyhow::ensure!(status.success(), "pip install faster-whisper failed");
        eprintln!("[fw] venv ready");
    }
    Ok(python)
}

impl FasterWhisperStt {
    pub fn new(model: &str) -> anyhow::Result<Self> {
        ensure_venv()?; // fail fast at startup, before any recording
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let inner = Arc::new(Inner {
            sidecar: Mutex::new(None),
            model: model.to_string(),
            threads,
        });
        // Warm up now: the sidecar downloads (first time) and loads the
        // model at spawn, so the first dictation doesn't pay for it.
        *inner.sidecar.lock().unwrap() = Some(inner.spawn_sidecar()?);
        Ok(Self { inner })
    }
}

impl Inner {
    fn spawn_sidecar(&self) -> anyhow::Result<Sidecar> {
        let python = ensure_venv()?;
        let script = data_dir().join("fw_server.py");
        std::fs::write(&script, include_str!("fw_server.py"))?;
        eprintln!("[fw] starting sidecar model={} threads={}", self.model, self.threads);
        let mut child = Command::new(python)
            .arg(&script)
            .arg(&self.model)
            .arg(self.threads.to_string())
            // Keep faster-whisper's own model downloads out of snap-polluted
            // default cache locations.
            .env("HF_HOME", data_dir().join("hf-cache"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("failed to start faster-whisper sidecar")?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Ok(Sidecar { child, stdin, stdout })
    }

    fn request(&self, wav_path: &str) -> anyhow::Result<(String, u128)> {
        let mut guard = self.sidecar.lock().unwrap();
        if guard.is_none() {
            *guard = Some(self.spawn_sidecar()?);
        }
        let sc = guard.as_mut().expect("sidecar just ensured");
        let req = serde_json::json!({
            "wav": wav_path,
            "initial_prompt": crate::userdata::vocabulary_prompt(),
        });
        let mut reply = String::new();
        let io_result = writeln!(sc.stdin, "{req}")
            .map_err(anyhow::Error::from)
            .and_then(|()| Ok(sc.stdout.read_line(&mut reply)?));
        match io_result {
            Ok(n) if n > 0 => {}
            _ => {
                // Sidecar died — drop it so the next call respawns.
                let _ = sc.child.kill();
                let _ = sc.child.wait();
                *guard = None;
                return Err(anyhow!("faster-whisper sidecar died; will restart on next use"));
            }
        }
        let json: serde_json::Value =
            serde_json::from_str(&reply).context("sidecar returned non-JSON")?;
        if let Some(err) = json.get("error").and_then(|e| e.as_str()) {
            return Err(anyhow!("faster-whisper: {err}"));
        }
        let text = json["text"].as_str().unwrap_or_default().trim().to_string();
        let latency = json["latency_ms"].as_u64().unwrap_or(0) as u128;
        Ok((text, latency))
    }
}

#[async_trait::async_trait]
impl SttProvider for FasterWhisperStt {
    async fn transcribe(&self, wav_bytes: Vec<u8>) -> anyhow::Result<Transcript> {
        let audio_s = (wav_bytes.len().saturating_sub(44)) as f64
            / (2.0 * crate::config::PIPELINE_SAMPLE_RATE as f64);
        let wav_path = format!("/tmp/bolo_fw_{}.wav", std::process::id());
        std::fs::write(&wav_path, &wav_bytes)?;
        let inner = Arc::clone(&self.inner);
        let t0 = Instant::now();
        // Blocking pipe I/O behind the Mutex — off the async runtime.
        let (text, sidecar_ms) =
            tokio::task::spawn_blocking(move || inner.request(&wav_path))
                .await
                .context("faster-whisper task panicked")??;
        let latency_ms = t0.elapsed().as_millis();
        let raw_json = serde_json::json!({
            "model": self.inner.model, "engine": "faster-whisper", "text": text,
            "sidecar_ms": sidecar_ms as u64,
        })
        .to_string();
        eprintln!(
            "[fw]      model={} latency_ms={} (sidecar_ms={}) audio_s={:.2} rtf={:.2}",
            self.inner.model,
            latency_ms,
            sidecar_ms,
            audio_s,
            latency_ms as f64 / 1000.0 / audio_s.max(0.001)
        );
        Ok(Transcript { text, raw_json, latency_ms })
    }
}
