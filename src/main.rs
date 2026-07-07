mod audio;
mod config;
mod daemon;
mod inject;
mod resample;
mod stt;
mod vad;

use crate::config::{Config, PIPELINE_SAMPLE_RATE};
use crate::stt::groq::{encode_wav, GroqStt, CAPTURE_DUMP_PATH};
use crate::stt::SttProvider;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

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
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    match args.get(1).map(String::as_str) {
        Some("daemon") => {
            let cfg = Config::load(&config_path)?;
            return daemon::run(cfg);
        }
        Some(cmd @ ("toggle" | "pause" | "status" | "quit")) => return client(cmd),
        _ => {} // one-shot mode below
    }
    let cfg = Config::load(&config_path)?;

    // Fail fast on a missing API key before recording anything.
    let stt = GroqStt::new(cfg.groq.clone())?;

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

    eprintln!("[groq-raw] {}", transcript.raw_json);
    println!("[result]  {}", transcript.text);
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
