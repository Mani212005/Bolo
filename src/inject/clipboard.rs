use super::TextInjector;
use crate::config::InjectConfig;
use anyhow::Context;
use std::io::Write;
use std::process::{Command, Stdio};

pub struct ClipboardInjector;

impl ClipboardInjector {
    pub fn new(_cfg: &InjectConfig) -> Self {
        Self
    }
}

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
        tokio::task::spawn_blocking(move || {
            set_clipboard(&text)
        })
        .await??;

        Ok(())
    }

    fn name(&self) -> &'static str {
        "clipboard"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clipboard_injector_name() {
        let injector = ClipboardInjector;
        assert_eq!(injector.name(), "clipboard");
    }
}
