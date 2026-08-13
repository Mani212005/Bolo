//! Settings & History app backend: tiny HTTP server inside the daemon, bound to
//! 127.0.0.1 only (single-user, localhost — no auth by design). Serves the
//! SuperWhisper-style UI (src/ui/app.html) and a rich JSON/Audio API over the same
//! state the daemon uses.

use crate::config::Config;
use crate::config_edit::{ConfigDoc, MODELS};
use crate::daemon::{Phase, PipelineMsg, Shared};
use crate::stt::SttProvider;
use crate::vad::Control;
use anyhow::Context;
use crossbeam_channel::Sender;
use serde_json::{json, Value};
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const APP_HTML: &str = include_str!("ui/app.html");
const HOTKEY_ACTIONS: [(&str, &str); 3] =
    [("toggle", "Bolo toggle"), ("pause", "Bolo pause"), ("insert", "Bolo insert")];

pub enum WebResponse {
    Html(String),
    Json(Value),
    Text(String),
    Audio(Vec<u8>),
    NotFound,
}

pub fn serve(
    config_path: PathBuf,
    shared: Arc<Mutex<Shared>>,
    cfg: Config,
    start_tx: Sender<()>,
    pipeline_tx: Sender<PipelineMsg>,
    stt: Arc<dyn SttProvider>,
) {
    let mut port = cfg.ui.port;
    let mut tries = 0;
    let server = loop {
        match tiny_http::Server::http(("127.0.0.1", port)) {
            Ok(s) => break s,
            Err(e) if tries < 15 => {
                std::thread::sleep(std::time::Duration::from_millis(100));
                tries += 1;
            }
            Err(e) if port < cfg.ui.port + 5 => {
                eprintln!("[web] port {port} unavailable ({e}); trying {}", port + 1);
                port += 1;
                tries = 0;
            }
            Err(e) => {
                eprintln!("[web] settings app disabled: {e}");
                return;
            }
        }
    };
    let _ = std::fs::create_dir_all(crate::userdata::data_dir());
    let _ = std::fs::write(crate::userdata::data_dir().join("port.txt"), port.to_string());
    eprintln!("[web] settings & history dashboard on http://127.0.0.1:{port}");
    for mut request in server.incoming_requests() {
        let config_path = config_path.clone();
        let shared = Arc::clone(&shared);
        let cfg = cfg.clone();
        let start_tx = start_tx.clone();
        let pipeline_tx = pipeline_tx.clone();
        let stt = Arc::clone(&stt);
        // Thread per request: mic-test and enhance block for seconds.
        std::thread::spawn(move || {
            let method = request.method().as_str().to_string();
            let url = request.url().to_string();
            let mut body_bytes = Vec::new();
            if let Some(len) = request.body_length() {
                body_bytes.resize(len, 0);
                let _ = request.as_reader().read_exact(&mut body_bytes);
            } else {
                let _ = request.as_reader().read_to_end(&mut body_bytes);
            }
            let body_str = String::from_utf8_lossy(&body_bytes).to_string();

            let result = route(
                &method,
                &url,
                &body_str,
                &body_bytes,
                &config_path,
                &shared,
                &cfg,
                &start_tx,
                &pipeline_tx,
                &stt,
            );

            let response = match result {
                Ok(WebResponse::Html(html)) => tiny_http::Response::from_string(html)
                    .with_header(
                        tiny_http::Header::from_bytes("Content-Type", "text/html; charset=utf-8")
                            .unwrap(),
                    ),
                Ok(WebResponse::Json(v)) => tiny_http::Response::from_string(v.to_string())
                    .with_header(
                        tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap(),
                    )
                    .with_header(
                        tiny_http::Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap(),
                    ),
                Ok(WebResponse::Text(t)) => tiny_http::Response::from_string(t)
                    .with_header(
                        tiny_http::Header::from_bytes(
                            "Content-Type",
                            "text/plain; charset=utf-8",
                        )
                        .unwrap(),
                    ),
                Ok(WebResponse::Audio(bytes)) => {
                    let len = bytes.len();
                    tiny_http::Response::from_data(bytes)
                        .with_header(
                            tiny_http::Header::from_bytes("Content-Type", "audio/wav").unwrap(),
                        )
                        .with_header(
                            tiny_http::Header::from_bytes("Content-Length", len.to_string()).unwrap(),
                        )
                        .with_header(
                            tiny_http::Header::from_bytes("Accept-Ranges", "bytes").unwrap(),
                        )
                        .with_header(
                            tiny_http::Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap(),
                        )
                        .with_header(
                            tiny_http::Header::from_bytes(
                                "Cache-Control",
                                "public, max-age=86400",
                            )
                            .unwrap(),
                        )
                }
                Ok(WebResponse::NotFound) => {
                    tiny_http::Response::from_string("not found").with_status_code(404)
                }
                Err(e) => {
                    eprintln!("[web] {method} {url} failed: {e:#}");
                    tiny_http::Response::from_string(format!("{e:#}")).with_status_code(500)
                }
            };
            let _ = request.respond(response);
        });
    }
}

fn route(
    method: &str,
    url: &str,
    body: &str,
    body_bytes: &[u8],
    config_path: &PathBuf,
    shared: &Arc<Mutex<Shared>>,
    cfg: &Config,
    start_tx: &Sender<()>,
    pipeline_tx: &Sender<PipelineMsg>,
    stt: &Arc<dyn SttProvider>,
) -> anyhow::Result<WebResponse> {
    if method == "GET" && (url == "/" || url == "/index.html") {
        return Ok(WebResponse::Html(APP_HTML.to_string()));
    }
    if method == "GET" && url == "/api/state" {
        return Ok(WebResponse::Json(state(config_path, shared)?));
    }
    if method == "GET" && url.starts_with("/api/audio") {
        let id = if let Some(query) = url.split_once('?') {
            query.1.split('&').find_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                if k == "id" || k == "file" {
                    Some(v.to_string())
                } else {
                    None
                }
            })
        } else {
            url.strip_prefix("/api/audio/")
                .filter(|s| !s.is_empty())
                .map(String::from)
        };
        if let Some(id) = id {
            if let Some(bytes) = crate::userdata::read_recording_wav(&id) {
                return Ok(WebResponse::Audio(bytes));
            }
        }
        return Ok(WebResponse::NotFound);
    }
    if method == "DELETE" && url.starts_with("/api/history") {
        let id = if let Some(query) = url.split_once('?') {
            query.1.split('&').find_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                if k == "id" {
                    Some(v.to_string())
                } else {
                    None
                }
            })
        } else {
            url.strip_prefix("/api/history/")
                .filter(|s| !s.is_empty())
                .map(String::from)
        };
        if let Some(id) = id {
            let deleted = crate::userdata::delete_history_item(&id);
            return Ok(WebResponse::Json(json!({ "ok": true, "deleted": deleted })));
        } else {
            crate::userdata::clear_history();
            return Ok(WebResponse::Json(json!({ "ok": true, "cleared": true })));
        }
    }
    if method == "POST" && url == "/api/toggle" {
        let mut s = shared.lock().unwrap();
        let status = match s.phase {
            Phase::Idle => {
                s.phase = Phase::Recording;
                s.toggle_t0 = Some(Instant::now());
                drop(s);
                let _ = start_tx.send(());
                "recording"
            }
            Phase::Recording => {
                if let Some(tx) = s.control_tx.as_ref() {
                    let _ = tx.send(Control::ForceStop);
                }
                "stopping"
            }
            Phase::Paused => {
                s.phase = Phase::Processing;
                drop(s);
                let _ = pipeline_tx.send(PipelineMsg::Finalize);
                "processing"
            }
            Phase::Processing => "processing",
        };
        return Ok(WebResponse::Json(json!({ "ok": true, "phase": status })));
    }
    if method == "POST" && url == "/api/upload-transcribe" {
        anyhow::ensure!(!body_bytes.is_empty(), "empty audio file");
        let runtime = tokio::runtime::Runtime::new()?;
        let stt = Arc::clone(stt);
        let wav_vec = body_bytes.to_vec();
        let transcript = runtime.block_on(async move { stt.transcribe(wav_vec).await })?;
        let text = transcript.text.trim().to_string();
        if !text.is_empty() {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let audio_id = format!("upload_{now_ms}");
            let _ = crate::userdata::save_recording_wav(&audio_id, body_bytes);
            crate::userdata::append_history("uploaded", &text, Some(&audio_id), None);
        }
        return Ok(WebResponse::Json(json!({ "ok": true, "text": text })));
    }
    if method == "POST" && url == "/api/config" {
        let changes: Value = serde_json::from_str(body).context("bad JSON body")?;
        let mut doc = ConfigDoc::load(config_path)?;
        if let Some(v) = changes["provider"].as_str() {
            doc.set(&["stt", "provider"], v.into());
        }
        if let Some(v) = changes["model"].as_str() {
            doc.set(&["stt", "whisper", "model"], v.into());
        }
        if let Some(v) = changes["sounds"].as_bool() {
            doc.set(&["daemon", "sounds"], v.into());
        }
        if let Some(v) = changes["notifications"].as_bool() {
            doc.set(&["daemon", "notifications"], v.into());
        }
        if let Some(v) = changes["auto_endpoint"].as_bool() {
            doc.set(&["vad", "auto_endpoint"], v.into());
        }
        if let Some(v) = changes["method"].as_str() {
            doc.set(&["inject", "method"], v.into());
        }
        if let Some(v) = changes["max_utterance_ms"].as_i64() {
            doc.set(&["vad", "max_utterance_ms"], v.into());
        }
        doc.save()?;
        eprintln!("[web] config saved");
        return Ok(WebResponse::Json(json!({ "ok": true, "needs_restart": true })));
    }
    if method == "POST" && url == "/api/restart" {
        // Reply first; the restart tears this process down.
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            let managed = std::process::Command::new("systemctl")
                .args(["--user", "restart", "bolo.service"])
                .status()
                .is_ok_and(|s| s.success());
            if !managed {
                // Fallback: exec a fresh daemon and exit this one.
                if let Ok(exe) = std::env::current_exe() {
                    let _ = std::process::Command::new(exe).arg("daemon").spawn();
                }
                std::process::exit(0);
            }
        });
        return Ok(WebResponse::Json(json!({ "ok": true })));
    }
    if method == "GET" && url == "/api/vocab" {
        return Ok(WebResponse::Text(vocab_terms().join("\n")));
    }
    if method == "PUT" && url == "/api/vocab" {
        crate::userdata::write_keeping_comments("vocabulary.txt", body)?;
        return Ok(WebResponse::Json(json!({ "ok": true })));
    }
    if method == "PUT" && url == "/api/enhance-prompt" {
        crate::userdata::write_keeping_comments("enhance_prompt.txt", body)?;
        return Ok(WebResponse::Json(json!({ "ok": true })));
    }
    if method == "GET" && url == "/api/scratchpad" {
        return Ok(WebResponse::Text(crate::userdata::read_scratchpad()));
    }
    if method == "PUT" && url == "/api/scratchpad" {
        crate::userdata::write_scratchpad(body)?;
        return Ok(WebResponse::Json(json!({ "ok": true })));
    }
    if method == "POST" && url == "/api/enhance" {
        anyhow::ensure!(!body.trim().is_empty(), "nothing to enhance");
        let runtime = tokio::runtime::Runtime::new()?;
        let enhanced = runtime.block_on(crate::enhance::enhance(&cfg.enhance, body))?;
        crate::userdata::append_history("enhanced", &enhanced, None, None);
        shared.lock().unwrap().last_text = Some(enhanced.clone());
        return Ok(WebResponse::Json(json!({ "text": enhanced })));
    }
    if method == "POST" && url == "/api/hotkeys" {
        let keys: Value = serde_json::from_str(body).context("bad JSON body")?;
        let get = |k: &str| -> anyhow::Result<String> {
            let v = keys[k].as_str().context("missing hotkey")?.trim();
            anyhow::ensure!(
                !v.is_empty() && v.chars().all(|c| c.is_ascii_graphic()),
                "invalid binding {v:?}"
            );
            Ok(v.to_string())
        };
        let script = script_path("install-hotkey.sh")?;
        let status = std::process::Command::new("bash")
            .arg(script)
            .args([get("toggle")?, get("pause")?, get("insert")?])
            .status()?;
        anyhow::ensure!(status.success(), "install-hotkey.sh failed");
        return Ok(WebResponse::Json(json!({ "ok": true })));
    }
    if method == "POST" && url == "/api/mic-test" {
        let fresh = Config::load(config_path)?;
        let phase = shared.lock().unwrap().phase;
        anyhow::ensure!(
            phase == Phase::Idle,
            "daemon is {} — finish the dictation first",
            phase.as_str()
        );
        let text = crate::mictest::run(&fresh, 3)?;
        return Ok(WebResponse::Json(json!({ "text": text })));
    }
    Ok(WebResponse::NotFound)
}

fn vocab_terms() -> Vec<String> {
    let text =
        std::fs::read_to_string(crate::userdata::config_dir().join("vocabulary.txt"))
            .unwrap_or_default();
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

/// scripts/ lives next to the repo the binary was built in (target/release/..).
fn script_path(name: &str) -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let path = exe
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .map(|repo| repo.join("scripts").join(name))
        .filter(|p| p.exists())
        .with_context(|| format!("scripts/{name} not found near {}", exe.display()))?;
    Ok(path)
}

fn read_hotkeys() -> Value {
    const SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys";
    const BASE: &str = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings";
    let mut out = serde_json::Map::new();
    for i in 0..10 {
        let slot = format!("{BASE}/custom{i}/");
        let get = |key: &str| -> Option<String> {
            let o = std::process::Command::new("gsettings")
                .args(["get", &format!("{SCHEMA}.custom-keybinding:{slot}"), key])
                .output()
                .ok()?;
            let s = String::from_utf8_lossy(&o.stdout).trim().trim_matches('\'').to_string();
            (!s.is_empty()).then_some(s)
        };
        if let Some(name) = get("name") {
            for (action, slot_name) in HOTKEY_ACTIONS {
                if name == slot_name {
                    if let Some(binding) = get("binding") {
                        out.insert(action.to_string(), Value::String(binding));
                    }
                }
            }
        }
    }
    Value::Object(out)
}

fn state(config_path: &PathBuf, shared: &Arc<Mutex<Shared>>) -> anyhow::Result<Value> {
    let doc = ConfigDoc::load(config_path)?;
    let status = shared.lock().unwrap().phase.as_str().to_string();
    let enhance_prompt = crate::userdata::enhance_prompt().unwrap_or_default();
    Ok(json!({
        "status": status,
        "provider": doc.str_at(&["stt", "provider"], "groq"),
        "model": doc.str_at(&["stt", "whisper", "model"], "small.en"),
        "sounds": doc.bool_at(&["daemon", "sounds"], true),
        "notifications": doc.bool_at(&["daemon", "notifications"], true),
        "auto_endpoint": doc.bool_at(&["vad", "auto_endpoint"], false),
        "method": doc.str_at(&["inject", "method"], "paste"),
        "max_len": doc.int_at(&["vad", "max_utterance_ms"], 1_800_000),
        "hotkeys": read_hotkeys(),
        "vocab": vocab_terms(),
        "enhance_prompt": enhance_prompt,
        "scratchpad": crate::userdata::read_scratchpad(),
        "history": crate::userdata::read_history(100),
        "models": MODELS.iter().map(|(m, s)| json!({ "name": m, "speed": s })).collect::<Vec<_>>(),
    }))
}
