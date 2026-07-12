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

/// Pipe `text` into a clipboard-setter command, bounded by coreutils
/// `timeout` (exit 124). On GNOME, wl-copy must briefly focus an invisible
/// window; focus-stealing prevention can block that forever while the user
/// is typing — hence the bound and the xclip fallback below.
fn pipe_to(cmd: &[&str], secs: &str, text: &str) -> anyhow::Result<()> {
    let mut child = Command::new("timeout")
        .arg(secs)
        .args(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to run {}", cmd[0]))?;
    child.stdin.take().expect("piped stdin").write_all(text.as_bytes())?;
    let status = child.wait()?;
    if status.code() == Some(124) {
        anyhow::bail!("{} timed out (focus-stealing prevention?)", cmd[0]);
    }
    anyhow::ensure!(status.success(), "{} exited with {status}", cmd[0]);
    Ok(())
}

pub fn set_clipboard(text: &str) -> anyhow::Result<()> {
    match pipe_to(&["wl-copy"], "3", text) {
        Ok(()) => Ok(()),
        Err(wl_err) => {
            // XWayland bridge: mutter syncs the X clipboard to Wayland
            // without needing focus.
            pipe_to(&["xclip", "-selection", "clipboard"], "2", text)
                .map(|()| eprintln!("[clipboard] wl-copy stalled ({wl_err}); used xclip bridge"))
                .map_err(|x_err| anyhow::anyhow!("wl-copy: {wl_err}; xclip: {x_err}"))
        }
    }
}

#[async_trait::async_trait]
impl TextInjector for ClipboardInjector {
    async fn inject(&mut self, text: &str) -> anyhow::Result<()> {
        let text = text.to_owned();
        tokio::task::spawn_blocking(move || set_clipboard(&text)).await?
    }

    fn name(&self) -> &'static str {
        "clipboard"
    }
}
