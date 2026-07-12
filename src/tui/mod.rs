//! `bolo settings` — full-screen bento-grid settings TUI with Bo the cat.
//! Plan reviewed & approved via lavish (M7): $EDITOR for the enhance prompt,
//! save-then-ask-restart modal, mic-test card.

mod ui;

use crate::config::Config;
use anyhow::Context;
use crossbeam_channel::{Receiver, Sender};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const MODELS: &[(&str, &str)] = &[
    ("base.en", "0.7s"),
    ("distil-small.en", "1.5s"),
    ("small.en", "1.9s"),
];
pub const METHODS: &[&str] = &["paste", "portal", "clipboard"];
pub const MAX_LENS: &[(i64, &str)] = &[
    (60_000, "1 min"),
    (120_000, "2 min"),
    (300_000, "5 min"),
    (600_000, "10 min"),
];
pub const PROVIDERS: &[(&str, &str, &str)] = &[
    ("groq", "Groq cloud", "~0.5s   fastest"),
    ("faster-whisper", "faster-whisper", "local & private"),
    ("whisper", "whisper.cpp", "local, slower"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Card {
    Engine,
    Behavior,
    Vocab,
    Enhance,
}

impl Card {
    fn next(self) -> Self {
        match self {
            Card::Engine => Card::Behavior,
            Card::Behavior => Card::Vocab,
            Card::Vocab => Card::Enhance,
            Card::Enhance => Card::Engine,
        }
    }
    fn prev(self) -> Self {
        match self {
            Card::Engine => Card::Enhance,
            Card::Behavior => Card::Engine,
            Card::Vocab => Card::Behavior,
            Card::Enhance => Card::Vocab,
        }
    }
}

pub enum Modal {
    SaveRestart,
    ConfirmQuit,
}

/// Messages from the mic-test worker thread.
pub enum TestMsg {
    Level(f32),
    Done(anyhow::Result<String>),
}

pub struct MicTest {
    pub rx: Receiver<TestMsg>,
    pub levels: Vec<f32>,
    pub transcript: Option<String>,
    pub failed: bool,
    pub started: Instant,
}

pub struct App {
    pub config_path: PathBuf,
    pub doc: toml_edit::DocumentMut,
    pub focus: Card,
    pub cursor: [usize; 4], // per-card row cursor, indexed by Card order
    pub vocab_comments: Vec<String>,
    pub vocab: Vec<String>,
    pub vocab_input: Option<String>,
    pub dirty: bool,
    pub daemon_status: String,
    pub modal: Option<Modal>,
    pub toast: Option<(String, Instant)>,
    pub mic_test: Option<MicTest>,
    last_poll: Instant,
}

fn cidx(c: Card) -> usize {
    match c {
        Card::Engine => 0,
        Card::Behavior => 1,
        Card::Vocab => 2,
        Card::Enhance => 3,
    }
}

impl App {
    pub fn load(config_path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(config_path)
            .with_context(|| format!("cannot read {}", config_path.display()))?;
        let doc: toml_edit::DocumentMut = text.parse().context("config.toml parse failed")?;
        crate::userdata::ensure_starter_files();
        let (vocab_comments, vocab) = load_vocab();
        Ok(Self {
            config_path: config_path.to_path_buf(),
            doc,
            focus: Card::Engine,
            cursor: [0; 4],
            vocab_comments,
            vocab,
            vocab_input: None,
            dirty: false,
            daemon_status: "?".into(),
            modal: None,
            toast: None,
            mic_test: None,
            last_poll: Instant::now() - Duration::from_secs(9),
        })
    }

    // ---- config accessors over the toml_edit document ----
    pub fn str_at(&self, path: &[&str], default: &str) -> String {
        let mut item: &toml_edit::Item = self.doc.as_item();
        for key in path {
            match item.get(key) {
                Some(next) => item = next,
                None => return default.to_string(),
            }
        }
        item.as_str().unwrap_or(default).to_string()
    }
    pub fn bool_at(&self, path: &[&str], default: bool) -> bool {
        let mut item: &toml_edit::Item = self.doc.as_item();
        for key in path {
            match item.get(key) {
                Some(next) => item = next,
                None => return default,
            }
        }
        item.as_bool().unwrap_or(default)
    }
    pub fn int_at(&self, path: &[&str], default: i64) -> i64 {
        let mut item: &toml_edit::Item = self.doc.as_item();
        for key in path {
            match item.get(key) {
                Some(next) => item = next,
                None => return default,
            }
        }
        item.as_integer().unwrap_or(default)
    }
    fn set(&mut self, path: &[&str], value: toml_edit::Value) {
        let mut item = self.doc.as_item_mut();
        for key in &path[..path.len() - 1] {
            if item.get(key).is_none() {
                item[key] = toml_edit::Item::Table(toml_edit::Table::new());
            }
            item = &mut item[key];
        }
        item[path[path.len() - 1]] = toml_edit::value(value);
        self.dirty = true;
    }

    pub fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn poll_daemon(&mut self) {
        if self.last_poll.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_poll = Instant::now();
        self.daemon_status = daemon_command("status").unwrap_or_else(|| "not running".into());
    }

    /// One event-loop turn; returns false to quit.
    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) -> anyhow::Result<LoopAction> {
        // Modal traps input first.
        if let Some(modal) = &self.modal {
            match (modal, code) {
                (Modal::SaveRestart, KeyCode::Enter) => {
                    self.modal = None;
                    self.restart_daemon();
                }
                (Modal::SaveRestart, KeyCode::Esc | KeyCode::Char('l')) => {
                    self.modal = None;
                    self.toast("Saved — restart the daemon when convenient");
                }
                (Modal::ConfirmQuit, KeyCode::Char('y') | KeyCode::Enter) => {
                    self.save()?;
                    return Ok(LoopAction::Quit);
                }
                (Modal::ConfirmQuit, KeyCode::Char('n')) => return Ok(LoopAction::Quit),
                (Modal::ConfirmQuit, KeyCode::Esc) => self.modal = None,
                _ => {}
            }
            return Ok(LoopAction::Continue);
        }
        // Vocabulary inline input mode.
        if let Some(input) = &mut self.vocab_input {
            match code {
                KeyCode::Enter => {
                    let term = input.trim().to_string();
                    self.vocab_input = None;
                    if !term.is_empty() && !self.vocab.contains(&term) {
                        self.vocab.push(term);
                        self.write_vocab();
                        self.toast("Vocabulary updated (applies instantly)");
                    }
                }
                KeyCode::Esc => self.vocab_input = None,
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(ch) => input.push(ch),
                _ => {}
            }
            return Ok(LoopAction::Continue);
        }

        match code {
            KeyCode::Char('q') => {
                if self.dirty {
                    self.modal = Some(Modal::ConfirmQuit);
                } else {
                    return Ok(LoopAction::Quit);
                }
            }
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => {
                return Ok(LoopAction::Quit)
            }
            KeyCode::Tab => self.focus = self.focus.next(),
            KeyCode::BackTab => self.focus = self.focus.prev(),
            KeyCode::Char('s') => {
                if self.dirty {
                    self.save()?;
                    self.modal = Some(Modal::SaveRestart);
                } else {
                    self.toast("Nothing to save");
                }
            }
            KeyCode::Char('t') => self.start_mic_test(),
            KeyCode::Char('e') => return Ok(LoopAction::EditPrompt),
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Left => self.cycle_value(-1),
            KeyCode::Right => self.cycle_value(1),
            KeyCode::Enter | KeyCode::Char(' ') => self.activate(),
            KeyCode::Char('a') if self.focus == Card::Vocab => {
                self.vocab_input = Some(String::new());
            }
            KeyCode::Char('d') if self.focus == Card::Vocab => {
                let i = self.cursor[cidx(Card::Vocab)];
                if i < self.vocab.len() {
                    self.vocab.remove(i);
                    self.write_vocab();
                    self.toast("Vocabulary updated (applies instantly)");
                    self.move_cursor(0);
                }
            }
            _ => {}
        }
        Ok(LoopAction::Continue)
    }

    fn rows_in(&self, card: Card) -> usize {
        match card {
            Card::Engine => PROVIDERS.len() + 1, // + model row
            Card::Behavior => 5,
            Card::Vocab => self.vocab.len().max(1),
            Card::Enhance => 1,
        }
    }

    fn move_cursor(&mut self, delta: i64) {
        let i = cidx(self.focus);
        let rows = self.rows_in(self.focus) as i64;
        self.cursor[i] = (self.cursor[i] as i64 + delta).clamp(0, rows - 1) as usize;
    }

    fn activate(&mut self) {
        match self.focus {
            Card::Engine => {
                let row = self.cursor[cidx(Card::Engine)];
                if row < PROVIDERS.len() {
                    let value = PROVIDERS[row].0;
                    self.set(&["stt", "provider"], value.into());
                } else {
                    self.cycle_value(1);
                }
            }
            Card::Behavior => match self.cursor[cidx(Card::Behavior)] {
                0 => {
                    let v = !self.bool_at(&["daemon", "sounds"], true);
                    self.set(&["daemon", "sounds"], v.into());
                }
                1 => {
                    let v = !self.bool_at(&["daemon", "notifications"], true);
                    self.set(&["daemon", "notifications"], v.into());
                }
                2 => {
                    let v = !self.bool_at(&["vad", "auto_endpoint"], true);
                    self.set(&["vad", "auto_endpoint"], v.into());
                }
                _ => self.cycle_value(1),
            },
            Card::Vocab => self.vocab_input = Some(String::new()),
            Card::Enhance => {} // handled by 'e' -> EditPrompt
        }
    }

    fn cycle_value(&mut self, dir: i64) {
        let cycle = |list_len: i64, at: i64| (at + dir).rem_euclid(list_len);
        match self.focus {
            Card::Engine if self.cursor[cidx(Card::Engine)] == PROVIDERS.len() => {
                let cur = self.str_at(&["stt", "whisper", "model"], "small.en");
                let at = MODELS.iter().position(|(m, _)| *m == cur).unwrap_or(0) as i64;
                let next = MODELS[cycle(MODELS.len() as i64, at) as usize].0;
                self.set(&["stt", "whisper", "model"], next.into());
            }
            Card::Behavior if self.cursor[cidx(Card::Behavior)] == 3 => {
                let cur = self.str_at(&["inject", "method"], "paste");
                let at = METHODS.iter().position(|m| *m == cur).unwrap_or(0) as i64;
                let next = METHODS[cycle(METHODS.len() as i64, at) as usize];
                self.set(&["inject", "method"], next.into());
            }
            Card::Behavior if self.cursor[cidx(Card::Behavior)] == 4 => {
                let cur = self.int_at(&["vad", "max_utterance_ms"], 300_000);
                let at = MAX_LENS.iter().position(|(v, _)| *v == cur).unwrap_or(2) as i64;
                let next = MAX_LENS[cycle(MAX_LENS.len() as i64, at) as usize].0;
                self.set(&["vad", "max_utterance_ms"], next.into());
            }
            _ => {}
        }
    }

    fn save(&mut self) -> anyhow::Result<()> {
        std::fs::write(&self.config_path, self.doc.to_string())
            .with_context(|| format!("cannot write {}", self.config_path.display()))?;
        self.dirty = false;
        Ok(())
    }

    fn write_vocab(&self) {
        let path = crate::userdata::config_dir().join("vocabulary.txt");
        let mut text = self.vocab_comments.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&self.vocab.join("\n"));
        text.push('\n');
        let _ = std::fs::write(path, text);
    }

    fn restart_daemon(&mut self) {
        // Installed setups run the daemon as a systemd user service — let
        // systemd do the restart so it stays supervised.
        let unit_managed = std::process::Command::new("systemctl")
            .args(["--user", "is-enabled", "--quiet", "bolo.service"])
            .status()
            .is_ok_and(|s| s.success());
        if unit_managed {
            let ok = std::process::Command::new("systemctl")
                .args(["--user", "restart", "bolo.service"])
                .status()
                .is_ok_and(|s| s.success());
            self.toast(if ok {
                "Daemon restarting… (portal dialog will show once)"
            } else {
                "systemctl restart failed — check: systemctl --user status bolo"
            });
            self.last_poll = Instant::now() - Duration::from_secs(9);
            return;
        }
        daemon_command("quit");
        std::thread::sleep(Duration::from_millis(400));
        let log = std::fs::File::options()
            .create(true)
            .append(true)
            .open("/tmp/bolo-daemon.log");
        let spawned = std::env::current_exe().ok().and_then(|exe| {
            let mut cmd = std::process::Command::new(exe);
            cmd.arg("daemon");
            if let Ok(log) = log {
                if let Ok(log2) = log.try_clone() {
                    cmd.stdout(log).stderr(log2);
                }
            }
            cmd.spawn().ok()
        });
        match spawned {
            Some(_) => self.toast("Daemon restarting… (portal dialog will show once)"),
            None => self.toast("Could not restart the daemon — run `bolo daemon` manually"),
        }
        self.last_poll = Instant::now() - Duration::from_secs(9);
    }

    fn start_mic_test(&mut self) {
        if self.mic_test.as_ref().is_some_and(|t| t.transcript.is_none() && !t.failed) {
            return; // already running
        }
        if self.daemon_status == "recording" || self.daemon_status == "paused" {
            self.toast("Bo is busy listening — finish the dictation first");
            return;
        }
        // Build a Config from the CURRENT (possibly unsaved) selections so
        // the test exercises exactly what's on screen.
        let cfg: Config = match toml::from_str(&self.doc.to_string()) {
            Ok(cfg) => cfg,
            Err(e) => {
                self.toast(format!("config invalid: {e}"));
                return;
            }
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        std::thread::spawn(move || mic_test_worker(cfg, tx));
        self.mic_test = Some(MicTest {
            rx,
            levels: Vec::new(),
            transcript: None,
            failed: false,
            started: Instant::now(),
        });
    }

    fn drain_mic_test(&mut self) {
        let Some(test) = &mut self.mic_test else { return };
        for msg in test.rx.try_iter() {
            match msg {
                TestMsg::Level(rms) => {
                    test.levels.push(rms);
                    if test.levels.len() > 24 {
                        test.levels.remove(0);
                    }
                }
                TestMsg::Done(Ok(text)) => {
                    test.transcript =
                        Some(if text.is_empty() { "(heard nothing)".into() } else { text });
                }
                TestMsg::Done(Err(e)) => {
                    test.transcript = Some(format!("test failed: {e}"));
                    test.failed = true;
                }
            }
        }
    }
}

enum LoopAction {
    Continue,
    Quit,
    EditPrompt,
}

fn load_vocab() -> (Vec<String>, Vec<String>) {
    let path = crate::userdata::config_dir().join("vocabulary.txt");
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut comments = Vec::new();
    let mut terms = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            comments.push(line.to_string());
        } else if !trimmed.is_empty() {
            terms.push(trimmed.to_string());
        }
    }
    (comments, terms)
}

/// Send one command to the daemon socket; None if unreachable.
fn daemon_command(cmd: &str) -> Option<String> {
    let conn = UnixStream::connect(crate::daemon::socket_path()).ok()?;
    conn.set_read_timeout(Some(Duration::from_millis(500))).ok()?;
    let mut conn = conn;
    writeln!(conn, "{cmd}").ok()?;
    let mut reply = String::new();
    BufReader::new(conn).read_line(&mut reply).ok()?;
    let reply = reply.trim().to_string();
    (!reply.is_empty()).then_some(reply)
}

/// Record ~3s from the default mic, transcribe with the configured provider.
fn mic_test_worker(cfg: Config, tx: Sender<TestMsg>) {
    let result = (|| -> anyhow::Result<String> {
        let (audio_tx, audio_rx) = crossbeam_channel::unbounded::<Vec<f32>>();
        let (stream, info) = crate::audio::start_capture(audio_tx)?;
        let mut resampler = crate::resample::StreamResampler::new(info.sample_rate)?;
        let mut samples: Vec<i16> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            match audio_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => {
                    let rms = (chunk.iter().map(|s| s * s).sum::<f32>()
                        / chunk.len().max(1) as f32)
                        .sqrt();
                    let _ = tx.send(TestMsg::Level(rms));
                    let resampled = resampler.process(&chunk)?;
                    samples.extend(
                        resampled.iter().map(|&s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16),
                    );
                }
                Err(_) => {}
            }
        }
        drop(stream);
        anyhow::ensure!(!samples.is_empty(), "no audio captured (mic wedged?)");
        let wav = crate::stt::groq::encode_wav(&samples)?;
        let provider = crate::stt::make_provider(&cfg)?;
        let runtime = tokio::runtime::Runtime::new()?;
        let transcript = runtime.block_on(provider.transcribe(wav))?;
        Ok(transcript.text)
    })();
    let _ = tx.send(TestMsg::Done(result));
}

pub fn run(config_path: &Path) -> anyhow::Result<()> {
    let mut app = App::load(config_path)?;
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
) -> anyhow::Result<()> {
    loop {
        app.poll_daemon();
        app.drain_mic_test();
        if let Some((_, at)) = &app.toast {
            if at.elapsed() > Duration::from_secs(4) {
                app.toast = None;
            }
        }
        terminal.draw(|frame| ui::draw(frame, app))?;
        if !event::poll(Duration::from_millis(120))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                match app.on_key(key.code, key.modifiers)? {
                    LoopAction::Continue => {}
                    LoopAction::Quit => return Ok(()),
                    LoopAction::EditPrompt => {
                        edit_prompt_in_editor(terminal)?;
                        app.toast("Enhance prompt updated (applies instantly)");
                    }
                }
            }
            _ => {}
        }
    }
}

/// Suspend the TUI, open the enhance prompt in $EDITOR, resume. Uses manual
/// raw-mode/alt-screen toggling (not ratatui::restore/init) because init()
/// probes the cursor position, which needs a terminal round-trip.
fn edit_prompt_in_editor(terminal: &mut ratatui::DefaultTerminal) -> anyhow::Result<()> {
    use ratatui::crossterm::terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    };
    let path = crate::userdata::config_dir().join("enhance_prompt.txt");
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "nano".into());
    disable_raw_mode()?;
    ratatui::crossterm::execute!(std::io::stdout(), LeaveAlternateScreen)?;
    let status = std::process::Command::new(&editor).arg(&path).status();
    ratatui::crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;
    enable_raw_mode()?;
    // Fresh Terminal = empty buffers = full repaint on the next draw, with
    // no cursor-position probe (Terminal::clear() would send one).
    *terminal = ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(
        std::io::stdout(),
    ))?;
    if let Err(e) = status {
        anyhow::bail!("failed to run editor {editor}: {e}");
    }
    Ok(())
}
