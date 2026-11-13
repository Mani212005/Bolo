//! Linux screen capture through the XDG Screenshot portal.
//!
//! GNOME on Wayland gives an unprivileged app no global pointer position, so
//! Bolo cannot know what the user points at. Instead it asks: the portal's
//! interactive dialog lets the user select the area. Just before that, a
//! non-interactive capture of the whole display provides the context image
//! and, by finding the selected pixels in it, where to outline the region.
//! If that capture is refused, the selected area is still sent on its own.

use super::shot::{self, Budget, Shot};
#[cfg(target_os = "linux")]
use anyhow::anyhow;
use anyhow::Result;
use image::RgbImage;

/// Builds the shot from the user-selected `crop` and, when available, the
/// whole-display `frame` captured just before.
pub fn shot_from_selection(crop: &RgbImage, frame: Option<&RgbImage>, budget: Budget) -> Shot {
    let region = frame.and_then(|f| shot::locate(f, crop));
    if frame.is_some() && region.is_none() {
        eprintln!(
            "[vision] selected area not found in the display capture; context image has no outline"
        );
    }
    shot::render(crop, frame, region, budget)
}

/// Captures the area the user selects in the portal dialog.
#[allow(dead_code)]
pub fn capture_shot(budget: Budget) -> Result<Shot> {
    #[cfg(target_os = "linux")]
    {
        let fut = async {
            let frame = match portal_screenshot(false).await {
                Ok(frame) => Some(frame),
                Err(e) => {
                    eprintln!("[vision] full-display capture unavailable ({e:#}); sending the selected area alone");
                    None
                }
            };
            let crop = portal_screenshot(true).await?;
            Ok(shot_from_selection(&crop, frame.as_ref(), budget))
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
            Err(_) => tokio::runtime::Runtime::new()?.block_on(fut),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = budget;
        Err(anyhow::anyhow!(
            "Linux portal screenshot unavailable on non-Linux platform"
        ))
    }
}

/// One portal screenshot, decoded. The portal writes the image to a file
/// for this request; Bolo keeps only its own crop, so that file is removed.
#[cfg(target_os = "linux")]
async fn portal_screenshot(interactive: bool) -> Result<RgbImage> {
    use ashpd::desktop::screenshot::Screenshot;
    let response = Screenshot::request()
        .interactive(interactive)
        .modal(interactive)
        .send()
        .await?
        .response()?;
    let uri = url::Url::parse(response.uri().as_str())?;
    let path = uri
        .to_file_path()
        .map_err(|_| anyhow!("invalid screenshot URI path"))?;
    let decoded = image::open(&path).map(|img| img.to_rgb8());
    let _ = std::fs::remove_file(&path);
    Ok(decoded?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{imageops, Rgb};

    fn frame() -> RgbImage {
        shot::test_frame(1920, 1080)
    }

    #[test]
    fn selection_found_in_the_frame_gets_an_outlined_context_image() {
        let frame = frame();
        let crop = imageops::crop_imm(&frame, 600, 300, 500, 300).to_image();
        let shot = shot_from_selection(&crop, Some(&frame), Budget::STANDARD);
        assert_eq!(shot.crop.dimensions(), (506, 306));
        let ctx = shot.context.expect("context image");
        assert_eq!(ctx.width(), shot::CONTEXT_LONG_EDGE);
        let s = 768.0 / 1920.0;
        let left = (600.0 * s) as u32 - 1;
        assert_eq!(*ctx.get_pixel(left, (450.0 * s) as u32), shot::BORDER_COLOR);
        assert!((shot.region_fraction - (500.0 * 300.0) / (1920.0 * 1080.0)).abs() < 1e-9);
    }

    #[test]
    fn selection_without_a_frame_is_sent_alone() {
        let crop = imageops::crop_imm(&frame(), 0, 0, 400, 200).to_image();
        let shot = shot_from_selection(&crop, None, Budget::STANDARD);
        assert!(shot.context.is_none());
        assert_eq!(shot.crop.dimensions(), (406, 206));
    }

    #[test]
    fn whole_screen_selection_is_fitted_to_the_budget() {
        let frame = RgbImage::from_fn(3840, 2160, |x, y| {
            Rgb([(x % 256) as u8, (y % 256) as u8, 7])
        });
        let shot = shot_from_selection(&frame, Some(&frame), Budget::STANDARD);
        let (w, h) = shot.crop.dimensions();
        assert!(Budget::STANDARD.fits(w, h), "{w}x{h}");
        assert_eq!(shot.region_fraction, 1.0);
    }
}
