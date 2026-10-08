#[cfg(target_os = "linux")]
use crate::config::InjectMethod;
use crate::config::{Config, SttBackend, PIPELINE_SAMPLE_RATE};
use crate::events::{Event, EventHub};
#[cfg(target_os = "macos")]
use crate::inject::macos::MacOsTextInjector;
#[cfg(target_os = "linux")]
use crate::inject::TextInjector;
#[cfg(target_os = "linux")]
use crate::inject::{clipboard::ClipboardInjector, portal::PortalInjector};
use crate::stt::groq::encode_wav;
use crate::vad::{self, Control, StopReason, Utterance};
use anyhow::Context;
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
            clipboard: ClipboardInjector::new(&cfg.inject),
        }
    }

    #[cfg(target_os = "macos")]
    fn new(cfg: &Config) -> Self {
        Self {
            macos: MacOsTextInjector::new(&cfg.inject),
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
            })
            .await??;
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Phase {
    #[default]
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

#[derive(Default)]
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
    /// Vision circle gesture detector active for current session
    pub(crate) vision_detector: Option<crate::vision::CircleGestureDetector>,
    /// Directory of current recording session for saving context bundle
    pub(crate) vision_session_dir: Option<PathBuf>,
    /// Chronological context images captured during session
    pub(crate) captured_context_images: Vec<PathBuf>,
    /// Broadcasts phase changes, levels and outcomes to subscribers.
    pub(crate) events: EventHub,
}

impl Shared {
    /// The only way to change the phase: every change is published, so the
    /// event stream (and the pill drawn from it) never misses a transition.
    pub(crate) fn set_phase(&mut self, phase: Phase) {
        if self.phase != phase {
            self.phase = phase;
            self.events.publish(Event::Phase(phase));
        }
    }
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

fn outcome_event(kind: &'static str, detail: &'static str, chars: Option<usize>) -> Event {
    Event::Outcome {
        kind,
        detail,
        chars,
    }
}

/// Ends a session that never reached the pipeline (no mic, endpointer error):
/// report why, then go idle.
fn end_session(shared: &Arc<Mutex<Shared>>, closing: Event) {
    let mut s = shared.lock().unwrap();
    s.events.publish(closing);
    s.set_phase(Phase::Idle);
    s.toggle_t0 = None;
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
    // wait_for_action blocks until click/close/timeout - needs its own thread.
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
    #[cfg(target_os = "linux")]
    {
        if let Ok(out) = std::process::Command::new("wl-paste").output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).to_string();
                if !s.trim().is_empty() {
                    return Some(s);
                }
            }
        }
        if let Ok(out) = std::process::Command::new("xclip")
            .args(&["-selection", "clipboard", "-o"])
            .output()
        {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).to_string();
                if !s.trim().is_empty() {
                    return Some(s);
                }
            }
        }
        None
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
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
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdotool")
            .args(&["key", "--clearmodifiers", "ctrl+c"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        std::thread::sleep(std::time::Duration::from_millis(60));
    }
}

fn split_voice_clipboard_triggers(text: &str) -> Vec<crate::vocab::TranscriptPiece> {
    split_voice_clipboard_triggers_with(text, read_clipboard().as_deref())
}

/// Splits speech at voice clipboard trigger phrases ("paste clipboard",
/// "insert the link"), inserting the clipboard contents as a pasted piece so
/// it is formatted exactly like a hotkey splice.
pub fn split_voice_clipboard_triggers_with(
    text: &str,
    clip: Option<&str>,
) -> Vec<crate::vocab::TranscriptPiece> {
    use crate::vocab::TranscriptPiece;
    static TRIGGER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\s*\b(paste|insert)\s+(?:the\s+)?(?:clipboard|link|url)\b[.,]?\s*")
            .unwrap()
    });
    let clip = clip.map(str::trim).filter(|c| !c.is_empty());
    let Some(clip) = clip.filter(|_| TRIGGER.is_match(text)) else {
        return vec![TranscriptPiece::Spoken(text.to_string())];
    };
    let mut pieces = Vec::new();
    let mut last = 0;
    for m in TRIGGER.find_iter(text) {
        let before = text[last..m.start()].trim();
        if !before.is_empty() {
            pieces.push(TranscriptPiece::Spoken(before.to_string()));
        }
        pieces.push(TranscriptPiece::Inserted(clip.to_string()));
        last = m.end();
    }
    let after = text[last..].trim();
    if !after.is_empty() {
        pieces.push(TranscriptPiece::Spoken(after.to_string()));
    }
    pieces
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
    let listener =
        UnixListener::bind(&path).with_context(|| format!("cannot bind {}", path.display()))?;
    eprintln!("[daemon] listening on {}", path.display());

    // Fail fast: missing GROQ_API_KEY (groq) or a first-time model download
    // (whisper) both surface here, before any recording.
    if cfg.stt.provider == SttBackend::Whisper
        && !crate::stt::whisper::model_path(&cfg.stt.whisper.model).exists()
    {
        notify(
            &cfg,
            &format!(
                "Downloading whisper model {} (one-time)…",
                cfg.stt.whisper.model
            ),
        );
    }
    let stt = crate::stt::make_provider(&cfg)?;

    let events = EventHub::spawn(cfg.pill.clone());
    let shared = Arc::new(Mutex::new(Shared {
        phase: Phase::Idle,
        control_tx: None,
        toggle_t0: None,
        clip_snapshot: None,
        last_text: None,
        vision_detector: None,
        vision_session_dir: None,
        captured_context_images: Vec::new(),
        events: events.clone(),
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
        let events = events.clone();
        std::thread::spawn(move || {
            for () in start_rx.iter() {
                let (audio_tx, audio_rx) = crossbeam_channel::unbounded::<Vec<f32>>();
                let (control_tx, control_rx) = crossbeam_channel::unbounded::<Control>();
                let stream_info = crate::audio::start_capture(audio_tx);
                let (stream, info) = match stream_info {
                    Ok(x) => x,
                    Err(e) => {
                        eprintln!("[daemon] audio start failed: {e:#}");
                        end_session(&shared, outcome_event("mic-unavailable", "", None));
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
                        &|rms, speech| events.publish_level(rms, speech),
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
                                if !text_to_insert.trim().is_empty()
                                    && pipeline_tx
                                        .send(PipelineMsg::Insert(text_to_insert))
                                        .is_err()
                                {
                                    break;
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
                                    s.set_phase(Phase::Processing);
                                }
                            }
                            let _ = pipeline_tx.send(PipelineMsg::Segment(utt));
                            break;
                        }
                        Err(e) => {
                            eprintln!("[daemon] endpointer failed: {e:#}");
                            drop(stream);
                            crate::sound::play(&cfg_audio, crate::sound::Chime::Stop);
                            end_session(&shared, outcome_event("error", "", None));
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
                    // (hotkey scripts, probes) - not worth an error line.
                    match e.downcast_ref::<std::io::Error>() {
                        Some(io) if io.kind() == std::io::ErrorKind::BrokenPipe => {}
                        _ => eprintln!("[daemon] client error: {e:#}"),
                    }
                }
            }
        });
    }

    // The pill helper subscribes to the socket above, so start it after the listener.
    crate::pill::spawn_supervisor(&cfg.pill);

    // Hotkey listener: on macOS, this intercepts keystrokes via CGEventTap.
    // On Linux, it's a no-op (hotkeys are handled by GNOME settings).
    // A test daemon (scripts/pill-e2e) sets BOLO_NO_HOTKEYS so it never reacts
    // to the real keyboard alongside the user's own daemon.
    if std::env::var_os("BOLO_NO_HOTKEYS").is_some() {
        eprintln!("[daemon] global hotkeys disabled by BOLO_NO_HOTKEYS");
    } else {
        let listener = crate::hotkey::get_listener();
        let path = path.clone();
        let shared_hotkey = Arc::clone(&shared);
        let vision_enabled = cfg.vision.enabled;
        if let Err(e) = listener.start(Box::new(move |cmd| {
            if cmd.starts_with("mouse ") {
                if !vision_enabled {
                    return;
                }
                let is_recording = match shared_hotkey.try_lock() {
                    Ok(s) => s.phase == Phase::Recording,
                    Err(_) => true,
                };
                if !is_recording {
                    return;
                }
            }
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
        std::thread::spawn(move || {
            crate::web::serve(config_path, shared, cfg, start_tx, pipeline_tx, stt)
        });
    }

    // Pipeline loop: assemble pieces per session; on finalize, await the
    // background transcriptions in order, join, and inject. Owns the tokio
    // runtime and the (stateful) injectors so the portal session survives
    // across utterances.
    let runtime = tokio::runtime::Runtime::new()?;
    let mut injectors = Injectors::new(&cfg);
    let mut pieces: Vec<Piece> = Vec::new();
    // The session hit the max-length cap; reported with its outcome.
    let mut capped = false;

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
                    capped = true;
                    notify(
                        &cfg,
                        &format!(
                            "Max length ({}s) reached - transcribing",
                            cfg.vad.max_utterance_ms / 1000
                        ),
                    );
                }
                if utt.reason != StopReason::Pause && !matches!(utt.reason, StopReason::Splice(_)) {
                    finalize(
                        &runtime,
                        &mut pieces,
                        &mut injectors,
                        &cfg,
                        &shared,
                        std::mem::take(&mut capped),
                    );
                }
            }
            PipelineMsg::Insert(text) => {
                eprintln!("[insert]  chars={}", text.chars().count());
                pieces.push(Piece::Inserted(text));
            }
            PipelineMsg::Finalize => {
                finalize(
                    &runtime,
                    &mut pieces,
                    &mut injectors,
                    &cfg,
                    &shared,
                    std::mem::take(&mut capped),
                );
            }
            PipelineMsg::InsertLast(text) => {
                let outcome = runtime.block_on(inject_text(&text, &mut injectors, &cfg, &[]));
                match outcome {
                    Ok(injected) => {
                        let used = injected.method;
                        eprintln!(
                            "[insert-last] method={} chars={}",
                            used,
                            text.chars().count()
                        );
                        if used != "portal" && used != "paste" {
                            notify(&cfg, "On clipboard - paste with Ctrl+V");
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
                        crate::userdata::append_history("enhanced", &enhanced, None, None, None);
                        shared.lock().unwrap().last_text = Some(enhanced);
                        notify(&cfg, "Enhanced & copied - Alt+I types it at your cursor, Cmd+V (Mac) or Ctrl+V pastes");
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

/// How a dictation was injected.
struct Injected {
    /// The method actually used (portal falls back to clipboard).
    method: &'static str,
    /// Set when a multi-piece paste stopped early, with the reason.
    interrupted: Option<&'static str>,
}

impl Injected {
    #[cfg(target_os = "linux")]
    fn new(method: &'static str) -> Self {
        Self {
            method,
            interrupted: None,
        }
    }

    /// What the pill says about a finished injection.
    fn detail(&self) -> &'static str {
        match self.method {
            "portal" => "typed",
            "clipboard" | "clipboard-fallback" => "copied",
            _ => "pasted",
        }
    }
}

/// Inject `text` by the configured method (portal falls back to clipboard).
async fn inject_text(
    text: &str,
    injectors: &mut Injectors,
    #[allow(unused)] cfg: &Config,
    images: &[std::path::PathBuf],
) -> anyhow::Result<Injected> {
    #[cfg(target_os = "macos")]
    {
        use crate::inject::macos::{InterruptReason, PasteOutcome};
        let outcome = injectors.macos.inject_with_images(text, images).await?;
        if let Some(notice) = outcome.notice() {
            notify(cfg, &notice);
        }
        let interrupted = match outcome {
            PasteOutcome::Done => None,
            PasteOutcome::Interrupted { reason, .. } => Some(match reason {
                InterruptReason::FocusChanged => "focus-changed",
                InterruptReason::ClipboardChanged => "clipboard-changed",
            }),
        };
        Ok(Injected {
            method: "macos",
            interrupted,
        })
    }

    #[cfg(target_os = "linux")]
    {
        let _ = images;
        match cfg.inject.method {
            InjectMethod::Paste => {
                let restore_clipboard = cfg.inject.restore_clipboard;
                let restore_delay_ms = cfg.inject.restore_delay_ms;
                let snap = if restore_clipboard {
                    crate::inject::restore::snapshot_clipboard()
                } else {
                    None
                };
                // Copy first; even if the chord fails the text is one Ctrl+V away.
                injectors.clipboard.inject(text).await?;
                let post_cc = crate::inject::restore::get_clipboard_change_count();
                let mut sm = crate::inject::restore::ClipboardStateMachine::new();
                sm.record_snapshot(snap);
                sm.record_paste(post_cc);

                match injectors.portal.paste_chord().await {
                    Ok(()) => {
                        if restore_clipboard && sm.should_restore(post_cc) {
                            tokio::task::spawn_blocking(move || {
                                std::thread::sleep(std::time::Duration::from_millis(
                                    restore_delay_ms,
                                ));
                                let curr_cc = crate::inject::restore::get_clipboard_change_count();
                                if sm.should_restore(curr_cc) {
                                    if let Some(snap) = sm.snapshot() {
                                        crate::inject::restore::restore_clipboard(snap);
                                        sm.record_restored();
                                    }
                                }
                            });
                        }
                        Ok(Injected::new("paste"))
                    }
                    Err(e) => {
                        eprintln!("[inject] paste chord failed ({e:#}); text is on the clipboard");
                        Ok(Injected::new("clipboard"))
                    }
                }
            }
            InjectMethod::Portal => match injectors.portal.inject(text).await {
                Ok(()) => Ok(Injected::new("portal")),
                Err(e) => {
                    eprintln!("[inject] portal failed ({e:#}); falling back to clipboard");
                    injectors.clipboard.inject(text).await?;
                    Ok(Injected::new("clipboard-fallback"))
                }
            },
            InjectMethod::Clipboard => {
                injectors.clipboard.inject(text).await?;
                Ok(Injected::new("clipboard"))
            }
        }
    }
}

/// Formats a finished dictation: local checks first, then at most one Jev
/// request for whatever they could not settle, rendered for the frontmost app.
async fn format_dictation(
    pieces: &[crate::vocab::TranscriptPiece],
    active_app: Option<&crate::vocab::ActiveApp>,
    cfg: &Config,
) -> String {
    let plan =
        crate::format::Plan::new(pieces, crate::format::Options::from_config(&cfg.formatting));
    let style = crate::format::code_style(active_app, &cfg.formatting);
    let target = if cfg.formatting.jev.enabled && plan.needs_jev() {
        cfg.formatting.jev.resolve()
    } else {
        None
    };
    let Some(target) = target else {
        eprintln!("[jev] {} calls=0 style={style:?}", plan.summary());
        crate::format::record(None);
        return plan.render(None, style);
    };
    let app_name = active_app.and_then(|a| a.name.as_deref().or(a.bundle_id.as_deref()));
    let request = plan
        .jev_request(app_name, &target.model)
        .expect("plan.needs_jev() guarantees a request");
    let started = Instant::now();
    let result = crate::jev::evaluate(
        &request,
        &target.api_key,
        cfg.formatting.jev.timeout_ms,
        target.provider,
    )
    .await;
    let latency_ms = started.elapsed().as_millis() as u64;
    let answers = match result {
        Ok(raw) => {
            let answers = plan.parse_answers(&raw);
            eprintln!(
                "[jev] {} calls=1 latency={latency_ms}ms style={style:?} answers={raw}",
                plan.summary()
            );
            Some(answers)
        }
        Err(e) => {
            eprintln!(
                "[jev] {} calls=1 latency={latency_ms}ms style={style:?} fallback: {e:#}",
                plan.summary()
            );
            None
        }
    };
    crate::format::record(Some((latency_ms, answers.is_some())));
    plan.render(answers.as_ref(), style)
}

/// How the text went in, final text, last audio id and total audio seconds.
type Transcribed = (Injected, String, Option<String>, f64);

/// The event that closes a dictation. `capped` marks a session that hit the
/// max-length limit; its text was still transcribed and pasted.
fn finalize_outcome(result: &anyhow::Result<Option<Transcribed>>, capped: bool) -> Event {
    match result {
        Ok(None) => outcome_event("no-speech", "", None),
        Ok(Some((injected, text, _, _))) => {
            let chars = Some(text.chars().count());
            if let Some(reason) = injected.interrupted {
                outcome_event("paste-interrupted", reason, chars)
            } else if capped {
                outcome_event("max-length", injected.detail(), chars)
            } else {
                outcome_event("done", injected.detail(), chars)
            }
        }
        Err(_) => outcome_event("error", "", None),
    }
}

fn finalize(
    runtime: &tokio::runtime::Runtime,
    pieces: &mut Vec<Piece>,
    injectors: &mut Injectors,
    cfg: &Config,
    shared: &Arc<Mutex<Shared>>,
    capped: bool,
) {
    let t_end = Instant::now();
    let n_pieces = pieces.len();
    if n_pieces > 0 {
        notify(cfg, "Transcribing…");
    }
    // notify-rust's blocking show() cannot run inside block_on (it spins up
    // its own runtime), so the async block only returns what to say.
    let outcome: anyhow::Result<Option<Transcribed>> = runtime.block_on(async {
        let mut resolved_pieces: Vec<crate::vocab::TranscriptPiece> = Vec::new();
        let mut last_audio_id: Option<String> = None;
        let mut total_duration_s = 0.0;
        for piece in pieces.drain(..) {
            match piece {
                Piece::Spoken {
                    handle,
                    audio_id,
                    duration_s,
                } => {
                    let transcript = handle.await.context("transcription task panicked")??;
                    let text = transcript.text.trim().to_string();
                    if !text.is_empty() {
                        resolved_pieces.extend(split_voice_clipboard_triggers(&text));
                        last_audio_id = Some(audio_id);
                        total_duration_s += duration_s;
                    }
                }
                Piece::Inserted(text) => {
                    let trimmed = text.trim().to_string();
                    if !trimmed.is_empty() {
                        resolved_pieces.push(crate::vocab::TranscriptPiece::Inserted(trimmed));
                    }
                }
            }
        }
        if resolved_pieces.is_empty() {
            return Ok(None);
        }

        let active_app = crate::vocab::detect_frontmost_app();
        let user_terms = if cfg.vocab.enabled {
            crate::userdata::read_user_vocabulary_terms()
        } else {
            Vec::new()
        };

        let pieces: Vec<crate::vocab::TranscriptPiece> = resolved_pieces
            .into_iter()
            .map(|piece| match piece {
                crate::vocab::TranscriptPiece::Spoken(s) if cfg.vocab.enabled => {
                    crate::vocab::TranscriptPiece::Spoken(crate::vocab::clean_text(
                        &s,
                        active_app.as_ref(),
                        &user_terms,
                    ))
                }
                other => other,
            })
            .collect();
        let text = format_dictation(&pieces, active_app.as_ref(), cfg).await;

        eprintln!(
            "[assemble] pieces={} chars={}",
            n_pieces,
            text.chars().count()
        );
        println!("[result]  {text}");

        let images = shared.lock().unwrap().captured_context_images.clone();

        let t_inject = Instant::now();
        let injected = inject_text(&text, injectors, cfg, &images).await?;
        let used = injected.method;
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
        Ok(Some((injected, text, last_audio_id, total_duration_s)))
    });
    let closing = finalize_outcome(&outcome, capped);
    match &outcome {
        Ok(None) => {
            eprintln!("[skip] no speech detected");
            notify(cfg, "No speech detected");
        }
        Ok(Some((injected, text, audio_id, duration_s))) => {
            let head = match injected.method {
                "paste" => "Pasted + on clipboard",
                "portal" => "Typed + copied - Ctrl+V pastes it elsewhere",
                _ => "On clipboard - paste with Ctrl+V",
            };
            notify_result(cfg, &format!("{head}\n{text}"));
            let images_for_history = shared.lock().unwrap().captured_context_images.clone();
            crate::userdata::append_history(
                "dictation",
                text,
                audio_id.as_deref(),
                Some(*duration_s),
                if images_for_history.is_empty() {
                    None
                } else {
                    Some(&images_for_history)
                },
            );
            shared.lock().unwrap().last_text = Some(text.clone());
        }
        Err(e) => {
            eprintln!("[daemon] session failed: {e:#}");
            notify(cfg, &format!("Error: {e}"));
        }
    }

    let (vision_session_dir, captured_images, session_duration_s) = {
        let mut s = shared.lock().unwrap();
        let duration = s
            .toggle_t0
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        let dir = s.vision_session_dir.take();
        let images = std::mem::take(&mut s.captured_context_images);
        s.vision_detector = None;
        (dir, images, duration)
    };

    let transcript_text = match &outcome {
        Ok(Some((_, text, _, _))) => text.as_str(),
        _ => "",
    };

    let has_transcript = !transcript_text.trim().is_empty();
    let has_context = !captured_images.is_empty();

    if crate::vision::is_accidental_session(has_transcript, has_context, session_duration_s) {
        eprintln!("[vision] accidental short recording (<2.5s) discarded");
        if let Some(session_dir) = vision_session_dir {
            let _ = std::fs::remove_dir_all(&session_dir);
        }
    } else if let Some(session_dir) = vision_session_dir {
        match crate::vision::write_context_bundle(&session_dir, transcript_text, &captured_images) {
            Ok(path) => eprintln!("[vision] saved context bundle: {}", path.display()),
            Err(e) => eprintln!("[vision] failed writing context bundle: {e:#}"),
        }
        let sessions_dir = crate::userdata::sessions_dir();
        if let Ok(removed) = crate::vision::prune_sessions(
            &sessions_dir,
            crate::vision::DEFAULT_MAX_AGE_SECS,
            crate::vision::DEFAULT_MAX_BYTES,
        ) {
            if removed > 0 {
                eprintln!("[vision] pruned {} old session folder(s)", removed);
            }
        }
    }

    let mut s = shared.lock().unwrap();
    // The outcome comes first so a renderer shows the result before it goes idle.
    s.events.publish(closing);
    s.set_phase(Phase::Idle);
    s.clip_snapshot = None;
}

/// What a toggle did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Toggle {
    /// Idle: recording started.
    Started,
    /// Recording: stop requested, the audio thread finishes the segment.
    Stopping,
    /// Recording, within 800 ms of the start: ignored.
    Debounced,
    /// Paused: the dictation so far is being transcribed.
    Finishing,
    /// Already transcribing.
    Busy,
}

/// The single start/stop path shared by the hotkey, `bolo toggle`, the
/// dashboard and the pill.
pub(crate) fn toggle(
    shared: &Arc<Mutex<Shared>>,
    start_tx: &Sender<()>,
    pipeline_tx: &Sender<PipelineMsg>,
    cfg: &Config,
) -> anyhow::Result<Toggle> {
    let mut s = shared.lock().unwrap();
    Ok(match s.phase {
        Phase::Idle => {
            s.set_phase(Phase::Recording);
            s.toggle_t0 = Some(Instant::now());
            if cfg.vision.enabled {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                let session_dir = crate::userdata::sessions_dir().join(format!("session_{now_ms}"));
                if let Err(e) = std::fs::create_dir_all(&session_dir) {
                    eprintln!(
                        "[vision] failed to create session dir {}: {e:#}",
                        session_dir.display()
                    );
                }
                s.vision_detector = Some(crate::vision::CircleGestureDetector::new(
                    cfg.vision.min_angle_degrees,
                ));
                s.vision_session_dir = Some(session_dir);
                s.captured_context_images.clear();
            }
            drop(s);
            start_tx.send(()).context("audio thread gone")?;
            Toggle::Started
        }
        Phase::Recording => {
            // Debounce: ignore accidental rapid double-tap within 800ms of start
            if s.toggle_t0.is_some_and(|t0| t0.elapsed().as_millis() < 800) {
                return Ok(Toggle::Debounced);
            }
            if let Some(tx) = s.control_tx.as_ref() {
                let _ = tx.send(Control::ForceStop);
            }
            Toggle::Stopping
        }
        Phase::Paused => {
            s.set_phase(Phase::Processing);
            drop(s);
            pipeline_tx
                .send(PipelineMsg::Finalize)
                .context("pipeline gone")?;
            Toggle::Finishing
        }
        Phase::Processing => Toggle::Busy,
    })
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
        "toggle" => match toggle(shared, start_tx, pipeline_tx, cfg)? {
            Toggle::Started => {
                if cfg.vision.enabled {
                    notify(
                        cfg,
                        "Listening… (hover in a circle to capture 📸 · Ctrl+Space stop)",
                    );
                } else {
                    notify(
                        cfg,
                        "Listening… (Ctrl+Space stop · Opt+V paste · Opt+P pause)",
                    );
                }
                "ok recording".to_string()
            }
            Toggle::Stopping => "ok stopping".to_string(),
            // A second toggle right after the first is an accidental double tap.
            Toggle::Debounced => return Ok(()),
            Toggle::Finishing => "ok finishing".to_string(),
            Toggle::Busy => "busy processing".to_string(),
        },
        cmd if cmd.starts_with("mouse ") => {
            let mut s = shared.lock().unwrap();
            if s.phase == Phase::Recording && cfg.vision.enabled {
                let parts: Vec<&str> = cmd.split_whitespace().collect();
                if parts.len() >= 3 {
                    if let (Ok(x), Ok(y)) = (parts[1].parse::<f64>(), parts[2].parse::<f64>()) {
                        let now_secs = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs_f64())
                            .unwrap_or(0.0);
                        let detected = s
                            .vision_detector
                            .as_mut()
                            .and_then(|detector| detector.add((x, y), now_secs));

                        if let Some(gesture) = detected {
                            let count = s.captured_context_images.len() + 1;
                            if let Some(session_dir) = s.vision_session_dir.clone() {
                                let img_path = session_dir.join(format!("context-{count}.png"));
                                s.captured_context_images.push(img_path.clone());
                                let shared_clone = Arc::clone(shared);
                                let cfg_clone = cfg.clone();
                                drop(s);
                                std::thread::spawn(move || {
                                    match crate::vision::capture_screen(gesture, &img_path) {
                                        Ok(()) => {
                                            eprintln!(
                                                "[vision] captured context image: {}",
                                                img_path.display()
                                            );
                                            notify(&cfg_clone, "Screen context captured 📸");
                                        }
                                        Err(e) => {
                                            let mut s = shared_clone.lock().unwrap();
                                            s.captured_context_images.retain(|p| p != &img_path);
                                            eprintln!("[vision] gesture recognized but screen capture failed: {e:#}");
                                        }
                                    }
                                });
                                return Ok(());
                            }
                        }
                    }
                }
            }
            "ok".to_string()
        }
        "pause" => {
            let mut s = shared.lock().unwrap();
            match s.phase {
                Phase::Recording => {
                    s.set_phase(Phase::Paused);
                    s.clip_snapshot = read_clipboard();
                    if let Some(tx) = s.control_tx.as_ref() {
                        let _ = tx.send(Control::Pause);
                    }
                    drop(s);
                    notify(
                        cfg,
                        "Paused - Alt+I insert clipboard · Alt+P resume · Ctrl+Space finish",
                    );
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
                    s.set_phase(Phase::Recording);
                    s.toggle_t0 = Some(Instant::now());
                    drop(s);
                    start_tx.send(()).context("audio thread gone")?;
                    if cfg.vision.enabled {
                        notify(
                            cfg,
                            "Listening… (hover in a circle to capture 📸 · Ctrl+Space stop)",
                        );
                    } else {
                        notify(
                            cfg,
                            "Listening… (Ctrl+Space stop · Opt+V paste · Opt+P pause)",
                        );
                    }
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
                    pipeline_tx
                        .send(PipelineMsg::InsertLast(text))
                        .context("pipeline gone")?;
                    "ok inserting".to_string()
                }
                (Phase::Idle, None) => "err nothing to insert yet".to_string(),
                // While paused, Alt+I means "splice the current clipboard
                // into the transcript" - no matter when it was copied (covers
                // content copied before the dictation even started).
                (Phase::Paused, _) => match read_clipboard() {
                    Some(text) if !text.trim().is_empty() => {
                        let n = text.chars().count();
                        // Remember it so the Alt+P resume's changed-clipboard
                        // check doesn't insert the same text twice.
                        s.clip_snapshot = Some(text.clone());
                        drop(s);
                        pipeline_tx
                            .send(PipelineMsg::Insert(text))
                            .context("pipeline gone")?;
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
                        pipeline_tx
                            .send(PipelineMsg::Insert(text))
                            .context("pipeline gone")?;
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
                    pipeline_tx
                        .send(PipelineMsg::Enhance(text))
                        .context("pipeline gone")?;
                    "ok enhancing".to_string()
                }
                (Phase::Idle, None) => "err nothing to enhance yet".to_string(),
                (phase, _) => format!("busy {}", phase.as_str()),
            }
        }
        "status" => shared.lock().unwrap().phase.as_str().to_string(),
        "subscribe" => {
            // The hub owns the connection from here; this thread must not
            // block, because it serves every other command.
            let hub = shared.lock().unwrap().events.clone();
            hub.subscribe(conn);
            return Ok(());
        }
        "quit" => {
            crate::pill::stop();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vocab::TranscriptPiece;
    use std::time::Duration;

    #[test]
    fn voice_trigger_inserts_clipboard_as_a_pasted_piece() {
        let text = "Here is the function paste clipboard please review it.";
        let clip = "def calculate_sum(a, b):\n    return a + b";
        assert_eq!(
            split_voice_clipboard_triggers_with(text, Some(clip)),
            vec![
                TranscriptPiece::Spoken("Here is the function".to_string()),
                TranscriptPiece::Inserted(clip.to_string()),
                TranscriptPiece::Spoken("please review it.".to_string()),
            ]
        );
    }

    #[test]
    fn voice_trigger_keeps_dollar_signs_and_needs_a_clipboard() {
        let text = "Check this script: paste clipboard and run it.";
        let clip = "#!/bin/bash\nexport PATH=\"$HOME/bin:$PATH\"\necho $1";
        let pieces = split_voice_clipboard_triggers_with(text, Some(clip));
        assert_eq!(pieces[1], TranscriptPiece::Inserted(clip.to_string()));
        assert_eq!(
            split_voice_clipboard_triggers_with(text, None),
            vec![TranscriptPiece::Spoken(text.to_string())]
        );
    }

    use crate::events::testing::{next, subscribe};
    use serde_json::json;

    /// Daemon state with a live event hub, as `run` builds it.
    fn daemon_state() -> (Arc<Mutex<Shared>>, EventHub, Config) {
        let mut cfg = Config::load(std::path::Path::new("config.toml")).unwrap();
        // Tests must not pop banners or write capture sessions.
        cfg.daemon.notifications = false;
        cfg.vision.enabled = false;
        let hub = EventHub::spawn(cfg.pill.clone());
        let shared = Arc::new(Mutex::new(Shared {
            events: hub.clone(),
            ..Shared::default()
        }));
        (shared, hub, cfg)
    }

    /// Sends one command line through the real socket handler; returns the reply.
    fn command(
        line: &str,
        shared: &Arc<Mutex<Shared>>,
        start_tx: &Sender<()>,
        pipeline_tx: &Sender<PipelineMsg>,
        cfg: &Config,
    ) -> String {
        let (server, mut client) = UnixStream::pair().unwrap();
        writeln!(client, "{line}").unwrap();
        handle_client(server, shared, start_tx, pipeline_tx, cfg).unwrap();
        let mut reply = String::new();
        BufReader::new(client).read_line(&mut reply).unwrap();
        reply.trim().to_string()
    }

    #[test]
    fn socket_toggle_from_idle_emits_phase_recording() {
        let (shared, hub, cfg) = daemon_state();
        let events = subscribe(&hub);
        let (start_tx, start_rx) = crossbeam_channel::unbounded();
        let (pipeline_tx, _pipeline_rx) = crossbeam_channel::unbounded();

        let reply = command("toggle", &shared, &start_tx, &pipeline_tx, &cfg);

        assert_eq!(reply, "ok recording");
        assert_eq!(
            next(&events),
            json!({ "type": "phase", "phase": "recording" })
        );
        assert!(start_rx.try_recv().is_ok(), "audio thread was not started");
    }

    #[test]
    fn pause_and_resume_emit_paused_then_recording() {
        let (shared, hub, cfg) = daemon_state();
        let (start_tx, _start_rx) = crossbeam_channel::unbounded();
        let (pipeline_tx, _pipeline_rx) = crossbeam_channel::unbounded();
        shared.lock().unwrap().set_phase(Phase::Recording);
        let events = subscribe(&hub);

        assert_eq!(
            command("pause", &shared, &start_tx, &pipeline_tx, &cfg),
            "ok paused"
        );
        assert_eq!(next(&events), json!({ "type": "phase", "phase": "paused" }));
        assert_eq!(
            command("pause", &shared, &start_tx, &pipeline_tx, &cfg),
            "ok recording"
        );
        assert_eq!(
            next(&events),
            json!({ "type": "phase", "phase": "recording" })
        );
    }

    #[test]
    fn toggle_while_paused_finishes_the_dictation() {
        let (shared, hub, cfg) = daemon_state();
        let (start_tx, _start_rx) = crossbeam_channel::unbounded();
        let (pipeline_tx, pipeline_rx) = crossbeam_channel::unbounded();
        shared.lock().unwrap().set_phase(Phase::Paused);
        let events = subscribe(&hub);

        assert_eq!(
            command("toggle", &shared, &start_tx, &pipeline_tx, &cfg),
            "ok finishing"
        );
        assert_eq!(
            next(&events),
            json!({ "type": "phase", "phase": "processing" })
        );
        assert!(matches!(pipeline_rx.try_recv(), Ok(PipelineMsg::Finalize)));
        assert_eq!(
            command("toggle", &shared, &start_tx, &pipeline_tx, &cfg),
            "busy processing"
        );
    }

    #[test]
    fn toggle_within_800ms_of_the_start_is_ignored() {
        let (shared, _hub, cfg) = daemon_state();
        let (start_tx, _start_rx) = crossbeam_channel::unbounded();
        let (pipeline_tx, _pipeline_rx) = crossbeam_channel::unbounded();
        let (control_tx, control_rx) = crossbeam_channel::unbounded();
        {
            let mut s = shared.lock().unwrap();
            s.set_phase(Phase::Recording);
            s.toggle_t0 = Some(Instant::now());
            s.control_tx = Some(control_tx);
        }
        assert_eq!(
            toggle(&shared, &start_tx, &pipeline_tx, &cfg).unwrap(),
            Toggle::Debounced
        );
        assert!(control_rx.try_recv().is_err());

        shared.lock().unwrap().toggle_t0 = Instant::now().checked_sub(Duration::from_secs(1));
        assert_eq!(
            toggle(&shared, &start_tx, &pipeline_tx, &cfg).unwrap(),
            Toggle::Stopping
        );
        assert!(matches!(control_rx.try_recv(), Ok(Control::ForceStop)));
    }

    #[test]
    fn subscribe_command_streams_hello_and_hands_the_connection_to_the_hub() {
        let (shared, _hub, cfg) = daemon_state();
        let (start_tx, _start_rx) = crossbeam_channel::unbounded();
        let (pipeline_tx, _pipeline_rx) = crossbeam_channel::unbounded();
        shared.lock().unwrap().set_phase(Phase::Recording);
        let (server, mut client) = UnixStream::pair().unwrap();
        writeln!(client, "subscribe").unwrap();
        handle_client(server, &shared, &start_tx, &pipeline_tx, &cfg).unwrap();

        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).unwrap();
        let hello: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["v"], 1);
        assert_eq!(hello["phase"], "recording");
    }

    #[test]
    fn finalize_emits_the_outcome_then_idle() {
        let (shared, hub, cfg) = daemon_state();
        shared.lock().unwrap().set_phase(Phase::Processing);
        let events = subscribe(&hub);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut injectors = Injectors::new(&cfg);

        // Nothing was said: no pieces to transcribe.
        finalize(
            &runtime,
            &mut Vec::new(),
            &mut injectors,
            &cfg,
            &shared,
            false,
        );

        assert_eq!(
            next(&events),
            json!({ "type": "outcome", "kind": "no-speech", "detail": "" })
        );
        assert_eq!(next(&events), json!({ "type": "phase", "phase": "idle" }));
        assert_eq!(shared.lock().unwrap().phase, Phase::Idle);
    }

    #[test]
    fn failed_sessions_report_why_then_go_idle() {
        let (shared, hub, _cfg) = daemon_state();
        shared.lock().unwrap().set_phase(Phase::Recording);
        let events = subscribe(&hub);

        end_session(&shared, outcome_event("mic-unavailable", "", None));

        assert_eq!(
            next(&events),
            json!({ "type": "outcome", "kind": "mic-unavailable", "detail": "" })
        );
        assert_eq!(next(&events), json!({ "type": "phase", "phase": "idle" }));
    }

    #[test]
    fn finalize_outcome_names_what_happened() {
        let done = |method, interrupted| -> anyhow::Result<Option<Transcribed>> {
            Ok(Some((
                Injected {
                    method,
                    interrupted,
                },
                "hello".to_string(),
                None,
                1.0,
            )))
        };
        let kind_detail = |e: Event| match e {
            Event::Outcome {
                kind,
                detail,
                chars,
            } => (kind, detail, chars),
            other => panic!("not an outcome: {other:?}"),
        };
        assert_eq!(
            kind_detail(finalize_outcome(&done("macos", None), false)),
            ("done", "pasted", Some(5))
        );
        assert_eq!(
            kind_detail(finalize_outcome(&done("portal", None), false)),
            ("done", "typed", Some(5))
        );
        assert_eq!(
            kind_detail(finalize_outcome(&done("clipboard-fallback", None), false)),
            ("done", "copied", Some(5))
        );
        assert_eq!(
            kind_detail(finalize_outcome(
                &done("macos", Some("focus-changed")),
                false
            )),
            ("paste-interrupted", "focus-changed", Some(5))
        );
        assert_eq!(
            kind_detail(finalize_outcome(&done("macos", None), true)),
            ("max-length", "pasted", Some(5))
        );
        assert_eq!(
            kind_detail(finalize_outcome(&Ok(None), false)),
            ("no-speech", "", None)
        );
        assert_eq!(
            kind_detail(finalize_outcome(&Err(anyhow::anyhow!("boom")), false)),
            ("error", "", None)
        );
    }

    #[tokio::test]
    async fn dictation_without_jev_candidates_formats_locally() {
        let mut cfg = Config::load(std::path::Path::new("config.toml")).unwrap();
        cfg.formatting.jev.enabled = true;
        let pieces = vec![
            TranscriptPiece::Spoken("Speech before".to_string()),
            TranscriptPiece::Inserted("const x: number = 42;\nconsole.log(x);".to_string()),
            TranscriptPiece::Spoken("Speech after".to_string()),
        ];
        let out = format_dictation(&pieces, None, &cfg).await;
        assert_eq!(
            out,
            "Speech before\n\n```typescript\nconst x: number = 42;\nconsole.log(x);\n```\n\nSpeech after"
        );
    }
}
