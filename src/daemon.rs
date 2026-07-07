use crate::config::{Config, InjectMethod, PIPELINE_SAMPLE_RATE};
use crate::inject::{clipboard::ClipboardInjector, portal::PortalInjector, TextInjector};
use crate::stt::groq::{encode_wav, GroqStt};
use crate::stt::SttProvider;
use crate::vad::{self, Control, StopReason, Utterance};
use anyhow::{anyhow, Context};
use crossbeam_channel::Sender;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Recording,
    Paused,
    Processing,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Recording => "recording",
            Phase::Paused => "paused",
            Phase::Processing => "processing",
        }
    }
}

struct Shared {
    phase: Phase,
    /// Control channel into the active endpointer, while recording.
    control_tx: Option<Sender<Control>>,
    /// When the starting toggle arrived, for the toggle→capture metric.
    toggle_t0: Option<Instant>,
    /// Clipboard content at pause time; a change by resume time means the
    /// user copied something to splice into the transcript.
    clip_snapshot: Option<String>,
}

enum PipelineMsg {
    Segment(Utterance),
    Insert(String),
    Finalize,
}

/// One piece of the transcript being assembled: speech already sent to Groq
/// (transcribing in the background while the user is paused), or text the
/// user copied during a pause.
enum Piece {
    Spoken(tokio::task::JoinHandle<anyhow::Result<crate::stt::Transcript>>),
    Inserted(String),
}

pub fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("bolo.sock")
}

fn notify(cfg: &Config, body: &str) {
    if !cfg.daemon.notifications {
        return;
    }
    let _ = notify_rust::Notification::new()
        .summary("Bolo")
        .body(body)
        .timeout(notify_rust::Timeout::Milliseconds(2500))
        .show();
}

fn read_clipboard() -> Option<String> {
    let out = std::process::Command::new("wl-paste").arg("-n").output().ok()?;
    if !out.status.success() {
        return None; // empty clipboard exits non-zero
    }
    String::from_utf8(out.stdout).ok() // non-text (e.g. image) -> None
}

pub fn run(cfg: Config) -> anyhow::Result<()> {
    let path = socket_path();
    // Single instance: a live socket means another daemon owns it.
    if UnixStream::connect(&path).is_ok() {
        return Err(anyhow!("bolo daemon already running on {}", path.display()));
    }
    let _ = std::fs::remove_file(&path); // stale socket from a dead daemon
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("cannot bind {}", path.display()))?;
    eprintln!("[daemon] listening on {}", path.display());

    // Fail fast on a missing key, like the one-shot path.
    let stt = Arc::new(GroqStt::new(cfg.groq.clone())?);

    let shared = Arc::new(Mutex::new(Shared {
        phase: Phase::Idle,
        control_tx: None,
        toggle_t0: None,
        clip_snapshot: None,
    }));
    let (start_tx, start_rx) = crossbeam_channel::unbounded::<()>();
    let (pipeline_tx, pipeline_rx) = crossbeam_channel::unbounded::<PipelineMsg>();

    // Audio-owner thread: cpal Stream is !Send, so streams are created and
    // dropped here, one per segment.
    {
        let shared = Arc::clone(&shared);
        let pipeline_tx = pipeline_tx.clone();
        let vad_cfg = cfg.vad.clone();
        std::thread::spawn(move || {
            for () in start_rx.iter() {
                let (audio_tx, audio_rx) = crossbeam_channel::unbounded::<Vec<f32>>();
                let (control_tx, control_rx) = crossbeam_channel::unbounded::<Control>();
                let stream_info = crate::audio::start_capture(audio_tx);
                let (stream, info) = match stream_info {
                    Ok(x) => x,
                    Err(e) => {
                        eprintln!("[daemon] audio start failed: {e:#}");
                        let mut s = shared.lock().unwrap();
                        s.phase = Phase::Idle;
                        s.toggle_t0 = None;
                        continue;
                    }
                };
                {
                    let mut s = shared.lock().unwrap();
                    s.control_tx = Some(control_tx);
                    if let Some(t0) = s.toggle_t0.take() {
                        eprintln!(
                            "[daemon] toggle→capture_ms={} device=\"{}\" rate={}",
                            t0.elapsed().as_millis(),
                            info.device_name,
                            info.sample_rate
                        );
                    }
                }
                let result =
                    vad::run_endpointer(audio_rx, control_rx, &vad_cfg, info.sample_rate, true);
                drop(stream);
                {
                    let mut s = shared.lock().unwrap();
                    s.control_tx = None;
                    // A pause keeps the session open; the socket thread has
                    // already set phase=Paused. Anything else goes to the
                    // pipeline for finalization.
                    if !matches!(result, Ok(Utterance { reason: StopReason::Pause, .. })) {
                        s.phase = Phase::Processing;
                    }
                }
                match result {
                    Ok(utt) => {
                        if pipeline_tx.send(PipelineMsg::Segment(utt)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("[daemon] endpointer failed: {e:#}");
                        shared.lock().unwrap().phase = Phase::Idle;
                    }
                }
            }
        });
    }

    // Socket listener thread: tiny line protocol.
    {
        let shared = Arc::clone(&shared);
        let cfg = cfg.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                if let Err(e) = handle_client(conn, &shared, &start_tx, &pipeline_tx, &cfg) {
                    eprintln!("[daemon] client error: {e:#}");
                }
            }
        });
    }

    // Pipeline loop: assemble pieces per session; on finalize, await the
    // background transcriptions in order, join, and inject. Owns the tokio
    // runtime and the (stateful) injectors so the portal session survives
    // across utterances.
    let runtime = tokio::runtime::Runtime::new()?;
    let mut portal = PortalInjector::new(cfg.inject.type_delay_ms);
    let mut clipboard = ClipboardInjector;
    let mut pieces: Vec<Piece> = Vec::new();

    for msg in pipeline_rx.iter() {
        match msg {
            PipelineMsg::Segment(utt) => {
                eprintln!(
                    "[segment] n={} reason={} speech_ms={}",
                    pieces.len() + 1,
                    utt.reason.as_str(),
                    utt.speech_ms
                );
                if utt.speech_ms > 0 {
                    match encode_wav(&utt.samples_16k) {
                        Ok(wav) => {
                            eprintln!(
                                "[wav]     mono=true sample_rate={} bytes={} path=/tmp/bolo_capture.wav",
                                PIPELINE_SAMPLE_RATE,
                                wav.len()
                            );
                            let stt = Arc::clone(&stt);
                            pieces.push(Piece::Spoken(
                                runtime.spawn(async move { stt.transcribe(wav).await }),
                            ));
                        }
                        Err(e) => eprintln!("[daemon] wav encode failed: {e:#}"),
                    }
                }
                if utt.reason != StopReason::Pause {
                    finalize(&runtime, &mut pieces, &mut portal, &mut clipboard, &cfg, &shared);
                }
            }
            PipelineMsg::Insert(text) => {
                eprintln!("[insert]  chars={}", text.chars().count());
                pieces.push(Piece::Inserted(text));
            }
            PipelineMsg::Finalize => {
                finalize(&runtime, &mut pieces, &mut portal, &mut clipboard, &cfg, &shared);
            }
        }
    }
    Ok(())
}

fn finalize(
    runtime: &tokio::runtime::Runtime,
    pieces: &mut Vec<Piece>,
    portal: &mut PortalInjector,
    clipboard: &mut ClipboardInjector,
    cfg: &Config,
    shared: &Arc<Mutex<Shared>>,
) {
    let t_end = Instant::now();
    let n_pieces = pieces.len();
    if n_pieces > 0 {
        notify(cfg, "Transcribing…");
    }
    // notify-rust's blocking show() cannot run inside block_on (it spins up
    // its own runtime), so the async block only returns what to say.
    let outcome: anyhow::Result<Option<(&'static str, String)>> = runtime.block_on(async {
        let mut texts: Vec<String> = Vec::new();
        for piece in pieces.drain(..) {
            match piece {
                Piece::Spoken(handle) => {
                    let transcript = handle.await.context("transcription task panicked")??;
                    let text = transcript.text.trim().to_string();
                    if !text.is_empty() {
                        texts.push(text);
                    }
                }
                Piece::Inserted(text) => texts.push(text.trim().to_string()),
            }
        }
        if texts.is_empty() {
            return Ok(None);
        }
        let text = texts.join(" ");
        eprintln!("[assemble] pieces={} chars={}", n_pieces, text.chars().count());
        println!("[result]  {text}");

        let t_inject = Instant::now();
        let (used, result) = match cfg.inject.method {
            InjectMethod::Portal => match portal.inject(&text).await {
                Ok(()) => ("portal", Ok(())),
                Err(e) => {
                    eprintln!("[inject] portal failed ({e:#}); falling back to clipboard");
                    ("clipboard-fallback", clipboard.inject(&text).await)
                }
            },
            InjectMethod::Clipboard => ("clipboard", clipboard.inject(&text).await),
        };
        result?;
        eprintln!(
            "[inject]  method={} chars={} inject_ms={} finalize→done_ms={}",
            used,
            text.chars().count(),
            t_inject.elapsed().as_millis(),
            t_end.elapsed().as_millis()
        );
        Ok(Some((used, text)))
    });
    match outcome {
        Ok(None) => {
            eprintln!("[skip] no speech detected");
            notify(cfg, "No speech detected");
        }
        Ok(Some((used, text))) if used != "portal" => {
            notify(cfg, &format!("On clipboard — paste with Ctrl+V:\n{text}"));
        }
        Ok(Some((_, text))) => notify(cfg, &text),
        Err(e) => {
            eprintln!("[daemon] session failed: {e:#}");
            notify(cfg, &format!("Error: {e}"));
        }
    }
    let mut s = shared.lock().unwrap();
    s.phase = Phase::Idle;
    s.clip_snapshot = None;
}

fn handle_client(
    conn: UnixStream,
    shared: &Arc<Mutex<Shared>>,
    start_tx: &Sender<()>,
    pipeline_tx: &Sender<PipelineMsg>,
    cfg: &Config,
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut conn = conn;
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(()); // connection probe (e.g. single-instance check), no command
    }
    let reply = match line.trim() {
        "toggle" => {
            let mut s = shared.lock().unwrap();
            match s.phase {
                Phase::Idle => {
                    s.phase = Phase::Recording;
                    s.toggle_t0 = Some(Instant::now());
                    drop(s);
                    start_tx.send(()).context("audio thread gone")?;
                    notify(cfg, "Listening… (Ctrl+Space stop · Alt+P pause)");
                    "ok recording".to_string()
                }
                Phase::Recording => {
                    if let Some(tx) = s.control_tx.as_ref() {
                        let _ = tx.send(Control::ForceStop);
                    }
                    "ok stopping".to_string()
                }
                Phase::Paused => {
                    s.phase = Phase::Processing;
                    drop(s);
                    pipeline_tx.send(PipelineMsg::Finalize).context("pipeline gone")?;
                    "ok finishing".to_string()
                }
                Phase::Processing => "busy processing".to_string(),
            }
        }
        "pause" => {
            let mut s = shared.lock().unwrap();
            match s.phase {
                Phase::Recording => {
                    s.phase = Phase::Paused;
                    s.clip_snapshot = read_clipboard();
                    if let Some(tx) = s.control_tx.as_ref() {
                        let _ = tx.send(Control::Pause);
                    }
                    drop(s);
                    notify(cfg, "Paused — copy text to insert · Alt+P resume · Ctrl+Space finish");
                    "ok paused".to_string()
                }
                Phase::Paused => {
                    let snapshot = s.clip_snapshot.take();
                    if let Some(text) = read_clipboard() {
                        if !text.trim().is_empty() && snapshot.as_ref() != Some(&text) {
                            let n = text.chars().count();
                            pipeline_tx
                                .send(PipelineMsg::Insert(text))
                                .context("pipeline gone")?;
                            notify(cfg, &format!("Inserted {n} chars from clipboard"));
                        }
                    }
                    s.phase = Phase::Recording;
                    s.toggle_t0 = Some(Instant::now());
                    drop(s);
                    start_tx.send(()).context("audio thread gone")?;
                    notify(cfg, "Listening… (Ctrl+Space stop · Alt+P pause)");
                    "ok recording".to_string()
                }
                phase => format!("err not recording (phase: {})", phase.as_str()),
            }
        }
        "status" => shared.lock().unwrap().phase.as_str().to_string(),
        "quit" => {
            let _ = writeln!(conn, "ok bye");
            let _ = std::fs::remove_file(socket_path());
            eprintln!("[daemon] quit requested, exiting");
            std::process::exit(0);
        }
        other => format!("err unknown command {other:?}"),
    };
    writeln!(conn, "{reply}")?;
    Ok(())
}
