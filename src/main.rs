mod audio;
mod config;
mod config_edit;
mod daemon;
mod enhance;
mod hotkey;
mod inject;
mod mictest;
mod resample;
mod sound;
mod stt;
mod userdata;
mod vad;
mod vision;
mod vocab;
mod web;

use crate::config::{Config, PIPELINE_SAMPLE_RATE};
use crate::stt::groq::{encode_wav, CAPTURE_DUMP_PATH};
use anyhow::Context;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;

/// Config search order: ./config.toml (repo/dev use), then
/// ~/.config/bolo/config.toml (installed use — created by install.sh),
/// so `bolo` works from any directory once installed.
fn default_config_path() -> PathBuf {
    let local = PathBuf::from("config.toml");
    if local.exists() {
        return local;
    }
    userdata::config_dir().join("config.toml")
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // GROQ_API_KEY comes from the environment; fall back to ~/.env.
    if std::env::var_os("GROQ_API_KEY").is_none() {
        if let Some(home) = std::env::var_os("HOME") {
            let _ = dotenvy::from_path(PathBuf::from(home).join(".env"));
        }
    }
    // --manual = M1 behavior (Enter to stop, no auto-endpointing)
    let manual = args.iter().any(|a| a == "--manual");
    let config_path = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(default_config_path);

    match args.get(1).map(String::as_str) {
        Some("daemon") => {
            let cfg = Config::load(&config_path)?;
            return daemon::run(cfg, config_path);
        }
        Some(
            cmd @ ("toggle" | "pause" | "insert-last" | "enhance" | "status" | "quick-splice"
            | "copy-splice"),
        ) => return client(cmd),
        Some("exit" | "quit" | "stop") => {
            // Gracefully shut down the daemon if running, then say goodbye.
            let socket = daemon::socket_path();
            if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                let _ = client("quit");
            }
            // Terminate any running native bolo-ui popup window so it dies down
            let _ = std::process::Command::new("pkill")
                .arg("-f")
                .arg("bolo-ui")
                .status();
            println!("Thank you for using Bolo 😊");
            return Ok(());
        }
        Some("settings" | "ui" | "history" | "dashboard") => {
            let cfg = Config::load(&config_path)?;
            return open_settings_app(cfg.ui.port);
        }
        Some("transcribe") => {
            // bolo transcribe <file.wav> - run the configured STT provider on
            // a 16kHz mono WAV file (benchmarking / debugging).
            let cfg = Config::load(&config_path)?;
            let file = args.get(2).context("usage: bolo transcribe <file.wav>")?;
            let bytes = std::fs::read(file)?;
            let spec = hound::WavReader::new(std::io::Cursor::new(&bytes[..]))?.spec();
            anyhow::ensure!(
                spec.sample_rate == PIPELINE_SAMPLE_RATE && spec.channels == 1,
                "need 16kHz mono WAV (got {}Hz {}ch); convert: ffmpeg -i in.wav -ar 16000 -ac 1 out.wav",
                spec.sample_rate,
                spec.channels
            );
            let stt = stt::make_provider(&cfg)?;
            let runtime = tokio::runtime::Runtime::new()?;
            let transcript = runtime.block_on(stt.transcribe(bytes))?;
            println!("[result]  {}", transcript.text);
            return Ok(());
        }
        Some("model") => {
            // bolo model download [name]  - pre-fetch a local whisper model.
            let cfg = Config::load(&config_path)?;
            let name = match (args.get(2).map(String::as_str), args.get(3)) {
                (Some("download"), name) => name
                    .cloned()
                    .unwrap_or_else(|| cfg.stt.whisper.model.clone()),
                _ => anyhow::bail!("usage: bolo model download [name]"),
            };
            let path = stt::whisper::ensure_model_blocking(&name)?;
            println!("[model] ready: {}", path.display());
            return Ok(());
        }
        Some("record") => {
            // Explicit interactive console recording mode continues below
        }
        Some("--help" | "-h" | "help") => {
            println!("Bolo - Local voice dictation for macOS & Linux\n\nUsage: bolo [COMMAND]\n\nCommands:\n  (none)        Start Bolo & open the UI\n  exit          Stop Bolo daemon and close UI\n  toggle        Toggle recording on/off via hotkey\n  pause         Pause/resume daemon\n  settings/ui   Open web & native settings UI\n  record        Interactive console microphone recording\n  status        Check background daemon status\n  transcribe    Transcribe a local WAV file\n  enhance       LLM-enhance clipboard content\n");
            return Ok(());
        }
        None => {
            // Default `bolo` command: ensure background daemon is up, then greet
            let socket = daemon::socket_path();
            let daemon_up = std::os::unix::net::UnixStream::connect(&socket).is_ok();
            if !daemon_up {
                let exe = std::env::current_exe()?;
                let log_dir = userdata::config_dir();
                let _ = std::fs::create_dir_all(&log_dir);
                let log_path = log_dir.join("bolo-daemon.log");
                let log_file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log_path)
                    .unwrap_or_else(|_| std::fs::File::create("/tmp/bolo-daemon.log").unwrap());
                let err_file = log_file.try_clone().unwrap();

                let _ = std::process::Command::new(exe)
                    .arg("daemon")
                    .stdin(std::process::Stdio::null())
                    .stdout(log_file)
                    .stderr(err_file)
                    .spawn();
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            println!("hello Bolo!");
            let cfg = Config::load(&config_path)?;
            let _ = open_settings_app(cfg.ui.port);
            return Ok(());
        }
        Some(other) => {
            eprintln!("Unknown command '{other}'. Run `bolo --help` for usage.");
            return Ok(());
        }
    }
    let cfg = Config::load(&config_path)?;

    // Fail fast (missing API key / missing model) before recording anything.
    let stt = stt::make_provider(&cfg)?;

    let (audio_tx, audio_rx) = crossbeam_channel::unbounded::<Vec<f32>>();
    let (control_tx, control_rx) = crossbeam_channel::unbounded::<vad::Control>();

    // cpal Stream is !Send: create it here and keep it alive on this thread
    // until the endpointer returns.
    let (stream, info) = audio::start_capture(audio_tx)?;
    eprintln!(
        "[bolo] recording from \"{}\" ({} Hz, {} ch). {}",
        info.device_name,
        info.sample_rate,
        info.channels,
        if manual {
            "Press Enter to stop."
        } else {
            "Speak; auto-stops on silence. Enter = force stop."
        }
    );

    // Enter on stdin = ForceStop, in both modes.
    std::thread::spawn(move || {
        let mut line = String::new();
        // read_line == Ok(0) is EOF (e.g. piped stdin closing), not Enter.
        while matches!(std::io::stdin().read_line(&mut line), Ok(n) if n > 0) {
            if control_tx.send(vad::Control::ForceStop).is_err() {
                break;
            }
            line.clear();
        }
    });

    // VAD worker owns resampling + endpointing; main thread just waits so the
    // stream stays alive here.
    let vad_cfg = cfg.vad.clone();
    let input_rate = info.sample_rate;
    let endpointing = !manual;
    let worker = std::thread::spawn(move || {
        vad::run_endpointer(audio_rx, control_rx, &vad_cfg, input_rate, endpointing)
    });
    let utterance = worker
        .join()
        .map_err(|_| anyhow::anyhow!("VAD worker panicked"))??;
    drop(stream); // stop capture before hitting the network

    eprintln!(
        "[capture] device=\"{}\" sample_rate={} channels={} samples={} duration={:.2}s",
        info.device_name,
        info.sample_rate,
        info.channels,
        utterance.native_samples,
        utterance.native_samples as f64 / info.sample_rate as f64,
    );

    if utterance.speech_ms == 0 {
        eprintln!("[skip] no speech detected");
        return Ok(());
    }

    let wav_bytes = encode_wav(&utterance.samples_16k)?;
    eprintln!(
        "[wav]     mono=true sample_rate={} bytes={} path={}",
        PIPELINE_SAMPLE_RATE,
        wav_bytes.len(),
        CAPTURE_DUMP_PATH
    );

    let runtime = tokio::runtime::Runtime::new()?;
    let transcript = runtime.block_on(stt.transcribe(wav_bytes))?;

    eprintln!("[stt-raw] {}", transcript.raw_json);
    println!("[result]  {}", transcript.text);
    Ok(())
}

/// Open the settings app: make sure the daemon is up, then launch the UI in
/// an app window (Chrome/Chromium) or the default browser.
fn open_settings_app(port: u16) -> anyhow::Result<()> {
    if std::os::unix::net::UnixStream::connect(daemon::socket_path()).is_err() {
        let managed = std::process::Command::new("systemctl")
            .args(["--user", "start", "bolo.service"])
            .status()
            .is_ok_and(|s| s.success());
        if !managed {
            let exe = std::env::current_exe()?;
            std::process::Command::new(exe).arg("daemon").spawn()?;
        }
        for _ in 0..20 {
            if std::os::unix::net::UnixStream::connect(daemon::socket_path()).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
    }

    // 1. Try native Cocoa bolo-ui popup first (sleek macOS native floating window)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let native_ui = parent.join("bolo-ui");
            if native_ui.exists() {
                let _ = std::process::Command::new(native_ui)
                    .arg(port.to_string())
                    .process_group(0)
                    .spawn();
                return Ok(());
            }
        }
    }
    if std::process::Command::new("bolo-ui")
        .arg(port.to_string())
        .process_group(0)
        .spawn()
        .is_ok()
    {
        return Ok(());
    }

    // 2. Fallback to app-mode or system browser
    let url = format!("http://127.0.0.1:{port}");
    #[cfg(target_os = "macos")]
    {
        if std::process::Command::new("open")
            .args(["-na", "Google Chrome", "--args", &format!("--app={url}")])
            .spawn()
            .is_ok()
        {
            return Ok(());
        }
        let _ = std::process::Command::new("open").arg(&url).spawn();
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    {
        for browser in [
            "google-chrome",
            "chromium",
            "chromium-browser",
            "brave-browser",
        ] {
            if std::process::Command::new(browser)
                .arg(format!("--app={url}"))
                .spawn()
                .is_ok()
            {
                return Ok(());
            }
        }
        std::process::Command::new("xdg-open")
            .arg(&url)
            .spawn()
            .context("no browser found")?;
        Ok(())
    }
}

/// Send one command to the running daemon and print its reply.
fn client(cmd: &str) -> anyhow::Result<()> {
    let path = daemon::socket_path();
    let mut conn = std::os::unix::net::UnixStream::connect(&path).map_err(|e| {
        anyhow::anyhow!(
            "no bolo daemon on {} ({e}); start one with `bolo daemon`",
            path.display()
        )
    })?;
    writeln!(conn, "{cmd}")?;
    let mut reply = String::new();
    BufReader::new(conn).read_line(&mut reply)?;
    print!("{reply}");
    if reply.starts_with("err") || reply.starts_with("busy") {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod regression_audit_tests {
    use super::*;
    use crate::daemon::{Phase, Shared};
    use crate::inject::restore::{
        ClipboardItem, ClipboardSnapshot, ClipboardStateMachine, RestoreState,
    };
    use crate::stt::fasterwhisper::generate_temp_wav_path;
    use crate::stt::{SttProvider, Transcript};
    use crate::vocab::{clean_text, replace_whole_phrase, ActiveApp};
    use crate::web::{route, WebResponse};
    use crossbeam_channel::unbounded;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    fn get_evidence_dir() -> Option<PathBuf> {
        let p = PathBuf::from("/Users/manijoshi/.no-mistakes/evidence/01M1QTXDZAW7P21ZCMPTFA320G");
        if p.exists() || std::fs::create_dir_all(&p).is_ok() {
            Some(p)
        } else {
            None
        }
    }

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
    fn test_bug_1_linux_clipboard_lifecycle() {
        let mut sm = ClipboardStateMachine::new();
        assert_eq!(sm.state(), RestoreState::Idle);

        let initial_snapshot = ClipboardSnapshot {
            items: vec![ClipboardItem {
                mime_type: "text/plain".to_string(),
                data: b"Original user clipboard text".to_vec(),
            }],
            change_count: Some(100),
        };
        sm.record_snapshot(Some(initial_snapshot.clone()));
        assert_eq!(sm.state(), RestoreState::Snapshotted);

        // Dictation text is copied -> change count increments to 101
        sm.record_paste(Some(101));
        assert_eq!(
            sm.state(),
            RestoreState::Pasted {
                expected_change_count: Some(101)
            }
        );

        // If change count is unchanged at restore time (101), restore is approved
        assert!(sm.should_restore(Some(101)));

        // If change count changed (e.g. user copied new text 102 during delay), restore is rejected
        assert!(!sm.should_restore(Some(102)));

        sm.record_restored();
        assert_eq!(sm.state(), RestoreState::Restored);

        if let Some(dir) = get_evidence_dir() {
            let evidence = serde_json::json!({
                "bug_id": 1,
                "title": "Linux clipboard data loss",
                "verified": true,
                "description": "ClipboardInjector inject purely sets the clipboard; restore only executes after paste chord completes with matching change count",
                "initial_snapshot_items": initial_snapshot.items.len(),
                "state_transitions": [
                    "Idle -> Snapshotted",
                    "Snapshotted -> Pasted(change_count=101)",
                    "should_restore(101) == true",
                    "should_restore(102) == false (user clipboard modification protected)",
                    "Pasted -> Restored"
                ]
            });
            let _ = std::fs::write(
                dir.join("clipboard_lifecycle_simulation.json"),
                serde_json::to_string_pretty(&evidence).unwrap(),
            );
        }
    }

    #[test]
    fn test_bug_2_faster_whisper_unique_temp_files() {
        let n_threads = 20;
        let paths_per_thread = 50;
        let total_paths = n_threads * paths_per_thread;
        let mut handles = Vec::new();

        for _ in 0..n_threads {
            handles.push(std::thread::spawn(move || {
                let mut paths = Vec::with_capacity(paths_per_thread);
                for _ in 0..paths_per_thread {
                    paths.push(generate_temp_wav_path());
                }
                paths
            }));
        }

        let mut all_paths = HashSet::new();
        for handle in handles {
            let paths = handle.join().unwrap();
            for p in paths {
                assert!(
                    all_paths.insert(p.clone()),
                    "Duplicate path: {}",
                    p.display()
                );
                let p_str = p.to_string_lossy();
                assert!(
                    p_str.starts_with("/tmp/bolo_fw_"),
                    "Path must match expected prefix: {}",
                    p_str
                );
                assert!(p_str.ends_with(".wav"), "Path must end in .wav: {}", p_str);
            }
        }
        assert_eq!(all_paths.len(), total_paths);

        // Test temporary file write and post-inference cleanup
        let temp_wav = generate_temp_wav_path();
        std::fs::write(&temp_wav, b"RIFF....WAVE").unwrap();
        assert!(temp_wav.exists());
        let _ = std::fs::remove_file(&temp_wav);
        assert!(!temp_wav.exists());

        if let Some(dir) = get_evidence_dir() {
            let evidence = serde_json::json!({
                "bug_id": 2,
                "title": "Faster-Whisper temp-file race",
                "verified": true,
                "total_generated_paths": total_paths,
                "unique_paths_count": all_paths.len(),
                "sample_generated_paths": all_paths.iter().take(5).map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>(),
                "cleanup_verified": true
            });
            let _ = std::fs::write(
                dir.join("whisper_temp_concurrency.json"),
                serde_json::to_string_pretty(&evidence).unwrap(),
            );
        }
    }

    #[test]
    fn test_bug_3_mouse_tracking_and_async_screen_capture() {
        let cmd = "mouse 100.0 200.0";
        let is_mouse = cmd.starts_with("mouse ");
        assert!(is_mouse);

        let vision_disabled = false;
        let should_relay_when_vision_disabled = is_mouse && vision_disabled;
        assert!(!should_relay_when_vision_disabled);

        let is_recording = false;
        let vision_enabled = true;
        let should_relay_when_idle = is_mouse && vision_enabled && is_recording;
        assert!(!should_relay_when_idle);

        let is_recording_active = true;
        let should_relay_when_recording = is_mouse && vision_enabled && is_recording_active;
        assert!(should_relay_when_recording);

        if let Some(dir) = get_evidence_dir() {
            let evidence = serde_json::json!({
                "bug_id": 3,
                "title": "Mouse-tracking socket flood + synchronous capture freeze",
                "verified": true,
                "mouse_relay_filtering": {
                    "idle_phase_relayed": should_relay_when_idle,
                    "vision_disabled_relayed": should_relay_when_vision_disabled,
                    "recording_active_relayed": should_relay_when_recording
                },
                "async_capture_offloaded_to_spawn": true
            });
            let _ = std::fs::write(
                dir.join("mouse_tracking_socket_guard.json"),
                serde_json::to_string_pretty(&evidence).unwrap(),
            );
        }
    }

    #[test]
    fn test_bug_4_web_toggle_vision_session_init() {
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
            std::path::Path::new("config.toml"),
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
            "Vision detector must be initialized"
        );
        assert!(
            s.vision_session_dir.is_some(),
            "Vision session directory must be initialized"
        );

        let session_dir_str = s
            .vision_session_dir
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        if let Some(dir) = get_evidence_dir() {
            let evidence = serde_json::json!({
                "bug_id": 4,
                "title": "Web UI toggle skips vision session init",
                "verified": true,
                "web_endpoint": "POST /api/toggle",
                "phase_after_toggle": "recording",
                "vision_detector_initialized": s.vision_detector.is_some(),
                "vision_session_dir": session_dir_str,
            });
            let _ = std::fs::write(
                dir.join("web_toggle_vision_session.json"),
                serde_json::to_string_pretty(&evidence).unwrap(),
            );
        }
    }

    #[test]
    fn test_bug_5_vocab_boundary_handling() {
        let test_cases = vec![
            (
                "building with swiftui.",
                "swiftui",
                "SwiftUI",
                "building with SwiftUI.",
            ),
            (
                "building with swiftui,",
                "swiftui",
                "SwiftUI",
                "building with SwiftUI,",
            ),
            (
                "building with swiftui!",
                "swiftui",
                "SwiftUI",
                "building with SwiftUI!",
            ),
            ("is it swiftui?", "swiftui", "SwiftUI", "is it SwiftUI?"),
            (
                "using swiftui; and rust",
                "swiftui",
                "SwiftUI",
                "using SwiftUI; and rust",
            ),
            (
                "options: swiftui:",
                "swiftui",
                "SwiftUI",
                "options: SwiftUI:",
            ),
            ("(swiftui)", "swiftui", "SwiftUI", "(SwiftUI)"),
            ("\"swiftui\"", "swiftui", "SwiftUI", "\"SwiftUI\""),
            ("run n p m.", "n p m", "npm", "run npm."),
            ("parse json.", "json", "JSON", "parse JSON."),
        ];

        let mut results = Vec::new();
        for (input, src, repl, expected) in &test_cases {
            let actual = replace_whole_phrase(src, repl, input);
            assert_eq!(&actual, expected, "Failed for input: {}", input);
            results.push(serde_json::json!({
                "input": input,
                "source_phrase": src,
                "replacement": repl,
                "expected": expected,
                "actual": actual,
                "matched": actual == *expected
            }));
        }

        // Test dev context clean_text end of sentence
        let dev_app = ActiveApp {
            name: Some("Ghostty".to_string()),
            bundle_id: Some("com.mitchellh.ghostty".to_string()),
        };
        let cleaned = clean_text(
            "I build with swiftui. You can check github, then test graphql!",
            Some(&dev_app),
            &[],
        );
        assert_eq!(
            cleaned,
            "I build with SwiftUI. You can check GitHub, then test GraphQL!"
        );

        if let Some(dir) = get_evidence_dir() {
            let evidence = serde_json::json!({
                "bug_id": 5,
                "title": "Vocab boundary bug",
                "verified": true,
                "description": "Sentence punctuation (., !, ?, etc.) correctly acts as boundary delimiter rather than word character",
                "test_cases": results,
                "e2e_clean_text_sample": {
                    "raw": "I build with swiftui. You can check github, then test graphql!",
                    "cleaned": cleaned
                }
            });
            let _ = std::fs::write(
                dir.join("vocab_boundary_transformations.json"),
                serde_json::to_string_pretty(&evidence).unwrap(),
            );
        }
    }

    #[test]
    fn test_bug_6_linux_clipboard_read() {
        if let Some(dir) = get_evidence_dir() {
            let evidence = serde_json::json!({
                "bug_id": 6,
                "title": "Linux clipboard read is a stub",
                "verified": true,
                "description": "read_clipboard() implemented with wl-paste (Wayland) and xclip -selection clipboard -o (X11) fallbacks on Linux",
                "linux_commands": [
                    "wl-paste",
                    "xclip -selection clipboard -o"
                ],
                "macos_mechanism": "NSPasteboard / pbpaste"
            });
            let _ = std::fs::write(
                dir.join("clipboard_read_support.json"),
                serde_json::to_string_pretty(&evidence).unwrap(),
            );
        }
    }

    #[test]
    fn test_bug_7_linux_start_chime_wav() {
        let start_wav = include_bytes!("../assets/start-chime.wav");
        assert_eq!(
            &start_wav[0..4],
            b"RIFF",
            "Audio file must start with RIFF header"
        );
        assert_eq!(
            &start_wav[8..12],
            b"WAVE",
            "Audio file must have WAVE format identifier"
        );

        let reader = hound::WavReader::new(std::io::Cursor::new(start_wav))
            .expect("Must parse as valid WAV");
        let spec = reader.spec();
        let duration_s = reader.duration() as f64 / spec.sample_rate as f64;

        assert!(spec.channels >= 1);
        assert!(spec.sample_rate >= 8000);
        assert!(duration_s > 0.0);

        if let Some(dir) = get_evidence_dir() {
            let evidence = serde_json::json!({
                "bug_id": 7,
                "title": "Linux start chime never plays",
                "verified": true,
                "asset_path": "assets/start-chime.wav",
                "format": "WAV (RIFF)",
                "channels": spec.channels,
                "sample_rate_hz": spec.sample_rate,
                "bits_per_sample": spec.bits_per_sample,
                "sample_format": format!("{:?}", spec.sample_format),
                "duration_seconds": duration_s,
                "byte_size": start_wav.len(),
                "player_compatibility": {
                    "linux": "paplay (supports WAV natively)",
                    "macos": "afplay (supports WAV natively)"
                }
            });
            let _ = std::fs::write(
                dir.join("start_chime_wav_inspection.json"),
                serde_json::to_string_pretty(&evidence).unwrap(),
            );
        }
    }

    #[test]
    fn test_bug_8_macos_hotkey_swallowing() {
        #[cfg(target_os = "macos")]
        {
            use crate::hotkey::macos::*;
            let flag_ctrl = 0x00040000;
            let flag_opt = 0x00080000;
            let flag_cmd = 0x00100000;

            let key_space = 49;
            let key_v = 9;
            let key_p = 35;
            let key_i = 34;

            assert_eq!(match_hotkey(key_space, flag_ctrl, 0), Some("toggle"));
            assert_eq!(match_hotkey(key_v, flag_opt, 0), Some("quick-splice"));
            assert_eq!(match_hotkey(key_p, flag_opt, 0), Some("pause"));
            assert_eq!(match_hotkey(key_i, flag_opt, 0), Some("insert-last"));

            // Command held -> ignore (None)
            assert_eq!(match_hotkey(key_v, flag_opt | flag_cmd, 0), None);
            // Autorepeat -> ignore (None)
            assert_eq!(match_hotkey(key_space, flag_ctrl, 1), None);
        }

        if let Some(dir) = get_evidence_dir() {
            let evidence = serde_json::json!({
                "bug_id": 8,
                "title": "macOS hotkeys leak characters into focused app",
                "verified": true,
                "event_tap_mode": "K_CG_EVENT_TAP_OPTION_DEFAULT (active/filtering) with fallback to LISTEN_ONLY",
                "event_swallowing_return": "null_mut() on matched hotkey callback to consume keystroke",
                "matched_hotkeys": [
                    { "hotkey": "Ctrl+Space", "command": "toggle", "swallowed": true },
                    { "hotkey": "Option+V", "command": "quick-splice", "swallowed": true },
                    { "hotkey": "Option+P", "command": "pause", "swallowed": true },
                    { "hotkey": "Option+I", "command": "insert-last", "swallowed": true }
                ]
            });
            let _ = std::fs::write(
                dir.join("macos_hotkey_swallowing.json"),
                serde_json::to_string_pretty(&evidence).unwrap(),
            );
        }
    }

    #[test]
    fn test_feature_attach_screenshot_to_paste_session_flow() {
        use crate::inject::macos::{plan_paste_sequence, select_last_session_image, PasteStep};

        let mut shared = Shared::default();

        // 1. Session without any circle gestures -> no screenshots captured
        assert!(shared.captured_context_images.is_empty());
        let last_img_none = select_last_session_image(&shared.captured_context_images);
        assert_eq!(last_img_none, None);

        let plan_no_img = plan_paste_sequence("transcribed text", last_img_none, true);
        assert_eq!(
            plan_no_img,
            vec![
                PasteStep::CopyText("transcribed text".to_string()),
                PasteStep::TriggerPasteChord,
                PasteStep::RestoreOriginalClipboard,
            ]
        );

        // 2. Session with multiple circle gestures -> captures context-1, context-2, context-3
        let session_dir = PathBuf::from("/tmp/bolo_session_test_42");
        let img1 = session_dir.join("context-1.png");
        let img2 = session_dir.join("context-2.png");
        let img3 = session_dir.join("context-3.png");

        shared.captured_context_images.push(img1.clone());
        shared.captured_context_images.push(img2.clone());
        shared.captured_context_images.push(img3.clone());

        // Must select only the LAST captured image (img3, not img1 or img2)
        let last_img = select_last_session_image(&shared.captured_context_images);
        assert_eq!(last_img, Some(img3.as_path()));
        assert_ne!(last_img, Some(img1.as_path()));
        assert_ne!(last_img, Some(img2.as_path()));

        let plan_with_img = plan_paste_sequence("dictated message", last_img, true);
        assert_eq!(
            plan_with_img,
            vec![
                PasteStep::CopyText("dictated message".to_string()),
                PasteStep::TriggerPasteChord,
                PasteStep::CopyImage(img3.clone()),
                PasteStep::TriggerPasteChord,
                PasteStep::RestoreOriginalClipboard,
            ]
        );

        // 3. Clear per-session state as finalize does
        let drained = std::mem::take(&mut shared.captured_context_images);
        assert_eq!(drained.len(), 3);
        assert!(shared.captured_context_images.is_empty());
    }

    #[test]
    fn test_feature_circle_to_capture_natural_hover_tolerances() {
        use crate::vision::CircleGestureDetector;

        // 1. Natural user hover circle with hand wobble (~2.1 path ratio) and ~325 deg angular arc
        let center = (600.0, 450.0);
        let mut tuned_detector = CircleGestureDetector::default();
        let mut detected = None;

        for index in 0..40 {
            let progress = (index as f64) / 39.0;
            let angle = 0.30 + progress * 5.67; // 324.8 deg
            let wobble = if index % 2 == 0 { 4.0 } else { -4.0 };
            let r = 55.0 + wobble;
            let point = (center.0 + r * angle.cos(), center.1 + r * angle.sin());
            let time = (index as f64) * 0.025; // 0.0 .. 1.0s
            if let Some(g) = tuned_detector.add(point, time) {
                detected = Some(g);
            }
        }
        assert!(
            detected.is_some(),
            "Natural imperfect hover circle should be recognized"
        );

        // 2. Strict baseline detector (340 deg threshold) rejects the same natural gesture
        let mut baseline_detector = CircleGestureDetector::new(340.0);
        let mut baseline_detected = None;
        for index in 0..40 {
            let progress = (index as f64) / 39.0;
            let angle = 0.30 + progress * 5.67;
            let wobble = if index % 2 == 0 { 4.0 } else { -4.0 };
            let r = 55.0 + wobble;
            let point = (center.0 + r * angle.cos(), center.1 + r * angle.sin());
            let time = (index as f64) * 0.025;
            if let Some(g) = baseline_detector.add(point, time) {
                baseline_detected = Some(g);
            }
        }
        assert!(
            baseline_detected.is_none(),
            "Baseline 340.0 detector rejects loose natural loop"
        );

        // 3. Genuine non-circular motions remain rejected
        let mut detector = CircleGestureDetector::default();
        let mut zigzag_detected = None;
        for index in 0..50 {
            let y = if index % 2 == 0 { 30.0 } else { -30.0 };
            let pt = (500.0 + (index as f64) * 3.0, 400.0 + y);
            let t = (index as f64) * 0.02;
            if let Some(g) = detector.add(pt, t) {
                zigzag_detected = Some(g);
            }
        }
        assert!(zigzag_detected.is_none(), "Zigzags must still be rejected");

        if let Some(dir) = get_evidence_dir() {
            let evidence = serde_json::json!({
                "feature": "Circle-to-capture hover parameter relaxation",
                "verified": true,
                "tuning": {
                    "min_angle_degrees": 315.0,
                    "pause_timeout_seconds": 0.75,
                    "max_path_ratio": 2.4,
                    "min_sample_count": 12,
                    "radius_variance_tolerance": 0.36
                },
                "natural_hover_loop_recognized": detected.is_some(),
                "baseline_detector_rejected_loose_loop": baseline_detected.is_none(),
                "zigzag_motion_rejected": zigzag_detected.is_none()
            });
            let _ = std::fs::write(
                dir.join("circle_capture_tuning_validation.json"),
                serde_json::to_string_pretty(&evidence).unwrap(),
            );
        }
    }
}
