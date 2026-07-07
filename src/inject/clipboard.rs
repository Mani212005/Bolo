use super::TextInjector;
use anyhow::Context;
use std::io::Write;
use std::process::{Command, Stdio};

/// Fallback injector: put the transcript on the Wayland clipboard via
/// wl-copy (which forks and serves the selection itself), and let the user
/// paste. Used when the portal fails or when configured directly.
///
/// Uses std::process + spawn_blocking rather than tokio::process: tokio's
/// SIGCHLD-driven reaping proved unreliable inside the daemon (observed
/// 16s stalls waiting on an already-exited wl-copy), while a plain waitpid
/// returns in ~50ms.
pub struct ClipboardInjector;

#[async_trait::async_trait]
impl TextInjector for ClipboardInjector {
    async fn inject(&mut self, text: &str) -> anyhow::Result<()> {
        let text = text.to_owned();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut child = Command::new("wl-copy")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("failed to run wl-copy (is wl-clipboard installed?)")?;
            child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(text.as_bytes())?;
            let status = child.wait()?;
            anyhow::ensure!(status.success(), "wl-copy exited with {status}");
            Ok(())
        })
        .await?
    }

    fn name(&self) -> &'static str {
        "clipboard"
    }
}
