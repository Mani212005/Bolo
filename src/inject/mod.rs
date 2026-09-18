#[cfg(target_os = "linux")]
pub mod clipboard;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "linux")]
pub mod portal;
pub mod restore;

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
