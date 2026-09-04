use crate::vision::gesture::CircleGesture;
use anyhow::{anyhow, Result};
use std::path::Path;

/// Capture display screenshot on Linux via XDG Portal Screenshot API with graceful fallback.
#[allow(dead_code)]
pub fn capture_screen(gesture: CircleGesture, output_path: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use tokio::runtime::Handle;
        let rt = Handle::try_current();
        let fut = capture_screen_async(gesture, output_path);
        if let Ok(handle) = rt {
            tokio::task::block_in_place(|| handle.block_on(fut))
        } else {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(fut)
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (gesture, output_path);
        Err(anyhow!("Linux portal screenshot unavailable on non-Linux platform"))
    }
}

#[cfg(target_os = "linux")]
async fn capture_screen_async(_gesture: CircleGesture, output_path: &Path) -> Result<()> {
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use ashpd::desktop::screenshot::Screenshot;
    let proxy = Screenshot::new().await?;
    let response = proxy.screenshot(true, true).await?.response()?;
    let uri = response.uri();
    let src_path = uri.to_file_path().map_err(|_| anyhow!("invalid screenshot URI path"))?;
    std::fs::copy(&src_path, output_path)?;
    Ok(())
}

/// Helper for unit/mock testing Linux capture behavior.
pub fn capture_screen_mock(_gesture: CircleGesture, output_path: &Path) -> Result<()> {
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Generate a minimal valid 1x1 PNG file for unit tests
    let minimal_png: [u8; 67] = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    std::fs::write(output_path, minimal_png)?;
    Ok(())
}
