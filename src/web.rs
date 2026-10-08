//! Settings & History app backend: tiny HTTP server inside the daemon, bound to
//! 127.0.0.1 only (single-user, localhost — no auth by design). Serves the
//! SuperWhisper-style UI (src/ui/app.html) and a rich JSON/Audio API over the same
//! state the daemon uses.

use crate::config::{Config, PillConfig, PillStyle};
use crate::config_edit::{ConfigDoc, MODELS};
use crate::daemon::{Phase, PipelineMsg, Shared, Toggle};
use crate::stt::SttProvider;
use anyhow::Context;
use crossbeam_channel::Sender;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const APP_HTML: &str = include_str!("ui/app.html");
const HOTKEY_ACTIONS: [(&str, &str); 3] = [
    ("toggle", "Bolo toggle"),
    ("pause", "Bolo pause"),
    ("insert", "Bolo insert"),
];

pub enum WebResponse {
    Html(String),
    Json(Value),
    Text(String),
    Audio(Vec<u8>),
    Image(Vec<u8>),
    NotFound,
}

fn percent_decode(input: &str) -> String {
    let mut bytes = Vec::new();
    let mut chars = input.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let h1 = chars.next();
            let h2 = chars.next();
            if let (Some(h1), Some(h2)) = (h1, h2) {
                if let Ok(num) = u8::from_str_radix(&format!("{}{}", h1 as char, h2 as char), 16) {
                    bytes.push(num);
                    continue;
                }
            }
        }
        bytes.push(b);
    }
    String::from_utf8_lossy(&bytes).to_string()
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
            Err(_e) if tries < 15 => {
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
    let _ = std::fs::write(
        crate::userdata::data_dir().join("port.txt"),
        port.to_string(),
    );
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
                    )
                    .with_header(
                        tiny_http::Header::from_bytes(
                            "Cache-Control",
                            "no-store, no-cache, must-revalidate",
                        )
                        .unwrap(),
                    )
                    .with_header(tiny_http::Header::from_bytes("Pragma", "no-cache").unwrap()),
                Ok(WebResponse::Json(v)) => tiny_http::Response::from_string(v.to_string())
                    .with_header(
                        tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap(),
                    )
                    .with_header(
                        tiny_http::Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap(),
                    )
                    .with_header(
                        tiny_http::Header::from_bytes(
                            "Cache-Control",
                            "no-store, no-cache, must-revalidate",
                        )
                        .unwrap(),
                    )
                    .with_header(tiny_http::Header::from_bytes("Pragma", "no-cache").unwrap()),
                Ok(WebResponse::Text(t)) => tiny_http::Response::from_string(t)
                    .with_header(
                        tiny_http::Header::from_bytes("Content-Type", "text/plain; charset=utf-8")
                            .unwrap(),
                    )
                    .with_header(
                        tiny_http::Header::from_bytes(
                            "Cache-Control",
                            "no-store, no-cache, must-revalidate",
                        )
                        .unwrap(),
                    )
                    .with_header(tiny_http::Header::from_bytes("Pragma", "no-cache").unwrap()),
                Ok(WebResponse::Audio(bytes)) => {
                    let len = bytes.len();
                    tiny_http::Response::from_data(bytes)
                        .with_header(
                            tiny_http::Header::from_bytes("Content-Type", "audio/wav").unwrap(),
                        )
                        .with_header(
                            tiny_http::Header::from_bytes("Content-Length", len.to_string())
                                .unwrap(),
                        )
                        .with_header(
                            tiny_http::Header::from_bytes("Accept-Ranges", "bytes").unwrap(),
                        )
                        .with_header(
                            tiny_http::Header::from_bytes("Access-Control-Allow-Origin", "*")
                                .unwrap(),
                        )
                        .with_header(
                            tiny_http::Header::from_bytes("Cache-Control", "public, max-age=86400")
                                .unwrap(),
                        )
                }
                Ok(WebResponse::Image(bytes)) => {
                    let len = bytes.len();
                    tiny_http::Response::from_data(bytes)
                        .with_header(
                            tiny_http::Header::from_bytes("Content-Type", "image/png").unwrap(),
                        )
                        .with_header(
                            tiny_http::Header::from_bytes("Content-Length", len.to_string())
                                .unwrap(),
                        )
                        .with_header(
                            tiny_http::Header::from_bytes("Access-Control-Allow-Origin", "*")
                                .unwrap(),
                        )
                        .with_header(
                            tiny_http::Header::from_bytes("Cache-Control", "public, max-age=86400")
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn route(
    method: &str,
    url: &str,
    body: &str,
    body_bytes: &[u8],
    config_path: &Path,
    shared: &Arc<Mutex<Shared>>,
    cfg: &Config,
    start_tx: &Sender<()>,
    pipeline_tx: &Sender<PipelineMsg>,
    stt: &Arc<dyn SttProvider>,
) -> anyhow::Result<WebResponse> {
    let (clean_path, query_str) = url.split_once('?').unwrap_or((url, ""));
    if (method == "GET" || method == "HEAD") && (clean_path == "/" || clean_path == "/index.html") {
        return Ok(WebResponse::Html(APP_HTML.to_string()));
    }
    if method == "GET" && clean_path == "/api/state" {
        return Ok(WebResponse::Json(state(config_path, shared)?));
    }
    if (method == "GET" || method == "HEAD") && clean_path.starts_with("/api/audio") {
        let id = if !query_str.is_empty() {
            query_str.split('&').find_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                if k == "id" || k == "file" {
                    Some(v.to_string())
                } else {
                    None
                }
            })
        } else {
            clean_path
                .strip_prefix("/api/audio/")
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
    if (method == "GET" || method == "HEAD") && clean_path.starts_with("/api/image") {
        let img_path = if !query_str.is_empty() {
            query_str.split('&').find_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                if k == "path" || k == "file" {
                    Some(v.to_string())
                } else {
                    None
                }
            })
        } else {
            clean_path
                .strip_prefix("/api/image/")
                .filter(|s| !s.is_empty())
                .map(String::from)
        };
        if let Some(raw_path) = img_path {
            let decoded = percent_decode(&raw_path);
            let path = PathBuf::from(decoded);
            let sessions_dir = crate::userdata::sessions_dir();
            let data_dir = crate::userdata::data_dir();
            let temp_dir = std::env::temp_dir();
            let is_allowed = path.starts_with(&sessions_dir)
                || path.starts_with(&data_dir)
                || path.starts_with("/tmp")
                || path.starts_with("/private/tmp")
                || path.starts_with(&temp_dir);
            if is_allowed && path.exists() && path.is_file() {
                if let Ok(bytes) = std::fs::read(&path) {
                    return Ok(WebResponse::Image(bytes));
                }
            }
        }
        return Ok(WebResponse::NotFound);
    }
    if method == "DELETE" && clean_path.starts_with("/api/history") {
        let id = if !query_str.is_empty() {
            query_str.split('&').find_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                if k == "id" {
                    Some(v.to_string())
                } else {
                    None
                }
            })
        } else {
            clean_path
                .strip_prefix("/api/history/")
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
    if method == "POST" && clean_path == "/api/toggle" {
        let status = match crate::daemon::toggle(shared, start_tx, pipeline_tx, cfg)? {
            Toggle::Started | Toggle::Debounced => "recording",
            Toggle::Stopping => "stopping",
            Toggle::Finishing | Toggle::Busy => "processing",
        };
        return Ok(WebResponse::Json(json!({ "ok": true, "phase": status })));
    }
    if method == "POST" && clean_path == "/api/upload-transcribe" {
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
            crate::userdata::append_history("uploaded", &text, Some(&audio_id), None, None);
        }
        return Ok(WebResponse::Json(json!({ "ok": true, "text": text })));
    }
    if method == "POST" && clean_path == "/api/config" {
        let changes: Value = serde_json::from_str(body).context("bad JSON body")?;
        // Checked before anything is written, so a bad value saves nothing.
        let pill_style = match changes["pill_style"].as_str() {
            Some(name) => Some(PillStyle::parse(name).with_context(|| {
                format!("unknown pill style {name:?} (small, large or hidden)")
            })?),
            None => None,
        };
        let pill_show_when_idle = changes["pill_show_when_idle"].as_bool();
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
        if let Some(v) = changes["enhance_model"].as_str() {
            doc.set(&["enhance", "model"], v.into());
        }
        if let Some(v) = changes["groq_api_key"].as_str() {
            let key = v.trim();
            if !key.is_empty() {
                let _ = crate::userdata::save_groq_api_key(key);
            }
        }
        if let Some(v) = changes["smart_code"].as_bool() {
            doc.set(&["formatting", "smart_code"], v.into());
        }
        if let Some(v) = changes["paragraphs"].as_bool() {
            doc.set(&["formatting", "paragraphs"], v.into());
        }
        if let Some(v) = changes["list_cues"].as_bool() {
            doc.set(&["formatting", "list_cues"], v.into());
        }
        if let Some(v) = changes["jev_enabled"].as_bool() {
            doc.set(&["formatting", "jev", "enabled"], v.into());
        }
        if let Some(v) = changes["jev_model"].as_str() {
            doc.set(&["formatting", "jev", "model"], v.into());
        }
        if let Some(v) = changes["jev_timeout_ms"].as_i64() {
            doc.set(&["formatting", "jev", "timeout_ms"], v.into());
        }
        if let Some(v) = changes["openrouter_api_key"]
            .as_str()
            .or_else(|| changes["jev_api_key"].as_str())
        {
            let key = v.trim();
            if !key.is_empty() {
                let provider = crate::jev::JevProvider::for_key(key);
                if provider == crate::jev::JevProvider::OpenRouter {
                    let _ = crate::userdata::save_openrouter_api_key(key);
                }
                let provider_name = match provider {
                    crate::jev::JevProvider::TypeSafe => "typesafe",
                    crate::jev::JevProvider::OpenRouter => "openrouter",
                };
                doc.set(&["formatting", "jev", "provider"], provider_name.into());
                doc.set(&["formatting", "jev", "api_key"], key.into());
            }
        }
        doc.save()?;
        // The pill applies live (the supervisor and the helper follow the event
        // stream), so it is saved after the rest and needs no restart.
        if pill_style.is_some() || pill_show_when_idle.is_some() {
            let events = shared.lock().unwrap().events.clone();
            crate::pill::save_settings(config_path, &events, pill_style, pill_show_when_idle)?;
        }
        let only_pill = changes
            .as_object()
            .is_some_and(|keys| keys.keys().all(|k| k.starts_with("pill_")));
        eprintln!("[web] config saved");
        return Ok(WebResponse::Json(
            json!({ "ok": true, "needs_restart": !only_pill }),
        ));
    }
    if method == "POST" && clean_path == "/api/restart" {
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
    if method == "GET" && clean_path == "/api/vocab" {
        return Ok(WebResponse::Text(vocab_terms().join("\n")));
    }
    if method == "PUT" && clean_path == "/api/vocab" {
        crate::userdata::write_keeping_comments("vocabulary.txt", body)?;
        return Ok(WebResponse::Json(json!({ "ok": true })));
    }
    if method == "PUT" && clean_path == "/api/enhance-prompt" {
        crate::userdata::write_keeping_comments("enhance_prompt.txt", body)?;
        return Ok(WebResponse::Json(json!({ "ok": true })));
    }
    if method == "GET" && clean_path == "/api/scratchpad" {
        return Ok(WebResponse::Text(crate::userdata::read_scratchpad()));
    }
    if method == "PUT" && clean_path == "/api/scratchpad" {
        crate::userdata::write_scratchpad(body)?;
        return Ok(WebResponse::Json(json!({ "ok": true })));
    }
    if method == "POST" && clean_path == "/api/enhance" {
        anyhow::ensure!(!body.trim().is_empty(), "nothing to enhance");
        let runtime = tokio::runtime::Runtime::new()?;
        let enhanced = runtime.block_on(crate::enhance::enhance(&cfg.enhance, body))?;
        crate::userdata::append_history("enhanced", &enhanced, None, None, None);
        shared.lock().unwrap().last_text = Some(enhanced.clone());
        return Ok(WebResponse::Json(json!({ "text": enhanced })));
    }
    if method == "POST" && clean_path == "/api/hotkeys" {
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
    if method == "POST" && clean_path == "/api/mic-test" {
        let fresh = Config::load(config_path)?;
        let phase = shared.lock().unwrap().phase;
        anyhow::ensure!(
            phase == Phase::Idle,
            "daemon is {} — finish the dictation first",
            phase.as_str()
        );
        let (text, audio_id) = crate::mictest::run(&fresh, 3)?;
        return Ok(WebResponse::Json(
            json!({ "text": text, "audio_id": audio_id }),
        ));
    }
    Ok(WebResponse::NotFound)
}

fn vocab_terms() -> Vec<String> {
    let text = std::fs::read_to_string(crate::userdata::config_dir().join("vocabulary.txt"))
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
            let s = String::from_utf8_lossy(&o.stdout)
                .trim()
                .trim_matches('\'')
                .to_string();
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

fn state(config_path: &Path, shared: &Arc<Mutex<Shared>>) -> anyhow::Result<Value> {
    let doc = ConfigDoc::load(config_path)?;
    let status = shared.lock().unwrap().phase.as_str().to_string();
    let enhance_prompt = crate::userdata::enhance_prompt().unwrap_or_default();
    let st = crate::format::stats();
    let jev_stats = json!({
        "dictations": st.dictations,
        "local_only": st.local_only,
        "calls": st.calls,
        "fallbacks": st.fallbacks,
        "avg_latency_ms": st.total_latency_ms.checked_div(st.calls),
    });
    let jev_target = Config::load(config_path)
        .ok()
        .and_then(|c| c.formatting.jev.resolve());
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
        "enhance_model": doc.str_at(&["enhance", "model"], "llama-3.3-70b-versatile"),
        "has_groq_api_key": crate::enhance::get_groq_api_key().is_ok(),
        "smart_code": doc.bool_at(&["formatting", "smart_code"], true),
        "jev_enabled": doc.bool_at(&["formatting", "jev", "enabled"], true),
        "jev_model": jev_target.as_ref().map_or("jev-latest", |t| t.model.as_str()),
        "jev_provider": jev_target.as_ref().map(|t| t.provider),
        "paragraphs": doc.bool_at(&["formatting", "paragraphs"], true),
        "list_cues": doc.bool_at(&["formatting", "list_cues"], true),
        "pill_style": doc.str_at(&["pill", "style"], PillConfig::default().style.as_str()),
        "pill_show_when_idle": doc.bool_at(&["pill", "show_when_idle"], true),
        // Only macOS has a pill renderer so far.
        "pill_supported": cfg!(target_os = "macos"),
        "jev_timeout_ms": doc.int_at(
            &["formatting", "jev", "timeout_ms"],
            crate::jev::DEFAULT_TIMEOUT_MS as i64,
        ),
        "jev_stats": jev_stats,
        "has_jev_api_key": jev_target.is_some(),
        // Kept for older dashboard builds that still read this name.
        "has_openrouter_api_key": jev_target.is_some(),
        "scratchpad": crate::userdata::read_scratchpad(),
        "history": crate::userdata::read_history(100),
        "models": MODELS.iter().map(|(m, s)| json!({ "name": m, "speed": s })).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::Transcript;
    use crossbeam_channel::unbounded;

    struct DummyStt;
    #[async_trait::async_trait]
    impl SttProvider for DummyStt {
        async fn transcribe(&self, _wav_bytes: Vec<u8>) -> anyhow::Result<Transcript> {
            Ok(Transcript {
                text: "test".to_string(),
                raw_json: "{}".to_string(),
                latency_ms: 0,
            })
        }
    }

    #[test]
    fn test_web_toggle_initializes_vision_session() {
        let (start_tx, _start_rx) = unbounded();
        let (pipeline_tx, _pipeline_rx) = unbounded();
        let stt: Arc<dyn SttProvider> = Arc::new(DummyStt);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let mut cfg = Config::load(std::path::Path::new("config.toml")).unwrap();
        cfg.vision.enabled = true;

        let res = route(
            "POST",
            "/api/toggle",
            "",
            &[],
            Path::new("config.toml"),
            &shared,
            &cfg,
            &start_tx,
            &pipeline_tx,
            &stt,
        )
        .unwrap();

        assert!(matches!(res, WebResponse::Json(_)));
        let s = shared.lock().unwrap();
        assert_eq!(s.phase, Phase::Recording);
        assert!(
            s.vision_detector.is_some(),
            "Vision detector should be initialized on toggle"
        );
        assert!(
            s.vision_session_dir.is_some(),
            "Vision session dir should be initialized on toggle"
        );
    }

    #[test]
    fn test_web_toggle_disabled_vision() {
        let (start_tx, _start_rx) = unbounded();
        let (pipeline_tx, _pipeline_rx) = unbounded();
        let stt: Arc<dyn SttProvider> = Arc::new(DummyStt);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let mut cfg = Config::load(std::path::Path::new("config.toml")).unwrap();
        cfg.vision.enabled = false;

        let res = route(
            "POST",
            "/api/toggle",
            "",
            &[],
            Path::new("config.toml"),
            &shared,
            &cfg,
            &start_tx,
            &pipeline_tx,
            &stt,
        )
        .unwrap();

        assert!(matches!(res, WebResponse::Json(_)));
        let s = shared.lock().unwrap();
        assert_eq!(s.phase, Phase::Recording);
        assert!(
            s.vision_detector.is_none(),
            "Vision detector must not be initialized when disabled"
        );
        assert!(
            s.vision_session_dir.is_none(),
            "Vision session dir must not be initialized when disabled"
        );
    }

    #[test]
    fn test_web_toggle_is_debounced_like_the_hotkey() {
        let (start_tx, _start_rx) = unbounded();
        let (pipeline_tx, _pipeline_rx) = unbounded();
        let (control_tx, control_rx) = unbounded();
        let stt: Arc<dyn SttProvider> = Arc::new(DummyStt);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let mut cfg = Config::load(std::path::Path::new("config.toml")).unwrap();
        cfg.vision.enabled = false;
        {
            let mut s = shared.lock().unwrap();
            s.set_phase(Phase::Recording);
            s.toggle_t0 = Some(std::time::Instant::now());
            s.control_tx = Some(control_tx);
        }
        let post_toggle = || {
            route(
                "POST",
                "/api/toggle",
                "",
                &[],
                Path::new("config.toml"),
                &shared,
                &cfg,
                &start_tx,
                &pipeline_tx,
                &stt,
            )
            .unwrap()
        };

        // Within 800 ms of the start a second toggle is an accidental double tap.
        post_toggle();
        assert!(control_rx.try_recv().is_err());

        shared.lock().unwrap().toggle_t0 =
            std::time::Instant::now().checked_sub(std::time::Duration::from_secs(1));
        match post_toggle() {
            WebResponse::Json(v) => assert_eq!(v["phase"], "stopping"),
            _ => panic!("expected json"),
        }
        assert!(matches!(
            control_rx.try_recv(),
            Ok(crate::vad::Control::ForceStop)
        ));
    }

    #[test]
    fn test_image_route_serves_valid_image_and_rejects_unauthorized() {
        let (start_tx, _start_rx) = unbounded();
        let (pipeline_tx, _pipeline_rx) = unbounded();
        let stt: Arc<dyn SttProvider> = Arc::new(DummyStt);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let cfg = Config::load(std::path::Path::new("config.toml")).unwrap();

        // 1. Create a dummy image in temp dir
        let temp_dir = std::env::temp_dir();
        let test_img_path = temp_dir.join("bolo_test_context_image.png");
        std::fs::write(&test_img_path, b"mock-png-data").unwrap();

        // 2. Request image via route
        let res = route(
            "GET",
            &format!("/api/image?path={}", test_img_path.to_string_lossy()),
            "",
            &[],
            Path::new("config.toml"),
            &shared,
            &cfg,
            &start_tx,
            &pipeline_tx,
            &stt,
        )
        .unwrap();

        match res {
            WebResponse::Image(bytes) => assert_eq!(bytes, b"mock-png-data"),
            _ => panic!("Expected WebResponse::Image"),
        }

        // 3. Unauthorized path traversal check (e.g. /etc/passwd or /var/log)
        let unauth_res = route(
            "GET",
            "/api/image?path=/etc/passwd",
            "",
            &[],
            Path::new("config.toml"),
            &shared,
            &cfg,
            &start_tx,
            &pipeline_tx,
            &stt,
        )
        .unwrap();

        assert!(matches!(unauth_res, WebResponse::NotFound));

        // Clean up
        let _ = std::fs::remove_file(test_img_path);
    }

    #[test]
    fn test_state_includes_enhance_and_smart_code() {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let s = state(Path::new("config.toml"), &shared).unwrap();
        assert!(s.get("enhance_model").is_some());
        assert!(s.get("has_groq_api_key").is_some());
        assert!(s.get("smart_code").is_some());
        assert!(s.get("jev_enabled").is_some());
        assert!(s.get("jev_model").is_some());
        assert!(s.get("jev_timeout_ms").is_some());
        assert!(s.get("has_openrouter_api_key").is_some());
    }

    #[test]
    fn test_state_includes_the_pill_settings() {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let s = state(Path::new("config.toml"), &shared).unwrap();
        assert_eq!(s["pill_style"], "small");
        assert_eq!(s["pill_show_when_idle"], true);
        assert_eq!(s["pill_supported"], cfg!(target_os = "macos"));
    }

    /// A private copy of the shipped config, so a test never edits the repo's.
    fn scratch_config(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "bolo-web-test-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::copy("config.toml", &path).unwrap();
        path
    }

    fn post_config(
        body: &str,
        config_path: &Path,
        shared: &Arc<Mutex<Shared>>,
    ) -> anyhow::Result<Value> {
        let (start_tx, _start_rx) = unbounded();
        let (pipeline_tx, _pipeline_rx) = unbounded();
        let stt: Arc<dyn SttProvider> = Arc::new(DummyStt);
        let cfg = Config::load(config_path).unwrap();
        match route(
            "POST",
            "/api/config",
            body,
            body.as_bytes(),
            config_path,
            shared,
            &cfg,
            &start_tx,
            &pipeline_tx,
            &stt,
        )? {
            WebResponse::Json(v) => Ok(v),
            _ => panic!("expected JSON"),
        }
    }

    #[test]
    fn test_pill_settings_round_trip_through_api_config_and_apply_live() {
        use crate::events::testing::{next, subscribe};
        let path = scratch_config("pill");
        let hub = crate::events::EventHub::spawn(PillConfig::default());
        let shared = Arc::new(Mutex::new(Shared::default()));
        shared.lock().unwrap().events = hub.clone();
        let events = subscribe(&hub);

        let reply = post_config(
            r#"{"pill_style":"large","pill_show_when_idle":false}"#,
            &path,
            &shared,
        )
        .unwrap();

        // The pill is applied live: no restart, and subscribers hear about it.
        assert_eq!(reply["needs_restart"], false);
        assert_eq!(
            next(&events),
            json!({ "type": "config", "style": "large", "show_idle": false })
        );
        assert_eq!(hub.pill().style, PillStyle::Large);
        let saved = Config::load(&path).unwrap().pill;
        assert_eq!(saved.style, PillStyle::Large);
        assert!(!saved.show_when_idle);
        let s = state(&path, &shared).unwrap();
        assert_eq!(s["pill_style"], "large");
        assert_eq!(s["pill_show_when_idle"], false);

        // Other settings still ask for a restart, and leave the pill alone.
        let reply = post_config(r#"{"sounds":false}"#, &path, &shared).unwrap();
        assert_eq!(reply["needs_restart"], true);
        assert_eq!(Config::load(&path).unwrap().pill.style, PillStyle::Large);

        // One request can carry both kinds; the pill part still lands.
        let reply =
            post_config(r#"{"sounds":true,"pill_style":"hidden"}"#, &path, &shared).unwrap();
        assert_eq!(reply["needs_restart"], true);
        assert_eq!(Config::load(&path).unwrap().pill.style, PillStyle::Hidden);
        assert!(Config::load(&path).unwrap().daemon.sounds);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn test_unknown_pill_style_is_rejected_and_nothing_is_saved() {
        let path = scratch_config("badpill");
        let before = std::fs::read_to_string(&path).unwrap();
        let shared = Arc::new(Mutex::new(Shared::default()));

        let err =
            post_config(r#"{"sounds":false,"pill_style":"huge"}"#, &path, &shared).unwrap_err();

        assert!(err.to_string().contains("unknown pill style"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
