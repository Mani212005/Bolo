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

/// Selects the last captured screenshot image from a session image list.
pub fn select_last_session_image(images: &[PathBuf]) -> Option<&Path> {
    images.last().map(|p| p.as_path())
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

    pub async fn inject_with_image(
        &mut self,
        text: &str,
        image_path: Option<&Path>,
    ) -> anyhow::Result<()> {
        let text = text.to_owned();
        let image_path = image_path.map(|p| p.to_path_buf());
        let restore_clipboard = self.restore_clipboard;
        let restore_delay_ms = self.restore_delay_ms;

        tokio::task::spawn_blocking(move || {
            inject_macos_blocking(
                &text,
                image_path.as_deref(),
                restore_clipboard,
                restore_delay_ms,
            )
        })
        .await??;

        Ok(())
    }
}

#[async_trait::async_trait]
impl TextInjector for MacOsTextInjector {
    async fn inject(&mut self, text: &str) -> anyhow::Result<()> {
        self.inject_with_image(text, None).await
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
    image_path: Option<&Path>,
    restore_clipboard: bool,
    restore_delay_ms: u64,
) -> anyhow::Result<()> {
    let mut sm = restore::ClipboardStateMachine::new();

    if restore_clipboard {
        let snap = restore::snapshot_clipboard();
        sm.record_snapshot(snap);
    }

    // 1. Put text into clipboard via pbcopy
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

    let mut last_post_cc = restore::get_clipboard_change_count();
    sm.record_paste(last_post_cc);

    // Trigger Cmd+V using osascript (AppleScript) for text
    let status = Command::new("osascript")
        .arg("-e")
        .arg(APPLESCRIPT_PASTE_CHORD)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to run osascript")?;
    anyhow::ensure!(status.success(), "osascript exited with {status}");

    // 2. If an image was captured in this session, do a second paste with the last image
    if let Some(img_path) = image_path {
        // Short pause between text paste and image paste so target application processes text paste
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

    // 3. Restore pre-existing clipboard after BOTH pastes have completed
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteStep {
    CopyText(String),
    TriggerPasteChord,
    CopyImage(PathBuf),
    RestoreOriginalClipboard,
}

/// Plans the sequential steps required for injecting text and optional screenshot.
pub fn plan_paste_sequence(
    text: &str,
    image_path: Option<&Path>,
    restore_clipboard: bool,
) -> Vec<PasteStep> {
    let mut steps = Vec::new();
    steps.push(PasteStep::CopyText(text.to_string()));
    steps.push(PasteStep::TriggerPasteChord);

    if let Some(img) = image_path {
        steps.push(PasteStep::CopyImage(img.to_path_buf()));
        steps.push(PasteStep::TriggerPasteChord);
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

        // Case 3: Multiple images captured (must select only the last one)
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
        // 1. Session with no image
        let steps_no_img = plan_paste_sequence("Hello world", None, true);
        assert_eq!(
            steps_no_img,
            vec![
                PasteStep::CopyText("Hello world".to_string()),
                PasteStep::TriggerPasteChord,
                PasteStep::RestoreOriginalClipboard,
            ]
        );

        // 2. Session with single image
        let img = Path::new("/tmp/session/context-1.png");
        let steps_single_img = plan_paste_sequence("Hello world", Some(img), true);
        assert_eq!(
            steps_single_img,
            vec![
                PasteStep::CopyText("Hello world".to_string()),
                PasteStep::TriggerPasteChord,
                PasteStep::CopyImage(img.to_path_buf()),
                PasteStep::TriggerPasteChord,
                PasteStep::RestoreOriginalClipboard,
            ]
        );

        // 3. Session with multiple images -> selected last image
        let all_images = vec![
            PathBuf::from("/tmp/session/context-1.png"),
            PathBuf::from("/tmp/session/context-2.png"),
        ];
        let chosen_img = select_last_session_image(&all_images);
        assert_eq!(chosen_img, Some(Path::new("/tmp/session/context-2.png")));

        let steps_multi = plan_paste_sequence("Query with screenshot", chosen_img, false);
        assert_eq!(
            steps_multi,
            vec![
                PasteStep::CopyText("Query with screenshot".to_string()),
                PasteStep::TriggerPasteChord,
                PasteStep::CopyImage(PathBuf::from("/tmp/session/context-2.png")),
                PasteStep::TriggerPasteChord,
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

        // 2. Image paste sets clipboard -> change count becomes 52
        sm.record_paste(Some(52));
        assert_eq!(
            sm.state(),
            RestoreState::Pasted {
                expected_change_count: Some(52)
            }
        );

        // Verify restoration criteria:
        // - Matching the final image paste count (52) triggers restore
        assert!(sm.should_restore(Some(52)));
        // - Stale text paste count (51) does not restore
        assert!(!sm.should_restore(Some(51)));
        // - Modified count (e.g. user copied text 53 during restore delay) prevents restore
        assert!(!sm.should_restore(Some(53)));

        // Complete restore
        sm.record_restored();
        assert_eq!(sm.state(), RestoreState::Restored);
    }
}
