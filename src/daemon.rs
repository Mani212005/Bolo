use crate::config::{Config, SttBackend, PIPELINE_SAMPLE_RATE};
#[cfg(target_os = "linux")]
use crate::config::InjectMethod;
use crate::inject::TextInjector;
#[cfg(target_os = "linux")]
use crate::inject::{clipboard::ClipboardInjector, portal::PortalInjector};
#[cfg(target_os = "macos")]
use crate::inject::macos::MacOsTextInjector;
use crate::stt::groq::encode_wav;
use crate::vad::{self, Control, StopReason, Utterance};
use anyhow::{anyhow, Context};
use crossbeam_channel::Sender;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[cfg(target_os = "linux")]
struct Injectors {
    portal: PortalInjector,
    clipboard: ClipboardInjector,
}

#[cfg(target_os = "macos")]
struct Injectors {
    macos: MacOsTextInjector,
}

impl Injectors {
    #[cfg(target_os = "linux")]
    fn new(cfg: &Config) -> Self {
        Self {
            portal: PortalInjector::new(cfg.inject.type_delay_ms),
            clipboard: ClipboardInjector,
        }
    }

    #[cfg(target_os = "macos")]
    fn new(_cfg: &Config) -> Self {
        Self {
            macos: MacOsTextInjector::new(),
        }
    }

    async fn clipboard_inject(&mut self, text: &str) -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        return self.clipboard.inject(text).await;
        #[cfg(target_os = "macos")]
        {
            let text = text.to_owned();
            tokio::task::spawn_blocking(move || {
                let mut child = std::process::Command::new("pbcopy")
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()?;
                if let Some(mut stdin) = child.stdin.take() {
                    use std::io::Write;
                    stdin.write_all(text.as_bytes())?;
                }
                child.wait()?;
                anyhow::Ok(())
            }).await??;
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Idle,
    Recording,
    Paused,
    Processing,
}

impl Phase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Recording => "recording",
            Phase::Paused => "paused",
            Phase::Processing => "processing",
        }
    }
}

pub(crate) struct Shared {
    pub(crate) phase: Phase,
    /// Control channel into the active endpointer, while recording.
    pub(crate) control_tx: Option<Sender<Control>>,
    /// When the starting toggle arrived, for the toggle→capture metric.
    pub(crate) toggle_t0: Option<Instant>,
    /// Clipboard content at pause time; a change by resume time means the
    /// user copied something to splice into the transcript.
    clip_snapshot: Option<String>,
    /// Most recent finished transcript (or enhanced text); what Alt+I types.
    pub(crate) last_text: Option<String>,
}

pub(crate) enum PipelineMsg {
    Segment(Utterance),
    Insert(String),
    Finalize,
    /// Re-type previously finished text at the current cursor (Alt+I).
    InsertLast(String),
    /// Rewrite the last transcript as a better LLM prompt (Enhance).
    Enhance(String),
}

/// One piece of the transcript being assembled: speech already sent to Groq
/// (transcribing in the background while the user is paused), or text the
/// user copied during a pause.
enum Piece {
    Spoken {
        handle: tokio::task::JoinHandle<anyhow::Result<crate::stt::Transcript>>,
        audio_id: String,
        duration_s: f64,
    },
    Inserted(String),
}

pub fn socket_path() -> PathBuf {
    let dir = crate::userdata::config_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir.join("bolo.sock")
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

/// Result notification with an Enhance action button. GNOME shows the button
/// on the banner; clicking it feeds "enhance" back through our own socket so
/// all state transitions stay on the socket thread. Falls back to a plain
/// notification body if the server ignores actions.
fn notify_result(cfg: &Config, body: &str) {
    if !cfg.daemon.notifications {
        return;
    }
    let notification = notify_rust::Notification::new()
        .summary("Bolo")
        .body(body)
        .action("enhance", "Enhance")
        .timeout(notify_rust::Timeout::Milliseconds(15000))
        .finalize();
    // wait_for_action blocks until click/close/timeout — needs its own thread.
    std::thread::spawn(move || match notification.show() {
        Ok(handle) => handle.wait_for_action(|action| {
            if action == "enhance" {
                if let Ok(mut c) = UnixStream::connect(socket_path()) {
                    let _ = writeln!(c, "enhance");
                    // Read the reply so the daemon's write doesn't hit a
                    // closed pipe (was logging "client error: Broken pipe").
                    let mut reply = String::new();
                    let _ = BufReader::new(c).read_line(&mut reply);
                }
            }
        }),
        Err(e) => eprintln!("[notify] failed: {e}"),
    });
}

fn read_clipboard() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("pbpaste").output().ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).to_string();
        if s.trim().is_empty() {
            None
        } else {
            Some(s)
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

fn copy_selection() {
    #[cfg(target_os = "macos")]
    {
        let applescript = r#"
            tell application "System Events"
                keystroke "c" using command down
            end tell
        "#;
        let _ = std::process::Command::new("osascript")
            .arg("-e")
            .arg(applescript)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
}

fn apply_voice_clipboard_triggers(text: &str) -> String {
    let Ok(re) = regex::Regex::new(r"(?i)\b(paste|insert)\s+(?:the\s+)?(?:clipboard|link|url)\b[.,]?") else {
        return text.to_string();
    };
    if re.is_match(text) {
        if let Some(clip) = read_clipboard() {
            let trimmed = clip.trim();
            if !trimmed.is_empty() {
                return re.replace_all(text, trimmed).to_string();
            }
        }
    }
    text.to_string()
}

pub fn run(cfg: Config, config_path: std::path::PathBuf) -> anyhow::Result<()> {
    let path = socket_path();
    // Single instance: if an old daemon is alive, send it quit or take over cleanly
    if let Ok(mut stream) = UnixStream::connect(&path) {
        use std::io::Write;
        let _ = writeln!(stream, "quit");
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    let _ = std::fs::remove_file(&path); // stale socket from a dead daemon
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("cannot bind {}", path.display()))?;
    eprintln!("[daemon] listening on {}", path.display());

    // Fail fast: missing GROQ_API_KEY (groq) or a first-time model download
    // (whisper) both surface here, before any recording.
    if cfg.stt.provider == SttBackend::Whisper
        && !crate::stt::whisper::model_path(&cfg.stt.whisper.model).exists()
    {
        notify(
            &cfg,
            &format!("Downloading whisper model {} (one-time)…", cfg.stt.whisper.model),
        );
    }
    let stt = crate::stt::make_provider(&cfg)?;

    let shared = Arc::new(Mutex::new(Shared {
        phase: Phase::Idle,
        control_tx: None,
        toggle_t0: None,
        clip_snapshot: None,
        last_text: None,
    }));
    let (start_tx, start_rx) = crossbeam_channel::unbounded::<()>();
    let (pipeline_tx, pipeline_rx) = crossbeam_channel::unbounded::<PipelineMsg>();

    // Audio-owner thread: cpal Stream is !Send, so streams are created and
    // dropped here, one per session/segment.
    {
        let shared = Arc::clone(&shared);
        let pipeline_tx = pipeline_tx.clone();
        let vad_cfg = cfg.vad.clone();
        let cfg_audio = cfg.clone();
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
                    if let Some(t0) = s.toggle_t0 {
                        eprintln!(
                            "[daemon] toggle→capture_ms={} device=\"{}\" rate={}",
                            t0.elapsed().as_millis(),
                            info.device_name,
                            info.sample_rate
                        );
                    }
                }
                crate::sound::play(&cfg_audio, crate::sound::Chime::Start);

                loop {
                    let result = vad::run_endpointer(
                        audio_rx.clone(),
                        control_rx.clone(),
                        &vad_cfg,
                        info.sample_rate,
                        vad_cfg.auto_endpoint,
                    );

                    match result {
                        Ok(utt) => {
                            if let StopReason::Splice(ref clip_text) = utt.reason {
                                let text_to_insert = clip_text.clone();
                                // 1. Spoken audio before splice point is sent for background transcription
                                if pipeline_tx.send(PipelineMsg::Segment(utt)).is_err() {
                                    break;
                                }
                                // 2. Insert the clipboard text right after the preceding spoken audio
                                if !text_to_insert.trim().is_empty() {
                                    if pipeline_tx.send(PipelineMsg::Insert(text_to_insert)).is_err() {
                                        break;
                                    }
                                }
                                // 3. Seamlessly continue audio capture without dropping the stream!
                                continue;
                            }

                            drop(stream);
                            if utt.reason != StopReason::Pause {
                                crate::sound::play(&cfg_audio, crate::sound::Chime::Stop);
                            }
                            {
                                let mut s = shared.lock().unwrap();
                                s.control_tx = None;
                                s.toggle_t0 = None;
                                if utt.reason != StopReason::Pause {
                                    s.phase = Phase::Processing;
                                }
                            }
                            let _ = pipeline_tx.send(PipelineMsg::Segment(utt));
                            break;
                        }
                        Err(e) => {
                            eprintln!("[daemon] endpointer failed: {e:#}");
                            drop(stream);
                            crate::sound::play(&cfg_audio, crate::sound::Chime::Stop);
                            shared.lock().unwrap().phase = Phase::Idle;
                            break;
                        }
                    }
                }
            }
        });
    }

    // Socket listener thread: tiny line protocol.
    {
        let shared = Arc::clone(&shared);
        let cfg = cfg.clone();
        let start_tx = start_tx.clone();
        let pipeline_tx = pipeline_tx.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                if let Err(e) = handle_client(conn, &shared, &start_tx, &pipeline_tx, &cfg) {
                    // A client hanging up before reading its reply is routine
                    // (hotkey scripts, probes) — not worth an error line.
                    match e.downcast_ref::<std::io::Error>() {
                        Some(io) if io.kind() == std::io::ErrorKind::BrokenPipe => {}
                        _ => eprintln!("[daemon] client error: {e:#}"),
                    }
                }
            }
        });
    }

    // Hotkey listener: on macOS, this intercepts keystrokes via rdev.
    // On Linux, it's a no-op (hotkeys are handled by GNOME settings).
    {
        let listener = crate::hotkey::get_listener();
        let path = path.clone();
        if let Err(e) = listener.start(Box::new(move |cmd| {
            if let Ok(mut conn) = std::os::unix::net::UnixStream::connect(&path) {
                let _ = writeln!(conn, "{}", cmd);
                // read response if necessary, but we don't care
            }
        })) {
            eprintln!("[daemon] hotkey listener failed: {e}");
        }
    }

    // Settings & History dashboard: local web UI served from inside the daemon.
    {
        let shared = Arc::clone(&shared);
        let cfg = cfg.clone();
        let start_tx = start_tx.clone();
        let pipeline_tx = pipeline_tx.clone();
        let stt = Arc::clone(&stt);
        std::thread::spawn(move || crate::web::serve(config_path, shared, cfg, start_tx, pipeline_tx, stt));
    }

    // Pipeline loop: assemble pieces per session; on finalize, await the
    // background transcriptions in order, join, and inject. Owns the tokio
    // runtime and the (stateful) injectors so the portal session survives
    // across utterances.
    let runtime = tokio::runtime::Runtime::new()?;
    let mut injectors = Injectors::new(&cfg);
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
                    let duration_s = utt.speech_ms as f64 / 1000.0;
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    let audio_id = format!("rec_{now_ms}");
                    match encode_wav(&utt.samples_16k) {
                        Ok(wav) => {
                            let _ = crate::userdata::save_recording_wav(&audio_id, &wav);
                            eprintln!(
                                "[wav]     mono=true sample_rate={} bytes={} audio_id={}",
                                PIPELINE_SAMPLE_RATE,
                                wav.len(),
                                audio_id
                            );
                            let stt = Arc::clone(&stt);
                            pieces.push(Piece::Spoken {
                                handle: runtime.spawn(async move { stt.transcribe(wav).await }),
                                audio_id,
                                duration_s,
                            });
                        }
                        Err(e) => eprintln!("[daemon] wav encode failed: {e:#}"),
                    }
                }
                if utt.reason == StopReason::MaxCap {
                    notify(
                        &cfg,
                        &format!("Max length ({}s) reached — transcribing", cfg.vad.max_utterance_ms / 1000),
                    );
                }
                if utt.reason != StopReason::Pause && !matches!(utt.reason, StopReason::Splice(_)) {
                    finalize(&runtime, &mut pieces, &mut injectors, &cfg, &shared);
                }
            }
            PipelineMsg::Insert(text) => {
                eprintln!("[insert]  chars={}", text.chars().count());
                pieces.push(Piece::Inserted(text));
            }
            PipelineMsg::Finalize => {
                finalize(&runtime, &mut pieces, &mut injectors, &cfg, &shared);
            }
            PipelineMsg::InsertLast(text) => {
                let outcome = runtime.block_on(inject_text(&text, &mut injectors, &cfg));
                match outcome {
                    Ok(used) => {
                        eprintln!("[insert-last] method={} chars={}", used, text.chars().count());
                        if used != "portal" && used != "paste" {
                            notify(&cfg, "On clipboard — paste with Ctrl+V");
                        }
                    }
                    Err(e) => {
                        eprintln!("[insert-last] failed: {e:#}");
                        notify(&cfg, &format!("Insert failed: {e}"));
                    }
                }
            }
            PipelineMsg::Enhance(text) => {
                notify(&cfg, "Enhancing…");
                let outcome = runtime.block_on(async {
                    let enhanced = crate::enhance::enhance(&cfg.enhance, &text).await?;
                    injectors.clipboard_inject(&enhanced).await?;
                    anyhow::Ok(enhanced)
                });
                match outcome {
                    Ok(enhanced) => {
                        println!("[enhanced] {enhanced}");
                        crate::userdata::append_history("enhanced", &enhanced, None, None);
                        shared.lock().unwrap().last_text = Some(enhanced);
                        notify(&cfg, "Enhanced & copied — Alt+I types it at your cursor, Cmd+V (Mac) or Ctrl+V pastes");
                    }
                    Err(e) => {
                        eprintln!("[enhance] failed: {e:#}");
                        notify(&cfg, &format!("Enhance failed: {e}"));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Inject `text` by the configured method (portal falls back to clipboard).
/// Returns the method actually used.
async fn inject_text(
    text: &str,
    injectors: &mut Injectors,
    #[allow(unused)] cfg: &Config,
) -> anyhow::Result<&'static str> {
    #[cfg(target_os = "macos")]
    {
        injectors.macos.inject(text).await?;
        return Ok("macos");
    }

    #[cfg(target_os = "linux")]
    match cfg.inject.method {
        InjectMethod::Paste => {
            // Copy first; even if the chord fails the text is one Ctrl+V away.
            injectors.clipboard.inject(text).await?;
            match injectors.portal.paste_chord().await {
                Ok(()) => Ok("paste"),
                Err(e) => {
                    eprintln!("[inject] paste chord failed ({e:#}); text is on the clipboard");
                    Ok("clipboard")
                }
            }
        }
        InjectMethod::Portal => match injectors.portal.inject(text).await {
            Ok(()) => Ok("portal"),
            Err(e) => {
                eprintln!("[inject] portal failed ({e:#}); falling back to clipboard");
                injectors.clipboard.inject(text).await?;
                Ok("clipboard-fallback")
            }
        },
        InjectMethod::Clipboard => {
            injectors.clipboard.inject(text).await?;
            Ok("clipboard")
        }
    }
}

fn finalize(
    runtime: &tokio::runtime::Runtime,
    pieces: &mut Vec<Piece>,
    injectors: &mut Injectors,
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
    let outcome: anyhow::Result<Option<(&'static str, String, Option<String>, f64)>> = runtime.block_on(async {
        let mut texts: Vec<String> = Vec::new();
        let mut last_audio_id: Option<String> = None;
        let mut total_duration_s = 0.0;
        for piece in pieces.drain(..) {
            match piece {
                Piece::Spoken { handle, audio_id, duration_s } => {
                    let transcript = handle.await.context("transcription task panicked")??;
                    let text = transcript.text.trim().to_string();
                    if !text.is_empty() {
                        let enriched = apply_voice_clipboard_triggers(&text);
                        texts.push(enriched);
                        last_audio_id = Some(audio_id);
                        total_duration_s += duration_s;
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
        let used = inject_text(&text, injectors, cfg).await?;
        // Safety net: the transcript is always on the clipboard too, so a
        // missed portal paste never means digging through daemon logs. The
        // text was already typed, so a copy failure is non-fatal.
        if used == "portal" {
            match injectors.clipboard_inject(&text).await {
                Ok(()) => eprintln!("[clipboard] copied chars={}", text.chars().count()),
                Err(e) => eprintln!("[clipboard] copy failed (text was typed): {e:#}"),
            }
        }
        eprintln!(
            "[inject]  method={} chars={} inject_ms={} finalize→done_ms={}",
            used,
            text.chars().count(),
            t_inject.elapsed().as_millis(),
            t_end.elapsed().as_millis()
        );
        Ok(Some((used, text, last_audio_id, total_duration_s)))
    });
    match outcome {
        Ok(None) => {
            eprintln!("[skip] no speech detected");
            notify(cfg, "No speech detected");
        }
        Ok(Some((used, text, audio_id, duration_s))) => {
            let head = match used {
                "paste" => "Pasted + on clipboard",
                "portal" => "Typed + copied — Ctrl+V pastes it elsewhere",
                _ => "On clipboard — paste with Ctrl+V",
            };
            notify_result(cfg, &format!("{head}\n{text}"));
            crate::userdata::append_history("dictation", &text, audio_id.as_deref(), Some(duration_s));
            shared.lock().unwrap().last_text = Some(text);
        }
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
                    notify(cfg, "Listening… (Ctrl+Space stop · Opt+V paste · Opt+P pause)");
                    "ok recording".to_string()
                }
                Phase::Recording => {
                    // Debounce: ignore accidental rapid double-tap within 800ms of start
                    if let Some(t0) = s.toggle_t0 {
                        if t0.elapsed().as_millis() < 800 {
                            return Ok(());
                        }
                    }
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
                    notify(cfg, "Paused — Alt+I insert clipboard · Alt+P resume · Ctrl+Space finish");
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
                    notify(cfg, "Listening… (Ctrl+Space stop · Opt+V paste · Opt+P pause)");
                    "ok recording".to_string()
                }
                phase => format!("err not recording (phase: {})", phase.as_str()),
            }
        }
        "insert-last" => {
            let mut s = shared.lock().unwrap();
            match (s.phase, s.last_text.clone()) {
                (Phase::Idle, Some(text)) => {
                    drop(s);
                    pipeline_tx.send(PipelineMsg::InsertLast(text)).context("pipeline gone")?;
                    "ok inserting".to_string()
                }
                (Phase::Idle, None) => "err nothing to insert yet".to_string(),
                // While paused, Alt+I means "splice the current clipboard
                // into the transcript" — no matter when it was copied (covers
                // content copied before the dictation even started).
                (Phase::Paused, _) => match read_clipboard() {
                    Some(text) if !text.trim().is_empty() => {
                        let n = text.chars().count();
                        // Remember it so the Alt+P resume's changed-clipboard
                        // check doesn't insert the same text twice.
                        s.clip_snapshot = Some(text.clone());
                        drop(s);
                        pipeline_tx.send(PipelineMsg::Insert(text)).context("pipeline gone")?;
                        notify(cfg, &format!("Inserted {n} chars from clipboard"));
                        "ok inserted".to_string()
                    }
                    _ => "err clipboard empty".to_string(),
                },
                (phase, _) => format!("busy {}", phase.as_str()),
            }
        }
        "quick-splice" => {
            let s = shared.lock().unwrap();
            match s.phase {
                Phase::Recording => {
                    if let Some(text) = read_clipboard() {
                        let n = text.chars().count();
                        if let Some(tx) = s.control_tx.as_ref() {
                            let _ = tx.send(Control::CutSegment(text));
                        }
                        notify(cfg, &format!("Spliced {n} chars from clipboard"));
                        "ok spliced".to_string()
                    } else {
                        "err clipboard empty".to_string()
                    }
                }
                Phase::Paused => {
                    if let Some(text) = read_clipboard() {
                        let n = text.chars().count();
                        drop(s);
                        pipeline_tx.send(PipelineMsg::Insert(text)).context("pipeline gone")?;
                        notify(cfg, &format!("Inserted {n} chars from clipboard"));
                        "ok inserted".to_string()
                    } else {
                        "err clipboard empty".to_string()
                    }
                }
                phase => format!("err not recording (phase: {})", phase.as_str()),
            }
        }
        "copy-splice" => {
            let s = shared.lock().unwrap();
            match s.phase {
                Phase::Recording => {
                    copy_selection();
                    if let Some(text) = read_clipboard() {
                        let n = text.chars().count();
                        if let Some(tx) = s.control_tx.as_ref() {
                            let _ = tx.send(Control::CutSegment(text));
                        }
                        notify(cfg, &format!("Copied & spliced {n} chars"));
                        "ok copied and spliced".to_string()
                    } else {
                        "err clipboard empty".to_string()
                    }
                }
                phase => format!("err not recording (phase: {})", phase.as_str()),
            }
        }
        "enhance" => {
            let s = shared.lock().unwrap();
            match (s.phase, s.last_text.clone()) {
                (Phase::Idle, Some(text)) => {
                    drop(s);
                    pipeline_tx.send(PipelineMsg::Enhance(text)).context("pipeline gone")?;
                    "ok enhancing".to_string()
                }
                (Phase::Idle, None) => "err nothing to enhance yet".to_string(),
                (phase, _) => format!("busy {}", phase.as_str()),
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
