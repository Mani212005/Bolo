#[cfg(target_os = "linux")]
pub mod clipboard;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "linux")]
pub mod portal;

#[async_trait::async_trait]
pub trait TextInjector: Send + Sync {
    /// Deliver `text` to the user's focused application.
    async fn inject(&mut self, text: &str) -> anyhow::Result<()>;
    /// Human-readable name for logs/notifications.
    fn name(&self) -> &'static str;
}
