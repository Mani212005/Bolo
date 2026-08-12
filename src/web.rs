//! Settings app backend: tiny HTTP server inside the daemon, bound to
//! 127.0.0.1 only (single-user, localhost — no auth by design). Serves the
//! glassmorphism UI (src/ui/app.html) and a small JSON API over the same
//! state the daemon uses.

use crate::config::Config;
use crate::config_edit::{ConfigDoc, MODELS};
use crate::daemon::{Phase, Shared};
use anyhow::Context;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const APP_HTML: &str = include_str!("ui/app.html");
const HOTKEY_ACTIONS: [(&str, &str); 3] =
    [("toggle", "Bolo toggle"), ("pause", "Bolo pause"), ("insert", "Bolo insert")];

pub fn serve(config_path: PathBuf, shared: Arc<Mutex<Shared>>, cfg: Config) {
    let mut port = cfg.ui.port;
    let server = loop {
        match tiny_http::Server::http(("127.0.0.1", port)) {
            Ok(s) => break s,
            Err(e) if port < cfg.ui.port + 5 => {
                eprintln!("[web] port {port} unavailable ({e}); trying {}", port + 1);
                port += 1;
            }
            Err(e) => {
                eprintln!("[web] settings app disabled: {e}");
                return;
            }
        }
    };
    eprintln!("[web] settings app on http://127.0.0.1:{port}");
    for request in server.incoming_requests() {
        let config_path = config_path.clone();
        let shared = Arc::clone(&shared);
        let cfg = cfg.clone();
        // Thread per request: mic-test and enhance block for seconds.
        std::thread::spawn(move || {
            let method = request.method().as_str().to_string();
            let url = request.url().to_string();
            let mut request = request;
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);
            let result = route(&method, &url, &body, &config_path, &shared, &cfg);
            let response = match result {
                Ok((content_type, payload)) => tiny_http::Response::from_string(payload)
                    .with_header(
                        tiny_http::Header::from_bytes("Content-Type", content_type).unwrap(),
                    ),
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
    config_path: &PathBuf,
    shared: &Arc<Mutex<Shared>>,
    cfg: &Config,
) -> anyhow::Result<(&'static str, String)> {
    let json_ok = |v: Value| Ok(("application/json", v.to_string()));
    match (method, url) {
        ("GET", "/") => Ok(("text/html; charset=utf-8", APP_HTML.to_string())),
        ("GET", "/api/state") => json_ok(state(config_path, shared)?),
        ("POST", "/api/config") => {
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
            json_ok(json!({ "ok": true, "needs_restart": true }))
        }
        ("POST", "/api/restart") => {
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
            json_ok(json!({ "ok": true }))
        }
        ("GET", "/api/vocab") => Ok(("text/plain", vocab_terms().join("\n"))),
        ("PUT", "/api/vocab") => {
            crate::userdata::write_keeping_comments("vocabulary.txt", body)?;
            json_ok(json!({ "ok": true }))
        }
        ("PUT", "/api/enhance-prompt") => {
            crate::userdata::write_keeping_comments("enhance_prompt.txt", body)?;
            json_ok(json!({ "ok": true }))
        }
        ("GET", "/api/scratchpad") => Ok(("text/plain", crate::userdata::read_scratchpad())),
        ("PUT", "/api/scratchpad") => {
            crate::userdata::write_scratchpad(body)?;
            json_ok(json!({ "ok": true }))
        }
        ("DELETE", "/api/history") => {
            crate::userdata::clear_history();
            json_ok(json!({ "ok": true }))
        }
        ("POST", "/api/enhance") => {
            anyhow::ensure!(!body.trim().is_empty(), "nothing to enhance");
            let runtime = tokio::runtime::Runtime::new()?;
            let enhanced = runtime.block_on(crate::enhance::enhance(&cfg.enhance, body))?;
            crate::userdata::append_history("enhanced", &enhanced);
            shared.lock().unwrap().last_text = Some(enhanced.clone());
            json_ok(json!({ "text": enhanced }))
        }
        ("POST", "/api/hotkeys") => {
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
            json_ok(json!({ "ok": true }))
        }
        ("POST", "/api/mic-test") => {
            let fresh = Config::load(config_path)?;
            let phase = shared.lock().unwrap().phase;
            anyhow::ensure!(
                phase == Phase::Idle,
                "daemon is {} — finish the dictation first",
                phase.as_str()
            );
            let text = crate::mictest::run(&fresh, 3)?;
            json_ok(json!({ "text": text }))
        }
        _ => Ok(("text/plain", "not found".into())),
    }
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
        "max_len": doc.int_at(&["vad", "max_utterance_ms"], 300_000),
        "hotkeys": read_hotkeys(),
        "vocab": vocab_terms(),
        "enhance_prompt": enhance_prompt,
        "scratchpad": crate::userdata::read_scratchpad(),
        "history": crate::userdata::read_history(50),
        "models": MODELS.iter().map(|(m, s)| json!({ "name": m, "speed": s })).collect::<Vec<_>>(),
    }))
}
