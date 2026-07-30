use super::TextInjector;
use anyhow::Context;
use std::io::Write;
use std::process::{Command, Stdio};

pub struct MacOsTextInjector;

impl MacOsTextInjector {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl TextInjector for MacOsTextInjector {
    async fn inject(&mut self, text: &str) -> anyhow::Result<()> {
        let text = text.to_owned();
        tokio::task::spawn_blocking(move || {
            // Put text into clipboard via pbcopy
            let mut child = Command::new("pbcopy")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("failed to run pbcopy")?;
            child.stdin.take().expect("piped stdin").write_all(text.as_bytes())?;
            let status = child.wait()?;
            anyhow::ensure!(status.success(), "pbcopy exited with {status}");
            
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
            
            Ok::<(), anyhow::Error>(())
        }).await??;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "macos"
    }
}
