use super::split::split_for_terminal;
use super::{restore, TextInjector};
use crate::config::{InjectConfig, TerminalPasteConfig};
use anyhow::Context;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const APPLESCRIPT_PASTE_CHORD: &str = r#"
    tell application "System Events"
        keystroke "v" using command down
    end tell
"#;

/// Builds AppleScript command string to set clipboard contents to a PNG file.
pub fn build_set_image_applescript(path: &Path) -> String {
    let escaped = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!(r#"set the clipboard to (read (POSIX file "{escaped}") as «class PNGf»)"#)
}

/// Formats a list of image paths as space-separated quoted strings for CLI / terminal insertion.
pub fn format_image_paths_for_cli(images: &[PathBuf]) -> String {
    images
        .iter()
        .map(|p| format!("\"{}\"", p.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Selects the last captured screenshot image from a session image list.
#[allow(dead_code)]
pub fn select_last_session_image(images: &[PathBuf]) -> Option<&Path> {
    images.last().map(|p| p.as_path())
}

/// Checks whether the frontmost application is a terminal emulator or CLI prompt.
pub fn is_terminal_app(bundle_id: Option<&str>, app_name: Option<&str>) -> bool {
    let terminal_ids = [
        "com.apple.terminal",
        "com.googlecode.iterm2",
        "net.kovidgoyal.kitty",
        "io.alacritty",
        "org.alacritty",
        "com.mitchellh.ghostty",
        "com.github.wez.wezterm",
        "dev.warp.warp-stable",
        "dev.warp.warp",
        "co.zeit.hyper",
        "org.tabby",
        "com.termius-danilook.mac",
    ];

    if let Some(id) = bundle_id {
        let id_lower = id.to_lowercase();
        if terminal_ids
            .iter()
            .any(|t| id_lower == *t || id_lower.contains(t))
        {
            return true;
        }
        if id_lower.contains("terminal")
            || id_lower.contains("iterm")
            || id_lower.contains("ghostty")
            || id_lower.contains("kitty")
            || id_lower.contains("alacritty")
            || id_lower.contains("wezterm")
            || id_lower.contains("warp")
        {
            return true;
        }
    }

    if let Some(name) = app_name {
        let name_lower = name.to_lowercase();
        if name_lower.contains("terminal")
            || name_lower.contains("iterm")
            || name_lower.contains("ghostty")
            || name_lower.contains("kitty")
            || name_lower.contains("alacritty")
            || name_lower.contains("wezterm")
            || name_lower.contains("warp")
        {
            return true;
        }
    }

    false
}

/// Detects the frontmost application's bundle identifier and localized name via NSWorkspace / AppleScript.
pub fn detect_frontmost_app_info() -> (Option<String>, Option<String>) {
    // 1. Query NSWorkspace via JXA (JavaScript for Automation)
    let jxa = r#"ObjC.import("AppKit"); const app = $.NSWorkspace.sharedWorkspace.frontmostApplication; app ? ((app.bundleIdentifier ? app.bundleIdentifier.js : "") + "\t" + (app.localizedName ? app.localizedName.js : "")) : """#;
    if let Ok(output) = Command::new("osascript")
        .arg("-l")
        .arg("JavaScript")
        .arg("-e")
        .arg(jxa)
        .output()
    {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !stdout.is_empty() {
                let parts: Vec<&str> = stdout.split('\t').collect();
                let bundle_id = parts.first().and_then(|s| {
                    if s.is_empty() {
                        None
                    } else {
                        Some(s.to_string())
                    }
                });
                let app_name = parts.get(1).and_then(|s| {
                    if s.is_empty() {
                        None
                    } else {
                        Some(s.to_string())
                    }
                });
                if bundle_id.is_some() || app_name.is_some() {
                    return (bundle_id, app_name);
                }
            }
        }
    }

    // 2. Fallback to System Events via AppleScript
    let applescript = r#"tell application "System Events"
        set frontApp to first application process whose frontmost is true
        return (bundle identifier of frontApp) & tab & (name of frontApp)
    end tell"#;
    if let Ok(output) = Command::new("osascript")
        .arg("-e")
        .arg(applescript)
        .output()
    {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !stdout.is_empty() {
                let parts: Vec<&str> = stdout.split('\t').collect();
                let bundle_id = parts.first().and_then(|s| {
                    if s.is_empty() {
                        None
                    } else {
                        Some(s.to_string())
                    }
                });
                let app_name = parts.get(1).and_then(|s| {
                    if s.is_empty() {
                        None
                    } else {
                        Some(s.to_string())
                    }
                });
                return (bundle_id, app_name);
            }
        }
    }

    (None, None)
}

/// Returns the macOS LaunchServices token (ASN) of the frontmost app, or
/// `None` when `lsappinfo` is unavailable. Cheap (a few ms), so it can guard
/// every piece of a multi-paste against a focus change.
pub fn frontmost_app_token() -> Option<String> {
    let output = Command::new("lsappinfo").arg("front").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    stdout.starts_with("ASN:").then_some(stdout)
}

/// Whether to paste into the frontmost app as a terminal: `exclude_apps`
/// wins, then `extra_apps`, then the built-in terminal list. Patterns are
/// case-insensitive substrings of the app name or bundle id.
pub fn is_terminal_target(
    bundle_id: Option<&str>,
    app_name: Option<&str>,
    cfg: &TerminalPasteConfig,
) -> bool {
    let name = app_name.unwrap_or("").to_lowercase();
    let id = bundle_id.unwrap_or("").to_lowercase();
    let matches = |pattern: &String| {
        let p = pattern.trim().to_lowercase();
        !p.is_empty() && (name.contains(&p) || id.contains(&p))
    };
    if cfg.exclude_apps.iter().any(matches) {
        return false;
    }
    cfg.extra_apps.iter().any(matches) || is_terminal_app(bundle_id, app_name)
}

/// One clipboard write followed by one Cmd+V.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Paste {
    Text(String),
    /// A screenshot's quoted path typed into a terminal; Claude Code turns a
    /// paste that is only an image path into an attached image.
    ImagePath {
        text: String,
        path: PathBuf,
    },
    /// GUI apps: the PNG itself on the clipboard.
    Image(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Terminal,
    Other,
}

/// Today's single terminal paste: the text, with quoted screenshot paths after it.
fn single_terminal_paste(text: &str, images: &[PathBuf]) -> Option<Paste> {
    let full = if images.is_empty() {
        text.to_string()
    } else if text.trim().is_empty() {
        format_image_paths_for_cli(images)
    } else {
        format!("{text} {}", format_image_paths_for_cli(images))
    };
    (!full.is_empty()).then_some(Paste::Text(full))
}

/// Plans the pastes that deliver `text` and screenshots to the frontmost app.
pub fn plan_pastes(
    text: &str,
    images: &[PathBuf],
    target: Target,
    cfg: &TerminalPasteConfig,
) -> Vec<Paste> {
    match target {
        Target::Other => (!text.is_empty())
            .then(|| Paste::Text(text.to_string()))
            .into_iter()
            .chain(images.iter().cloned().map(Paste::Image))
            .collect(),
        Target::Terminal => {
            if !cfg.split_paste {
                return single_terminal_paste(text, images).into_iter().collect();
            }
            let body = if images.is_empty() {
                text.to_string()
            } else if text.trim().is_empty() {
                String::new()
            } else {
                format!("{text} ")
            };
            let pieces = split_for_terminal(&body, cfg.budget());
            if pieces.len() > cfg.max_pieces {
                return single_terminal_paste(text, images).into_iter().collect();
            }
            let mut pastes: Vec<Paste> = pieces
                .into_iter()
                .map(|p| Paste::Text(p.to_string()))
                .collect();
            for (i, path) in images.iter().enumerate() {
                let quoted = format!("\"{}\"", path.to_string_lossy());
                pastes.push(Paste::ImagePath {
                    text: if i == 0 { quoted } else { format!(" {quoted}") },
                    path: path.clone(),
                });
            }
            pastes
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    Continue,
    FocusChanged,
    ClipboardChanged,
}

/// Decides whether the next paste may go ahead. A changed clipboard wins over
/// a changed focus: the user's own copy must never be overwritten or restored over.
pub fn check_guard(
    initial_front: Option<&str>,
    current_front: Option<&str>,
    expected_change_count: Option<i64>,
    current_change_count: Option<i64>,
) -> Guard {
    if let (Some(expected), Some(current)) = (expected_change_count, current_change_count) {
        if expected != current {
            return Guard::ClipboardChanged;
        }
    }
    if let (Some(initial), Some(current)) = (initial_front, current_front) {
        if initial != current {
            return Guard::FocusChanged;
        }
    }
    Guard::Continue
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptReason {
    FocusChanged,
    ClipboardChanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteOutcome {
    Done,
    Interrupted {
        pasted: usize,
        total: usize,
        reason: InterruptReason,
    },
}

impl PasteOutcome {
    /// The notification text for an interrupted paste.
    pub fn notice(&self) -> Option<String> {
        match self {
            PasteOutcome::Done => None,
            PasteOutcome::Interrupted {
                pasted,
                total,
                reason,
            } => Some(match reason {
                InterruptReason::FocusChanged => format!(
                    "Focus changed - pasted {pasted} of {total} parts. Alt+I inserts the whole dictation again."
                ),
                InterruptReason::ClipboardChanged => format!(
                    "Clipboard changed - pasted {pasted} of {total} parts and kept your new copy. Alt+I inserts the whole dictation again."
                ),
            }),
        }
    }
}

pub struct MacOsTextInjector {
    restore_clipboard: bool,
    restore_delay_ms: u64,
    terminal: TerminalPasteConfig,
}

impl MacOsTextInjector {
    pub fn new(cfg: &InjectConfig) -> Self {
        Self {
            restore_clipboard: cfg.restore_clipboard,
            restore_delay_ms: cfg.restore_delay_ms,
            terminal: cfg.terminal.clone(),
        }
    }

    pub async fn inject_with_images(
        &mut self,
        text: &str,
        images: &[PathBuf],
    ) -> anyhow::Result<PasteOutcome> {
        let text = text.to_owned();
        let images = images.to_vec();
        let restore_clipboard = self.restore_clipboard;
        let restore_delay_ms = self.restore_delay_ms;
        let terminal = self.terminal.clone();

        tokio::task::spawn_blocking(move || {
            inject_macos_blocking(
                &text,
                &images,
                restore_clipboard,
                restore_delay_ms,
                &terminal,
            )
        })
        .await?
    }

    pub async fn inject_with_image(
        &mut self,
        text: &str,
        image_path: Option<&Path>,
    ) -> anyhow::Result<PasteOutcome> {
        let images = image_path
            .map(|p| vec![p.to_path_buf()])
            .unwrap_or_default();
        self.inject_with_images(text, &images).await
    }
}

#[async_trait::async_trait]
impl TextInjector for MacOsTextInjector {
    async fn inject(&mut self, text: &str) -> anyhow::Result<()> {
        MacOsTextInjector::inject_with_images(self, text, &[]).await?;
        Ok(())
    }

    async fn inject_with_images(&mut self, text: &str, images: &[PathBuf]) -> anyhow::Result<()> {
        MacOsTextInjector::inject_with_images(self, text, images).await?;
        Ok(())
    }

    async fn inject_with_image(
        &mut self,
        text: &str,
        image_path: Option<&Path>,
    ) -> anyhow::Result<()> {
        MacOsTextInjector::inject_with_image(self, text, image_path).await?;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "macos"
    }
}

/// Puts `text` on the clipboard via pbcopy.
fn copy_text(text: &str) -> anyhow::Result<()> {
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to run pbcopy")?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(text.as_bytes())?;
    let status = child.wait()?;
    anyhow::ensure!(status.success(), "pbcopy exited with {status}");
    Ok(())
}

/// Puts the PNG at `path` on the clipboard via osascript.
fn copy_image(path: &Path) -> anyhow::Result<()> {
    let script = build_set_image_applescript(path);
    let status = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to set clipboard image via osascript")?;
    anyhow::ensure!(
        status.success(),
        "osascript image copy exited with {status}"
    );
    Ok(())
}

/// Sends Cmd+V to the frontmost app.
fn send_paste_chord() -> anyhow::Result<()> {
    let status = Command::new("osascript")
        .arg("-e")
        .arg(APPLESCRIPT_PASTE_CHORD)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to run osascript")?;
    anyhow::ensure!(status.success(), "osascript exited with {status}");
    Ok(())
}

/// Waits up to 500 ms for a screenshot the capture thread may still be writing.
fn wait_for_file(path: &Path) -> bool {
    let start = Instant::now();
    while !path.exists() && start.elapsed() < Duration::from_millis(500) {
        std::thread::sleep(Duration::from_millis(25));
    }
    path.exists()
}

pub fn inject_macos_blocking(
    text: &str,
    images: &[PathBuf],
    restore_clipboard: bool,
    restore_delay_ms: u64,
    terminal_cfg: &TerminalPasteConfig,
) -> anyhow::Result<PasteOutcome> {
    let (bundle_id, app_name) = detect_frontmost_app_info();
    let is_terminal = is_terminal_target(bundle_id.as_deref(), app_name.as_deref(), terminal_cfg);
    let target = if is_terminal {
        Target::Terminal
    } else {
        Target::Other
    };

    let pastes = plan_pastes(text, images, target, terminal_cfg);
    if pastes.is_empty() {
        return Ok(PasteOutcome::Done);
    }

    let mut sm = restore::ClipboardStateMachine::new();
    if restore_clipboard {
        sm.record_snapshot(restore::snapshot_clipboard());
    }

    let initial_front = if pastes.len() > 1 {
        frontmost_app_token()
    } else {
        None
    };
    let settle = Duration::from_millis(if is_terminal {
        terminal_cfg.settle_ms
    } else {
        100
    });

    let mut last_cc: Option<i64> = None;
    let mut outcome = PasteOutcome::Done;
    for (index, paste) in pastes.iter().enumerate() {
        if index > 0 {
            // Let the app read the previous paste before the clipboard changes again.
            std::thread::sleep(settle);
            let guard = check_guard(
                initial_front.as_deref(),
                frontmost_app_token().as_deref(),
                last_cc,
                restore::get_clipboard_change_count(),
            );
            let reason = match guard {
                Guard::Continue => None,
                Guard::FocusChanged => Some(InterruptReason::FocusChanged),
                Guard::ClipboardChanged => Some(InterruptReason::ClipboardChanged),
            };
            if let Some(reason) = reason {
                outcome = PasteOutcome::Interrupted {
                    pasted: index,
                    total: pastes.len(),
                    reason,
                };
                break;
            }
        }

        let written = match paste {
            Paste::Text(s) => copy_text(s),
            Paste::ImagePath { text, path } => {
                // Past the wait the path is pasted anyway; it then stays plain text.
                wait_for_file(path);
                copy_text(text)
            }
            Paste::Image(path) => {
                if wait_for_file(path) {
                    copy_image(path)
                } else {
                    Err(anyhow::anyhow!(
                        "screenshot image not found at {}",
                        path.display()
                    ))
                }
            }
        };
        if let Err(e) = written {
            if matches!(paste, Paste::Image(_)) {
                eprintln!("[inject] screenshot image paste failed (soft failure): {e:#}");
                continue;
            }
            return Err(e);
        }

        let cc = restore::get_clipboard_change_count();
        sm.record_paste(cc);
        last_cc = cc;

        if let Err(e) = send_paste_chord() {
            if matches!(paste, Paste::Image(_)) {
                eprintln!("[inject] screenshot image paste failed (soft failure): {e:#}");
                continue;
            }
            return Err(e);
        }
    }

    eprintln!(
        "[inject] target={} app={:?} pastes={} settle_ms={} outcome={}",
        if is_terminal { "terminal" } else { "other" },
        bundle_id,
        pastes.len(),
        settle.as_millis(),
        match &outcome {
            PasteOutcome::Done => "done".to_string(),
            PasteOutcome::Interrupted { pasted, reason, .. } =>
                format!("interrupted({reason:?}) after {pasted}"),
        }
    );

    // Restore the original clipboard once, unless the user copied something new.
    let clipboard_changed = matches!(
        outcome,
        PasteOutcome::Interrupted {
            reason: InterruptReason::ClipboardChanged,
            ..
        }
    );
    if restore_clipboard && !clipboard_changed && sm.should_restore(last_cc) {
        std::thread::sleep(Duration::from_millis(restore_delay_ms));
        let curr_cc = restore::get_clipboard_change_count();
        if sm.should_restore(curr_cc) {
            if let Some(snap) = sm.snapshot() {
                restore::restore_clipboard(snap);
                sm.record_restored();
            }
        } else {
            sm.record_skipped();
        }
    }

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inject::restore::{ClipboardItem, ClipboardSnapshot, RestoreState};
    use std::sync::Mutex;

    static CLIPBOARD_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_is_terminal_app_detection() {
        assert!(is_terminal_app(Some("com.apple.Terminal"), None));
        assert!(is_terminal_app(Some("com.googlecode.iterm2"), None));
        assert!(is_terminal_app(Some("net.kovidgoyal.kitty"), None));
        assert!(is_terminal_app(Some("io.alacritty"), None));
        assert!(is_terminal_app(Some("com.mitchellh.ghostty"), None));
        assert!(is_terminal_app(Some("com.github.wez.wezterm"), None));
        assert!(is_terminal_app(Some("dev.warp.Warp-Stable"), None));
        assert!(is_terminal_app(None, Some("iTerm2")));
        assert!(is_terminal_app(None, Some("Ghostty")));
        assert!(is_terminal_app(None, Some("Terminal")));

        // Non-terminal apps
        assert!(!is_terminal_app(
            Some("com.google.Chrome"),
            Some("Google Chrome")
        ));
        assert!(!is_terminal_app(Some("com.apple.Safari"), Some("Safari")));
        assert!(!is_terminal_app(Some("com.openai.chat"), Some("ChatGPT")));
        assert!(!is_terminal_app(None, None));
    }

    #[test]
    fn test_format_image_paths_for_cli() {
        let empty: Vec<PathBuf> = Vec::new();
        assert_eq!(format_image_paths_for_cli(&empty), "");

        let single = vec![PathBuf::from("/tmp/session_1/context-1.png")];
        assert_eq!(
            format_image_paths_for_cli(&single),
            "\"/tmp/session_1/context-1.png\""
        );

        let multiple = vec![
            PathBuf::from("/tmp/session_1/context-1.png"),
            PathBuf::from("/tmp/session_1/context-2.png"),
        ];
        assert_eq!(
            format_image_paths_for_cli(&multiple),
            "\"/tmp/session_1/context-1.png\" \"/tmp/session_1/context-2.png\""
        );
    }

    #[test]
    fn test_select_last_session_image_selection() {
        // Case 1: No images captured
        let empty_images: Vec<PathBuf> = Vec::new();
        assert_eq!(select_last_session_image(&empty_images), None);

        // Case 2: Single image captured
        let single_image = vec![PathBuf::from("/tmp/session_1/context-1.png")];
        assert_eq!(
            select_last_session_image(&single_image),
            Some(Path::new("/tmp/session_1/context-1.png"))
        );

        // Case 3: Multiple images captured
        let multiple_images = vec![
            PathBuf::from("/tmp/session_1/context-1.png"),
            PathBuf::from("/tmp/session_1/context-2.png"),
            PathBuf::from("/tmp/session_1/context-3.png"),
        ];
        assert_eq!(
            select_last_session_image(&multiple_images),
            Some(Path::new("/tmp/session_1/context-3.png"))
        );
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names
            .iter()
            .map(|n| PathBuf::from(format!("/tmp/session/{n}")))
            .collect()
    }

    fn text_of(pastes: &[Paste]) -> String {
        pastes
            .iter()
            .map(|p| match p {
                Paste::Text(t) => t.as_str(),
                Paste::ImagePath { text, .. } => text.as_str(),
                Paste::Image(_) => "",
            })
            .collect()
    }

    fn long_dictation() -> String {
        let para = "This is a sentence that goes on for a while. ".repeat(6);
        [para.as_str(); 4].join("\n\n")
    }

    #[test]
    fn test_is_terminal_target_defaults() {
        let cfg = TerminalPasteConfig::default();
        for id in [
            "com.apple.Terminal",
            "com.googlecode.iterm2",
            "net.kovidgoyal.kitty",
            "io.alacritty",
            "org.alacritty",
            "com.mitchellh.ghostty",
            "com.github.wez.wezterm",
            "dev.warp.Warp-Stable",
            "co.zeit.hyper",
            "org.tabby",
            "com.termius-danilook.mac",
        ] {
            assert!(is_terminal_target(Some(id), None, &cfg), "{id}");
        }
        for (id, name) in [
            ("com.microsoft.VSCode", "Code"),
            ("com.todesktop.230313mzl4w4u92", "Cursor"),
            ("com.google.antigravity-ide", "Antigravity"),
            ("dev.zed.Zed", "Zed"),
            ("com.jetbrains.intellij", "IntelliJ IDEA"),
            ("com.google.Chrome", "Google Chrome"),
            ("com.tinyspeck.slackmacgap", "Slack"),
            ("com.anthropic.claudefordesktop", "Claude"),
        ] {
            assert!(!is_terminal_target(Some(id), Some(name), &cfg), "{id}");
        }
        assert!(!is_terminal_target(None, None, &cfg));
    }

    #[test]
    fn test_is_terminal_target_extra_and_exclude() {
        let extra = TerminalPasteConfig {
            extra_apps: vec!["com.microsoft.VSCode".to_string()],
            ..Default::default()
        };
        assert!(is_terminal_target(
            Some("com.microsoft.VSCode"),
            Some("Code"),
            &extra
        ));
        assert!(is_terminal_target(
            None,
            Some("Visual Studio Code"),
            &TerminalPasteConfig {
                extra_apps: vec!["studio code".to_string()],
                ..Default::default()
            }
        ));
        assert!(!is_terminal_target(
            Some("dev.zed.Zed"),
            Some("Zed"),
            &extra
        ));

        // exclude beats extra and the built-in list
        let both = TerminalPasteConfig {
            extra_apps: vec!["com.microsoft.VSCode".to_string()],
            exclude_apps: vec!["vscode".to_string(), "Warp".to_string()],
            ..Default::default()
        };
        assert!(!is_terminal_target(
            Some("com.microsoft.VSCode"),
            Some("Code"),
            &both
        ));
        assert!(!is_terminal_target(
            Some("dev.warp.Warp-Stable"),
            Some("Warp"),
            &both
        ));
        assert!(is_terminal_target(
            Some("com.github.wez.wezterm"),
            None,
            &both
        ));

        // blank patterns match nothing
        let blank = TerminalPasteConfig {
            extra_apps: vec!["  ".to_string()],
            ..Default::default()
        };
        assert!(!is_terminal_target(
            Some("com.google.Chrome"),
            Some("Chrome"),
            &blank
        ));
    }

    #[test]
    fn test_plan_other_matches_today() {
        let cfg = TerminalPasteConfig::default();
        let imgs = paths(&["context-1.png", "context-2.png"]);
        assert_eq!(
            plan_pastes("Hello world", &[], Target::Other, &cfg),
            vec![Paste::Text("Hello world".to_string())]
        );
        assert_eq!(
            plan_pastes("Hello world", &imgs, Target::Other, &cfg),
            vec![
                Paste::Text("Hello world".to_string()),
                Paste::Image(imgs[0].clone()),
                Paste::Image(imgs[1].clone()),
            ]
        );
        assert_eq!(
            plan_pastes("", &imgs, Target::Other, &cfg),
            vec![Paste::Image(imgs[0].clone()), Paste::Image(imgs[1].clone())]
        );
        // The GUI path never splits, however long the text is.
        let long = long_dictation();
        assert_eq!(
            plan_pastes(&long, &[], Target::Other, &cfg),
            vec![Paste::Text(long)]
        );
        assert!(plan_pastes("", &[], Target::Other, &cfg).is_empty());
    }

    #[test]
    fn test_plan_terminal_without_split_is_todays_single_string() {
        let cfg = TerminalPasteConfig {
            split_paste: false,
            ..Default::default()
        };
        let imgs = paths(&["context-1.png", "context-2.png"]);
        let long = long_dictation();
        assert_eq!(
            plan_pastes(&long, &[], Target::Terminal, &cfg),
            vec![Paste::Text(long.clone())]
        );
        assert_eq!(
            plan_pastes("Explain these", &imgs, Target::Terminal, &cfg),
            vec![Paste::Text(
                "Explain these \"/tmp/session/context-1.png\" \"/tmp/session/context-2.png\""
                    .to_string()
            )]
        );
        assert_eq!(
            plan_pastes("   ", &imgs[..1], Target::Terminal, &cfg),
            vec![Paste::Text("\"/tmp/session/context-1.png\"".to_string())]
        );
    }

    #[test]
    fn test_plan_terminal_split_text_only() {
        let cfg = TerminalPasteConfig::default();
        assert_eq!(
            plan_pastes("short dictation", &[], Target::Terminal, &cfg),
            vec![Paste::Text("short dictation".to_string())]
        );
        let long = long_dictation();
        let plan = plan_pastes(&long, &[], Target::Terminal, &cfg);
        assert!(plan.len() > 1);
        assert_eq!(text_of(&plan), long);
        let budget = cfg.budget();
        for paste in &plan {
            let Paste::Text(t) = paste else {
                panic!("text only")
            };
            assert!(t.encode_utf16().count() <= budget.max_units);
            assert!(t.matches('\n').count() <= budget.max_breaks);
            assert!(!t.ends_with('\n'));
        }
    }

    #[test]
    fn test_plan_terminal_split_with_screenshots_rejoins_to_todays_string() {
        let cfg = TerminalPasteConfig::default();
        let imgs = paths(&["context-1.png", "context-2.png"]);
        let long = long_dictation();
        for text in [
            "Explain what is wrong in these two screenshots.",
            long.as_str(),
        ] {
            let plan = plan_pastes(text, &imgs, Target::Terminal, &cfg);
            let today = match single_terminal_paste(text, &imgs) {
                Some(Paste::Text(t)) => t,
                _ => unreachable!(),
            };
            assert_eq!(text_of(&plan), today);
            let n = plan.len();
            assert_eq!(
                plan[n - 2],
                Paste::ImagePath {
                    text: "\"/tmp/session/context-1.png\"".to_string(),
                    path: imgs[0].clone()
                }
            );
            assert_eq!(
                plan[n - 1],
                Paste::ImagePath {
                    text: " \"/tmp/session/context-2.png\"".to_string(),
                    path: imgs[1].clone()
                }
            );
            assert!(plan[..n - 2].iter().all(|p| matches!(p, Paste::Text(_))));
        }
    }

    #[test]
    fn test_plan_terminal_blank_text_with_screenshots_pastes_only_paths() {
        let cfg = TerminalPasteConfig::default();
        let imgs = paths(&["context-1.png", "context-2.png"]);
        for text in ["", "  \n "] {
            let plan = plan_pastes(text, &imgs, Target::Terminal, &cfg);
            assert_eq!(
                plan,
                vec![
                    Paste::ImagePath {
                        text: "\"/tmp/session/context-1.png\"".to_string(),
                        path: imgs[0].clone()
                    },
                    Paste::ImagePath {
                        text: " \"/tmp/session/context-2.png\"".to_string(),
                        path: imgs[1].clone()
                    },
                ]
            );
        }
    }

    #[test]
    fn test_plan_terminal_empty_and_whitespace_only() {
        let cfg = TerminalPasteConfig::default();
        assert!(plan_pastes("", &[], Target::Terminal, &cfg).is_empty());
        let off = TerminalPasteConfig {
            split_paste: false,
            ..Default::default()
        };
        assert!(plan_pastes("", &[], Target::Terminal, &off).is_empty());
        // Whitespace-only text without screenshots is pasted as is, like today.
        assert_eq!(
            plan_pastes("  ", &[], Target::Terminal, &cfg),
            vec![Paste::Text("  ".to_string())]
        );
    }

    #[test]
    fn test_plan_terminal_max_pieces_falls_back_to_one_paste() {
        let cfg = TerminalPasteConfig {
            max_pieces: 3,
            ..Default::default()
        };
        let imgs = paths(&["context-1.png"]);
        let long = "word ".repeat(1000); // 5000 units, 7 pieces
        assert!(split_for_terminal(&long, cfg.budget()).len() > 3);
        assert_eq!(
            plan_pastes(&long, &[], Target::Terminal, &cfg),
            vec![Paste::Text(long.clone())]
        );
        assert_eq!(
            plan_pastes(&long, &imgs, Target::Terminal, &cfg),
            vec![Paste::Text(format!(
                "{long} \"/tmp/session/context-1.png\""
            ))]
        );
        // Exactly max_pieces still splits.
        let at_limit = TerminalPasteConfig {
            max_pieces: split_for_terminal(&long, cfg.budget()).len(),
            ..Default::default()
        };
        assert!(plan_pastes(&long, &[], Target::Terminal, &at_limit).len() > 1);
    }

    #[test]
    fn test_check_guard() {
        // Same app, same clipboard
        assert_eq!(
            check_guard(Some("ASN:1"), Some("ASN:1"), Some(5), Some(5)),
            Guard::Continue
        );
        // Focus moved
        assert_eq!(
            check_guard(Some("ASN:1"), Some("ASN:2"), Some(5), Some(5)),
            Guard::FocusChanged
        );
        // Someone copied
        assert_eq!(
            check_guard(Some("ASN:1"), Some("ASN:1"), Some(5), Some(6)),
            Guard::ClipboardChanged
        );
        // Both: the user's copy wins, so nothing is restored over it
        assert_eq!(
            check_guard(Some("ASN:1"), Some("ASN:2"), Some(5), Some(6)),
            Guard::ClipboardChanged
        );
        // Unknown values never stop a paste
        assert_eq!(
            check_guard(None, Some("ASN:2"), None, None),
            Guard::Continue
        );
        assert_eq!(
            check_guard(Some("ASN:1"), None, Some(5), None),
            Guard::Continue
        );
    }

    #[test]
    fn test_paste_outcome_notices() {
        assert_eq!(PasteOutcome::Done.notice(), None);
        let focus = PasteOutcome::Interrupted {
            pasted: 3,
            total: 9,
            reason: InterruptReason::FocusChanged,
        };
        assert_eq!(
            focus.notice().unwrap(),
            "Focus changed - pasted 3 of 9 parts. Alt+I inserts the whole dictation again."
        );
        let clip = PasteOutcome::Interrupted {
            pasted: 2,
            total: 4,
            reason: InterruptReason::ClipboardChanged,
        };
        assert_eq!(
            clip.notice().unwrap(),
            "Clipboard changed - pasted 2 of 4 parts and kept your new copy. Alt+I inserts the whole dictation again."
        );
    }

    #[test]
    fn test_build_set_image_applescript_escaping() {
        let path = Path::new("/tmp/sessions/session_123/context-1.png");
        let script = build_set_image_applescript(path);
        assert!(script.contains("set the clipboard to (read (POSIX file \"/tmp/sessions/session_123/context-1.png\") as «class PNGf»)"));

        // Test escaping quotes and backslashes
        let path_with_quotes = Path::new("/tmp/test \"folder\"/image\\1.png");
        let escaped_script = build_set_image_applescript(path_with_quotes);
        assert!(escaped_script.contains(r#"/tmp/test \"folder\"/image\\1.png"#));
    }

    #[test]
    fn test_dual_paste_clipboard_state_machine_transitions() {
        let mut sm = restore::ClipboardStateMachine::new();
        assert_eq!(sm.state(), RestoreState::Idle);

        let initial_snapshot = ClipboardSnapshot {
            items: vec![ClipboardItem {
                mime_type: "public.utf8-plain-text".to_string(),
                data: b"Original user text".to_vec(),
            }],
            change_count: Some(50),
        };
        sm.record_snapshot(Some(initial_snapshot));
        assert_eq!(sm.state(), RestoreState::Snapshotted);

        // 1. Text paste sets clipboard -> change count becomes 51
        sm.record_paste(Some(51));
        assert_eq!(
            sm.state(),
            RestoreState::Pasted {
                expected_change_count: Some(51)
            }
        );

        // 2. Image paste 1 sets clipboard -> change count becomes 52
        sm.record_paste(Some(52));
        assert_eq!(
            sm.state(),
            RestoreState::Pasted {
                expected_change_count: Some(52)
            }
        );

        // 3. Image paste 2 sets clipboard -> change count becomes 53
        sm.record_paste(Some(53));
        assert_eq!(
            sm.state(),
            RestoreState::Pasted {
                expected_change_count: Some(53)
            }
        );

        // Verify restoration criteria:
        // - Matching the final image paste count (53) triggers restore
        assert!(sm.should_restore(Some(53)));
        // - Earlier paste counts (51, 52) do not restore
        assert!(!sm.should_restore(Some(51)));
        assert!(!sm.should_restore(Some(52)));
        // - Modified count (e.g. user copied text 54 during restore delay) prevents restore
        assert!(!sm.should_restore(Some(54)));

        // Complete restore
        sm.record_restored();
        assert_eq!(sm.state(), RestoreState::Restored);
    }

    #[test]
    #[ignore = "sends a real Cmd+V to the frontmost app and replaces the clipboard"]
    fn test_inject_macos_blocking_soft_failure_on_missing_image() {
        let _guard = CLIPBOARD_TEST_LOCK.lock().unwrap();
        // When image path does not exist, text paste still succeeds and function returns Ok(())
        let nonexistent = PathBuf::from("/tmp/nonexistent_screenshot_path_12345.png");
        let result = inject_macos_blocking(
            "Test text for soft failure",
            &[nonexistent],
            false,
            10,
            &TerminalPasteConfig::default(),
        );
        assert!(
            result.is_ok(),
            "Expected soft failure to return Ok(()), got: {:?}",
            result
        );
    }

    #[test]
    #[ignore = "sends a real Cmd+V to the frontmost app and replaces the clipboard"]
    fn test_inject_macos_blocking_with_real_image_and_restore() {
        let _guard = CLIPBOARD_TEST_LOCK.lock().unwrap();
        use std::io::Write;

        // Create a real minimal valid 1x1 PNG file in temp dir
        let temp_png = std::env::temp_dir().join(format!(
            "bolo_test_img_{}_{}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let png_bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x06\x00\x00\x00\x1f\x15c4\x00\x00\x00\nIDATx\x9cc\x00\x01\x00\x00\x05\x00\x01\r\n-\xb4\x00\x00\x00\x00IEND\xaeB`\x82";
        std::fs::write(&temp_png, png_bytes).expect("write png");

        // Seed clipboard with known initial content
        let initial_text = format!("initial_user_clipboard_{}", std::process::id());
        let mut child = Command::new("pbcopy")
            .stdin(Stdio::piped())
            .spawn()
            .expect("pbcopy");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(initial_text.as_bytes())
            .unwrap();
        child.wait().unwrap();

        // Perform injection with image and restore enabled
        let res = inject_macos_blocking(
            "Hello with screenshot",
            std::slice::from_ref(&temp_png),
            true,
            50,
            &TerminalPasteConfig::default(),
        );
        assert!(res.is_ok(), "Injection with image failed: {:?}", res);

        // Sleep slightly to let the restore thread complete
        std::thread::sleep(Duration::from_millis(150));

        // Read clipboard via pbpaste and verify initial content was restored
        let output = Command::new("pbpaste").output().expect("pbpaste");
        let pasted = String::from_utf8_lossy(&output.stdout);
        assert_eq!(pasted, initial_text);

        let _ = std::fs::remove_file(&temp_png);
    }

    #[test]
    #[ignore = "sends a real Cmd+V to the frontmost app and replaces the clipboard"]
    fn test_inject_macos_blocking_with_multiple_images_and_restore() {
        let _guard = CLIPBOARD_TEST_LOCK.lock().unwrap();
        use std::io::Write;

        let temp_png1 = std::env::temp_dir().join(format!(
            "bolo_test_img1_{}_{}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let temp_png2 = std::env::temp_dir().join(format!(
            "bolo_test_img2_{}_{}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let png_bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x06\x00\x00\x00\x1f\x15c4\x00\x00\x00\nIDATx\x9cc\x00\x01\x00\x00\x05\x00\x01\r\n-\xb4\x00\x00\x00\x00IEND\xaeB`\x82";
        std::fs::write(&temp_png1, png_bytes).expect("write png1");
        std::fs::write(&temp_png2, png_bytes).expect("write png2");

        // Seed clipboard with known initial content
        let initial_text = format!("initial_user_clipboard_multi_{}", std::process::id());
        let mut child = Command::new("pbcopy")
            .stdin(Stdio::piped())
            .spawn()
            .expect("pbcopy");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(initial_text.as_bytes())
            .unwrap();
        child.wait().unwrap();

        // Perform injection with multiple images and restore enabled
        let res = inject_macos_blocking(
            "Hello with multiple screenshots",
            &[temp_png1.clone(), temp_png2.clone()],
            true,
            50,
            &TerminalPasteConfig::default(),
        );
        assert!(
            res.is_ok(),
            "Injection with multiple images failed: {:?}",
            res
        );

        std::thread::sleep(Duration::from_millis(150));

        let output = Command::new("pbpaste").output().expect("pbpaste");
        let pasted = String::from_utf8_lossy(&output.stdout);
        assert_eq!(pasted, initial_text);

        let _ = std::fs::remove_file(&temp_png1);
        let _ = std::fs::remove_file(&temp_png2);
    }
}
