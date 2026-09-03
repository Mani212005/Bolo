use super::{restore, TextInjector};
use crate::config::InjectConfig;
use anyhow::Context;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

pub struct ClipboardInjector {
    restore_clipboard: bool,
    restore_delay_ms: u64,
}

impl ClipboardInjector {
    pub fn new(cfg: &InjectConfig) -> Self {
        Self {
            restore_clipboard: cfg.restore_clipboard,
            restore_delay_ms: cfg.restore_delay_ms,
        }
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
        let restore_clipboard = self.restore_clipboard;
        let restore_delay_ms = self.restore_delay_ms;

        tokio::task::spawn_blocking(move || {
            let mut sm = restore::ClipboardStateMachine::new();

            if restore_clipboard {
                let snap = restore::snapshot_clipboard();
                sm.record_snapshot(snap);
            }

            set_clipboard(&text)?;

            let post_cc = restore::get_clipboard_change_count();
            sm.record_paste(post_cc);

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
        "clipboard"
    }
}
