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
mod web;

use crate::config::{Config, PIPELINE_SAMPLE_RATE};
use crate::stt::groq::{encode_wav, CAPTURE_DUMP_PATH};
use anyhow::Context;
use std::io::{BufRead, BufReader, Write};
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
        Some(cmd @ ("toggle" | "pause" | "insert-last" | "enhance" | "status" | "quit")) => {
            return client(cmd)
        }
        Some("settings" | "ui") => {
            let cfg = Config::load(&config_path)?;
            return open_settings_app(cfg.ui.port);
        }
        Some("transcribe") => {
            // bolo transcribe <file.wav> — run the configured STT provider on
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
            // bolo model download [name]  — pre-fetch a local whisper model.
            let cfg = Config::load(&config_path)?;
            let name = match (args.get(2).map(String::as_str), args.get(3)) {
                (Some("download"), name) => {
                    name.cloned().unwrap_or_else(|| cfg.stt.whisper.model.clone())
                }
                _ => anyhow::bail!("usage: bolo model download [name]"),
            };
            let path = stt::whisper::ensure_model_blocking(&name)?;
            println!("[model] ready: {}", path.display());
            return Ok(());
        }
        _ => {} // one-shot mode below
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
    let daemon_up = std::os::unix::net::UnixStream::connect(daemon::socket_path()).is_ok();
    if !daemon_up {
        let started = std::process::Command::new("systemctl")
            .args(["--user", "start", "bolo.service"])
            .status()
            .is_ok_and(|s| s.success());
        if !started {
            let exe = std::env::current_exe()?;
            std::process::Command::new(exe).arg("daemon").spawn()?;
        }
        // Give it a moment to bind the web port.
        for _ in 0..20 {
            if std::os::unix::net::UnixStream::connect(daemon::socket_path()).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
    }
    let url = format!("http://127.0.0.1:{port}");
    for browser in ["google-chrome", "chromium", "chromium-browser", "brave-browser"] {
        if std::process::Command::new(browser)
            .arg(format!("--app={url}"))
            .spawn()
            .is_ok()
        {
            return Ok(());
        }
    }
    std::process::Command::new("xdg-open").arg(&url).spawn().context("no browser found")?;
    Ok(())
}

/// Send one command to the running daemon and print its reply.
fn client(cmd: &str) -> anyhow::Result<()> {
    let path = daemon::socket_path();
    let mut conn = std::os::unix::net::UnixStream::connect(&path).map_err(|e| {
        anyhow::anyhow!("no bolo daemon on {} ({e}); start one with `bolo daemon`", path.display())
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
