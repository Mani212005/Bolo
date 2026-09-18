use super::{restore, TextInjector};
use crate::config::InjectConfig;
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

pub struct MacOsTextInjector {
    restore_clipboard: bool,
    restore_delay_ms: u64,
}

impl MacOsTextInjector {
    pub fn new(cfg: &InjectConfig) -> Self {
        Self {
            restore_clipboard: cfg.restore_clipboard,
            restore_delay_ms: cfg.restore_delay_ms,
        }
    }

    pub async fn inject_with_images(
        &mut self,
        text: &str,
        images: &[PathBuf],
    ) -> anyhow::Result<()> {
        let text = text.to_owned();
        let images = images.to_vec();
        let restore_clipboard = self.restore_clipboard;
        let restore_delay_ms = self.restore_delay_ms;

        tokio::task::spawn_blocking(move || {
            inject_macos_blocking(&text, &images, restore_clipboard, restore_delay_ms)
        })
        .await??;

        Ok(())
    }

    pub async fn inject_with_image(
        &mut self,
        text: &str,
        image_path: Option<&Path>,
    ) -> anyhow::Result<()> {
        let images = image_path
            .map(|p| vec![p.to_path_buf()])
            .unwrap_or_default();
        self.inject_with_images(text, &images).await
    }
}

#[async_trait::async_trait]
impl TextInjector for MacOsTextInjector {
    async fn inject(&mut self, text: &str) -> anyhow::Result<()> {
        self.inject_with_images(text, &[]).await
    }

    async fn inject_with_images(&mut self, text: &str, images: &[PathBuf]) -> anyhow::Result<()> {
        self.inject_with_images(text, images).await
    }

    async fn inject_with_image(
        &mut self,
        text: &str,
        image_path: Option<&Path>,
    ) -> anyhow::Result<()> {
        self.inject_with_image(text, image_path).await
    }

    fn name(&self) -> &'static str {
        "macos"
    }
}

pub fn inject_macos_blocking(
    text: &str,
    images: &[PathBuf],
    restore_clipboard: bool,
    restore_delay_ms: u64,
) -> anyhow::Result<()> {
    let mut sm = restore::ClipboardStateMachine::new();

    if restore_clipboard {
        let snap = restore::snapshot_clipboard();
        sm.record_snapshot(snap);
    }

    let (bundle_id, app_name) = detect_frontmost_app_info();
    let is_terminal = is_terminal_app(bundle_id.as_deref(), app_name.as_deref());

    if is_terminal && !images.is_empty() {
        // Terminal / CLI fallback: format quoted paths directly into prompt text stream
        let cli_paths = format_image_paths_for_cli(images);
        let full_text = if text.trim().is_empty() {
            cli_paths
        } else {
            format!("{text} {cli_paths}")
        };

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
            .write_all(full_text.as_bytes())?;
        let status = child.wait()?;
        anyhow::ensure!(status.success(), "pbcopy exited with {status}");

        let last_post_cc = restore::get_clipboard_change_count();
        sm.record_paste(last_post_cc);

        let status = Command::new("osascript")
            .arg("-e")
            .arg(APPLESCRIPT_PASTE_CHORD)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("failed to run osascript")?;
        anyhow::ensure!(status.success(), "osascript exited with {status}");
        eprintln!(
            "[inject] terminal app detected ({:?}) -> injected {} image path(s)",
            bundle_id,
            images.len()
        );

        if restore_clipboard && sm.should_restore(last_post_cc) {
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

        return Ok(());
    }

    // 1. Text paste via pbcopy
    let mut last_post_cc = None;
    if !text.is_empty() {
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

        let post_cc = restore::get_clipboard_change_count();
        last_post_cc = post_cc;
        sm.record_paste(post_cc);

        // Trigger Cmd+V using osascript (AppleScript) for text
        let status = Command::new("osascript")
            .arg("-e")
            .arg(APPLESCRIPT_PASTE_CHORD)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("failed to run osascript")?;
        anyhow::ensure!(status.success(), "osascript exited with {status}");
    }

    // 2. Sequential image pastes for all captured session screenshots
    for img_path in images {
        // Settle delay between pastes so target application processes the preceding paste
        std::thread::sleep(Duration::from_millis(100));

        let image_paste_result = (|| -> anyhow::Result<()> {
            let start = Instant::now();
            while !img_path.exists() && start.elapsed() < Duration::from_millis(500) {
                std::thread::sleep(Duration::from_millis(25));
            }
            anyhow::ensure!(
                img_path.exists(),
                "screenshot image not found at {}",
                img_path.display()
            );

            let script = build_set_image_applescript(img_path);
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

            let img_post_cc = restore::get_clipboard_change_count();
            last_post_cc = img_post_cc;
            sm.record_paste(img_post_cc);

            let status = Command::new("osascript")
                .arg("-e")
                .arg(APPLESCRIPT_PASTE_CHORD)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("failed to paste image via osascript")?;
            anyhow::ensure!(
                status.success(),
                "osascript image paste exited with {status}"
            );
            eprintln!("[inject] pasted screenshot image: {}", img_path.display());
            Ok(())
        })();

        if let Err(e) = image_paste_result {
            eprintln!("[inject] screenshot image paste failed (soft failure): {e:#}");
        }
    }

    // 3. Restore pre-existing clipboard after all pastes have completed
    if restore_clipboard && sm.should_restore(last_post_cc) {
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

    Ok(())
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteStep {
    CopyText(String),
    TriggerPasteChord,
    CopyImage(PathBuf),
    RestoreOriginalClipboard,
}

/// Plans the sequential steps required for injecting text and screenshots.
#[allow(dead_code)]
pub fn plan_paste_sequence(
    text: &str,
    images: &[PathBuf],
    restore_clipboard: bool,
    is_terminal: bool,
) -> Vec<PasteStep> {
    let mut steps = Vec::new();

    if is_terminal {
        let text_to_inject = if images.is_empty() {
            text.to_string()
        } else if text.trim().is_empty() {
            format_image_paths_for_cli(images)
        } else {
            format!("{text} {}", format_image_paths_for_cli(images))
        };
        if !text_to_inject.is_empty() {
            steps.push(PasteStep::CopyText(text_to_inject));
            steps.push(PasteStep::TriggerPasteChord);
        }
    } else {
        if !text.is_empty() {
            steps.push(PasteStep::CopyText(text.to_string()));
            steps.push(PasteStep::TriggerPasteChord);
        }
        for img in images {
            steps.push(PasteStep::CopyImage(img.clone()));
            steps.push(PasteStep::TriggerPasteChord);
        }
    }

    if restore_clipboard {
        steps.push(PasteStep::RestoreOriginalClipboard);
    }

    steps
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

    #[test]
    fn test_plan_paste_sequence_behavior() {
        // 1. GUI session with no image
        let steps_no_img = plan_paste_sequence("Hello world", &[], true, false);
        assert_eq!(
            steps_no_img,
            vec![
                PasteStep::CopyText("Hello world".to_string()),
                PasteStep::TriggerPasteChord,
                PasteStep::RestoreOriginalClipboard,
            ]
        );

        // 2. GUI session with multiple images
        let img1 = PathBuf::from("/tmp/session/context-1.png");
        let img2 = PathBuf::from("/tmp/session/context-2.png");
        let steps_multi_img =
            plan_paste_sequence("Hello world", &[img1.clone(), img2.clone()], true, false);
        assert_eq!(
            steps_multi_img,
            vec![
                PasteStep::CopyText("Hello world".to_string()),
                PasteStep::TriggerPasteChord,
                PasteStep::CopyImage(img1.clone()),
                PasteStep::TriggerPasteChord,
                PasteStep::CopyImage(img2.clone()),
                PasteStep::TriggerPasteChord,
                PasteStep::RestoreOriginalClipboard,
            ]
        );

        // 3. Terminal session with multiple images -> formatted directly into text stream
        let steps_terminal =
            plan_paste_sequence("Explain these screenshots", &[img1, img2], true, true);
        assert_eq!(
            steps_terminal,
            vec![
                PasteStep::CopyText(
                    "Explain these screenshots \"/tmp/session/context-1.png\" \"/tmp/session/context-2.png\""
                        .to_string()
                ),
                PasteStep::TriggerPasteChord,
                PasteStep::RestoreOriginalClipboard,
            ]
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
    fn test_inject_macos_blocking_soft_failure_on_missing_image() {
        let _guard = CLIPBOARD_TEST_LOCK.lock().unwrap();
        // When image path does not exist, text paste still succeeds and function returns Ok(())
        let nonexistent = PathBuf::from("/tmp/nonexistent_screenshot_path_12345.png");
        let result = inject_macos_blocking("Test text for soft failure", &[nonexistent], false, 10);
        assert!(
            result.is_ok(),
            "Expected soft failure to return Ok(()), got: {:?}",
            result
        );
    }

    #[test]
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
        let res = inject_macos_blocking("Hello with screenshot", &[temp_png.clone()], true, 50);
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
