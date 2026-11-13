#[cfg(target_os = "linux")]
pub mod clipboard;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "linux")]
pub mod portal;
pub mod restore;
pub mod split;

/// Formats a list of image paths as space-separated quoted strings for CLI / terminal insertion.
pub fn format_image_paths_for_cli(images: &[std::path::PathBuf]) -> String {
    images
        .iter()
        .map(|p| format!("\"{}\"", p.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `text` followed by the quoted image paths, the way a terminal agent
/// (Claude Code) turns a pasted path into an attached image.
#[allow(dead_code)] // used on Linux; macOS plans its pastes in inject::macos
pub fn append_image_paths(text: &str, images: &[std::path::PathBuf]) -> String {
    if images.is_empty() {
        text.to_string()
    } else if text.trim().is_empty() {
        format_image_paths_for_cli(images)
    } else {
        format!("{text} {}", format_image_paths_for_cli(images))
    }
}

#[async_trait::async_trait]
pub trait TextInjector: Send + Sync {
    /// Deliver `text` to the user's focused application.
    #[allow(dead_code)]
    async fn inject(&mut self, text: &str) -> anyhow::Result<()>;
    /// Deliver `text` and optionally accompanying images to the user's focused application.
    #[allow(dead_code)]
    async fn inject_with_images(
        &mut self,
        text: &str,
        images: &[std::path::PathBuf],
    ) -> anyhow::Result<()> {
        let _ = images;
        self.inject(text).await
    }
    /// Deliver `text` and optionally an accompanying image to the user's focused application.
    #[allow(dead_code)]
    async fn inject_with_image(
        &mut self,
        text: &str,
        image_path: Option<&std::path::Path>,
    ) -> anyhow::Result<()> {
        let images = image_path
            .map(|p| vec![p.to_path_buf()])
            .unwrap_or_default();
        self.inject_with_images(text, &images).await
    }
    /// Human-readable name for logs/notifications.
    #[allow(dead_code)]
    fn name(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn image_paths_follow_the_text_quoted() {
        let images = vec![
            PathBuf::from("/s/context-1.png"),
            PathBuf::from("/s/context-1-screen.png"),
        ];
        assert_eq!(append_image_paths("fix this", &[]), "fix this");
        assert_eq!(
            append_image_paths("fix this", &images),
            r#"fix this "/s/context-1.png" "/s/context-1-screen.png""#
        );
        assert_eq!(
            append_image_paths("  ", &images[..1]),
            r#""/s/context-1.png""#
        );
    }
}
