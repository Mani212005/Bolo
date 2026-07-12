//! Rendering for the settings TUI: bento cards, Bo the cat, modal overlay.

use super::{App, Card, MicTest, Modal, MAX_LENS, METHODS, MODELS, PROVIDERS};
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

const ACCENT: Color = Color::Blue;
const FOCUS: Color = Color::Yellow;
const OK: Color = Color::Green;
const KEY: Color = Color::Magenta;
const DIM: Color = Color::DarkGray;

fn card_block(title: &str, focused: bool) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { FOCUS } else { ACCENT }))
        .title(format!(" {title} "))
        .title_style(Style::default().fg(if focused { FOCUS } else { Color::White }).bold())
}

fn key_hint<'a>(key: &'a str, label: &'a str) -> Vec<Span<'a>> {
    vec![
        Span::styled(key, Style::default().fg(KEY).bold()),
        Span::raw(" "),
        Span::styled(label, Style::default().fg(Color::Gray)),
        Span::raw("   "),
    ]
}

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(10), Constraint::Length(1)])
            .areas(area);

    draw_header(frame, app, header);
    if area.width >= 84 && area.height >= 26 {
        draw_bento(frame, app, body);
    } else {
        draw_stacked(frame, app, body);
    }
    draw_footer(frame, app, footer);
    if let Some(modal) = &app.modal {
        draw_modal(frame, app, modal);
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let status_color = match app.daemon_status.as_str() {
        "idle" => OK,
        "recording" => Color::Red,
        "paused" => FOCUS,
        "processing" => KEY,
        _ => DIM,
    };
    let mut spans = vec![
        Span::styled(" BOLO SETTINGS", Style::default().fg(Color::White).bold()),
        Span::styled(
            if app.dirty { "  [unsaved changes]" } else { "" },
            Style::default().fg(FOCUS),
        ),
    ];
    let right = format!("daemon: {} ", app.daemon_status);
    let pad = (area.width as usize).saturating_sub(
        spans.iter().map(|s| s.content.chars().count()).sum::<usize>() + right.chars().count(),
    );
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(right, Style::default().fg(status_color).bold()));
    let text = vec![
        Line::from(spans),
        Line::from(Span::styled(
            "─".repeat(area.width as usize),
            Style::default().fg(ACCENT),
        )),
    ];
    frame.render_widget(Paragraph::new(text), area);
}

fn draw_bento(frame: &mut Frame, app: &App, area: Rect) {
    let [row1, row2, row3] = Layout::vertical([
        Constraint::Length(11),
        Constraint::Min(8),
        Constraint::Length(4),
    ])
    .areas(area);
    let [bo, engine] =
        Layout::horizontal([Constraint::Length(30), Constraint::Min(40)]).areas(row1);
    let [vocab, behavior, hotkeys] = Layout::horizontal([
        Constraint::Percentage(32),
        Constraint::Percentage(38),
        Constraint::Percentage(30),
    ])
    .areas(row2);
    draw_bo(frame, app, bo);
    draw_engine(frame, app, engine);
    draw_vocab(frame, app, vocab);
    draw_behavior(frame, app, behavior);
    draw_hotkeys(frame, hotkeys);
    draw_enhance(frame, app, row3);
}

fn draw_stacked(frame: &mut Frame, app: &App, area: Rect) {
    let [bo, engine, behavior, vocab, enhance] = Layout::vertical([
        Constraint::Length(9),
        Constraint::Length(7),
        Constraint::Length(7),
        Constraint::Min(4),
        Constraint::Length(4),
    ])
    .areas(area);
    draw_bo(frame, app, bo);
    draw_engine(frame, app, engine);
    draw_behavior(frame, app, behavior);
    draw_vocab(frame, app, vocab);
    draw_enhance(frame, app, enhance);
}

/// Bo's face + speech bubble follow daemon state / mic test.
fn bo_state(app: &App) -> (&'static str, String, Option<String>) {
    if let Some(MicTest { levels, transcript, failed, .. }) = &app.mic_test {
        if let Some(text) = transcript {
            let face = if *failed { "x.x" } else { "^.^" };
            return (face, format!("heard: {text}"), None);
        }
        let bar: String = levels
            .iter()
            .rev()
            .take(12)
            .rev()
            .map(|rms| {
                let i = ((rms * 40.0) as usize).min(7);
                ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'][i]
            })
            .collect();
        return ("O.O", "listening…".into(), Some(bar));
    }
    match app.daemon_status.as_str() {
        "idle" => ("o.o", "ready!".into(), None),
        "recording" => ("O.O", "I hear you!".into(), None),
        "paused" => ("-.-", "paused…".into(), None),
        "processing" => ("~.~", "thinking…".into(), None),
        _ => ("z.z", "daemon off".into(), None),
    }
}

fn draw_bo(frame: &mut Frame, app: &App, area: Rect) {
    let (face, bubble, bar) = bo_state(app);
    let cat = Style::default().fg(OK);
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![Span::styled("      /\\_/\\", cat)]),
        Line::from(vec![
            Span::styled(format!("     ( {face} )"), cat),
            Span::styled(format!("  < {bubble}"), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![Span::styled("      > ^ <", cat)]),
        Line::from(vec![
            Span::styled("     /|   |\\", cat),
            Span::styled(
                bar.map(|b| format!("  {b}")).unwrap_or_default(),
                Style::default().fg(KEY),
            ),
        ]),
        Line::from(vec![Span::styled("    (_|   |_)", cat)]),
        Line::from(""),
    ];
    lines.push(Line::from(key_hint("t", "test my mic")));
    frame.render_widget(Paragraph::new(lines).block(card_block("Bo", false)), area);
}

fn draw_engine(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Card::Engine;
    let cursor = app.cursor[0];
    let provider = app.str_at(&["stt", "provider"], "groq");
    let model = app.str_at(&["stt", "whisper", "model"], "small.en");
    let mut lines = vec![Line::from("")];
    for (i, (value, label, hint)) in PROVIDERS.iter().enumerate() {
        let selected = provider == *value;
        let marker = if selected { "(*)" } else { "( )" };
        let style = if focused && cursor == i {
            Style::default().fg(Color::White).bg(Color::Indexed(236)).bold()
        } else if selected {
            Style::default().fg(OK)
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{marker} {label:<18} {hint}"), style),
        ]));
    }
    lines.push(Line::from(Span::styled(
        format!("  {}", "─".repeat(area.width.saturating_sub(6) as usize)),
        Style::default().fg(ACCENT),
    )));
    let speed = MODELS
        .iter()
        .find(|(m, _)| *m == model)
        .map(|(_, s)| *s)
        .unwrap_or("?");
    let model_style = if focused && cursor == PROVIDERS.len() {
        Style::default().fg(Color::White).bg(Color::Indexed(236)).bold()
    } else {
        Style::default().fg(OK)
    };
    lines.push(Line::from(vec![
        Span::styled("  Local model   ", Style::default().fg(Color::Gray)),
        Span::styled(format!("‹ {model}  {speed} ›"), model_style),
    ]));
    if provider == "groq" {
        lines.push(Line::from(Span::styled(
            "  (local model is used when a local provider is selected)",
            Style::default().fg(DIM),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).block(card_block("Speech engine", focused)),
        area,
    );
}

fn draw_behavior(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Card::Behavior;
    let cursor = app.cursor[1];
    let check = |v: bool| if v { "[x]" } else { "[ ]" };
    let rows: Vec<(String, bool)> = vec![
        (
            format!("{} Sounds", check(app.bool_at(&["daemon", "sounds"], true))),
            app.bool_at(&["daemon", "sounds"], true),
        ),
        (
            format!("{} Notifications", check(app.bool_at(&["daemon", "notifications"], true))),
            app.bool_at(&["daemon", "notifications"], true),
        ),
        (
            format!("{} Auto-stop on silence", check(app.bool_at(&["vad", "auto_endpoint"], true))),
            app.bool_at(&["vad", "auto_endpoint"], true),
        ),
        (
            format!("Insert    ‹ {} ›", app.str_at(&["inject", "method"], "paste")),
            true,
        ),
        (
            {
                let ms = app.int_at(&["vad", "max_utterance_ms"], 300_000);
                let label = MAX_LENS
                    .iter()
                    .find(|(v, _)| *v == ms)
                    .map(|(_, l)| (*l).to_string())
                    .unwrap_or_else(|| format!("{}s", ms / 1000));
                format!("Max len   ‹ {label} ›")
            },
            true,
        ),
    ];
    let mut lines = vec![Line::from("")];
    for (i, (label, on)) in rows.iter().enumerate() {
        let style = if focused && cursor == i {
            Style::default().fg(Color::White).bg(Color::Indexed(236)).bold()
        } else if *on && i < 3 {
            Style::default().fg(OK)
        } else if i < 3 {
            Style::default().fg(Color::Gray)
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(vec![Span::raw("  "), Span::styled(label.clone(), style)]));
    }
    frame.render_widget(
        Paragraph::new(lines).block(card_block("Behavior", focused)),
        area,
    );
}

fn draw_vocab(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Card::Vocab;
    let cursor = app.cursor[2];
    let mut lines = vec![Line::from("")];
    if app.vocab.is_empty() {
        lines.push(Line::from(Span::styled("  (empty)", Style::default().fg(DIM))));
    }
    let visible = area.height.saturating_sub(5) as usize;
    let start = cursor.saturating_sub(visible.saturating_sub(1));
    for (i, term) in app.vocab.iter().enumerate().skip(start).take(visible.max(1)) {
        let style = if focused && cursor == i {
            Style::default().fg(Color::White).bg(Color::Indexed(236)).bold()
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(vec![Span::raw("  "), Span::styled(term.clone(), style)]));
    }
    if let Some(input) = &app.vocab_input {
        lines.push(Line::from(vec![
            Span::styled("  + ", Style::default().fg(OK)),
            Span::styled(format!("{input}▏"), Style::default().fg(Color::White)),
        ]));
    } else {
        lines.push(Line::from(""));
        let mut hint = key_hint("a", "add");
        hint.extend(key_hint("d", "delete"));
        hint.insert(0, Span::raw("  "));
        lines.push(Line::from(hint));
    }
    let title = format!("Vocabulary ({})", app.vocab.len());
    frame.render_widget(Paragraph::new(lines).block(card_block(&title, focused)), area);
}

fn draw_hotkeys(frame: &mut Frame, area: Rect) {
    let key = |k: &'static str, v: &'static str| {
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{k:<10}"), Style::default().fg(KEY).bold()),
            Span::styled(v, Style::default().fg(Color::Gray)),
        ])
    };
    let lines = vec![
        Line::from(""),
        key("Ctrl+Spc", "start / finish"),
        key("Alt+P", "pause / resume"),
        key("Alt+I", "insert / re-type"),
        Line::from(""),
        Line::from(Span::styled("  (edit: install-hotkey.sh)", Style::default().fg(DIM))),
    ];
    frame.render_widget(Paragraph::new(lines).block(card_block("Hotkeys", false)), area);
}

fn draw_enhance(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Card::Enhance;
    let preview = crate::userdata::enhance_prompt();
    let (text, style) = match &preview {
        Some(p) => {
            let first = p.lines().next().unwrap_or_default().to_string();
            let extra = p.lines().count().saturating_sub(1);
            let suffix = if extra > 0 { format!("  (+{extra} more lines)") } else { String::new() };
            (format!("{first}{suffix}"), Style::default().fg(Color::Gray))
        }
        None => ("(built-in default prompt)".to_string(), Style::default().fg(DIM)),
    };
    let mut spans = vec![Span::raw("  "), Span::styled(text, style), Span::raw("   ")];
    spans.extend(key_hint("e", "edit in $EDITOR"));
    let lines = vec![Line::from(""), Line::from(spans)];
    frame.render_widget(
        Paragraph::new(lines).block(card_block("Enhance prompt", focused)),
        area,
    );
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    if let Some((toast, _)) = &app.toast {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {toast}"),
                Style::default().fg(Color::Black).bg(FOCUS).add_modifier(Modifier::BOLD),
            ))),
            area,
        );
        return;
    }
    let mut spans = vec![Span::raw(" ")];
    spans.extend(key_hint("Tab", "next card"));
    spans.extend(key_hint("↑↓", "move"));
    spans.extend(key_hint("Enter", "change"));
    spans.extend(key_hint("←→", "cycle"));
    spans.extend(key_hint("s", "save"));
    spans.extend(key_hint("t", "test"));
    spans.extend(key_hint("q", "quit"));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_modal(frame: &mut Frame, app: &App, modal: &Modal) {
    let area = frame.area();
    let [popup] = Layout::horizontal([Constraint::Length(46)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::vertical([Constraint::Length(8)]).flex(Flex::Center).areas(popup);
    frame.render_widget(Clear, popup);
    let (title, body): (&str, Vec<Line>) = match modal {
        Modal::SaveRestart => (
            "Save changes",
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  Saved. Restart daemon to apply?",
                    Style::default().fg(Color::White),
                )),
                Line::from(Span::styled(
                    "  (portal dialog will show once)",
                    Style::default().fg(DIM),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::raw("   "),
                    Span::styled("[ Enter: restart now ]", Style::default().fg(OK).bold()),
                    Span::raw("  "),
                    Span::styled("[ Esc: later ]", Style::default().fg(Color::Gray)),
                ]),
            ],
        ),
        Modal::ConfirmQuit => (
            "Unsaved changes",
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  Save config before quitting?",
                    Style::default().fg(Color::White),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::raw("   "),
                    Span::styled("[ y: save & quit ]", Style::default().fg(OK).bold()),
                    Span::raw("  "),
                    Span::styled("[ n: discard ]", Style::default().fg(Color::Red)),
                    Span::raw("  "),
                    Span::styled("[ Esc ]", Style::default().fg(Color::Gray)),
                ]),
            ],
        ),
    };
    let _ = app;
    frame.render_widget(
        Paragraph::new(body).block(card_block(title, true)),
        popup,
    );
}
