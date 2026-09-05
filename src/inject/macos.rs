use super::{restore, TextInjector};
use crate::config::InjectConfig;
use anyhow::Context;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

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
}

#[async_trait::async_trait]
impl TextInjector for MacOsTextInjector {
    async fn inject(&mut self, text: &str) -> anyhow::Result<()> {
        let text = text.to_owned();
        let restore_clipboard = self.restore_clipboard;
        let restore_delay_ms = self.restore_delay_ms;

        tokio::task::spawn_blocking(move || {
            let mut sm = restore::ClipboardStateMachine::new();

            if restore_clipboard {
                let snap = restore::snapshot_clipboard();
                sm.record_snapshot(snap);
            }

            // Put text into clipboard via pbcopy
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
            sm.record_paste(post_cc);

            // Trigger Cmd+V using osascript (AppleScript)
            let applescript = r#"
                tell application "System Events"
                    keystroke "v" using command down
                end tell
            "#;
            let status = Command::new("osascript")
                .arg("-e")
                .arg(applescript)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("failed to run osascript")?;
            anyhow::ensure!(status.success(), "osascript exited with {status}");

            if restore_clipboard && sm.should_restore(post_cc) {
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

            Ok::<(), anyhow::Error>(())
        })
        .await??;

        Ok(())
    }

    fn name(&self) -> &'static str {
        "macos"
    }
}
