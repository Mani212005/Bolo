use crate::vision::gesture::CircleGesture;
use anyhow::{anyhow, Result};
use std::path::Path;

/// Captures display screenshot under cursor gesture on macOS,
/// and draws a highlight indicator around the target gesture area.
pub fn capture_screen(gesture: CircleGesture, output_path: &Path) -> Result<()> {
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp_dir = std::env::temp_dir();
    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let raw_capture_path = temp_dir.join(format!("bolo_raw_{now_nanos}.png"));

    // screencapture -x captures display silently without shutter sound
    let status = std::process::Command::new("screencapture")
        .arg("-x")
        .arg(&raw_capture_path)
        .status();

    match status {
        Ok(s) if s.success() && raw_capture_path.exists() => {}
        _ => {
            let _ = std::fs::remove_file(&raw_capture_path);
            return Err(anyhow!("screencapture failed or permission denied"));
        }
    }

    // Try applying highlight ring around gesture center; fallback to unhighlighted image on error
    if let Err(e) = apply_circle_highlight(&raw_capture_path, output_path, gesture) {
        eprintln!("[vision] highlight rendering failed ({e:#}), saving unhighlighted capture");
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if std::fs::rename(&raw_capture_path, output_path).is_err() {
            std::fs::copy(&raw_capture_path, output_path)?;
            let _ = std::fs::remove_file(&raw_capture_path);
        }
    } else {
        let _ = std::fs::remove_file(&raw_capture_path);
    }

    Ok(())
}

fn apply_circle_highlight(input_path: &Path, output_path: &Path, gesture: CircleGesture) -> Result<()> {
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // macOS CoreGraphics FFI for rendering highlight overlay onto captured screenshot PNG
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::raw::c_void;

        type CFURLRef = *const c_void;
        type CGImageSourceRef = *const c_void;
        type CGImageRef = *const c_void;
        type CGContextRef = *const c_void;
        type CGImageDestinationRef = *const c_void;
        type CGColorSpaceRef = *const c_void;
        type CFStringRef = *const c_void;

        #[link(name = "CoreGraphics", kind = "framework")]
        extern "C" {}
        #[link(name = "ImageIO", kind = "framework")]
        extern "C" {}
        #[link(name = "CoreFoundation", kind = "framework")]
        extern "C" {
            fn CGMainDisplayID() -> u32;
            fn CGDisplayBounds(display: u32) -> CGRect;

            fn CFURLCreateWithFileSystemPath(
                allocator: *const c_void,
                filePath: CFStringRef,
                pathStyle: isize,
                isDirectory: bool,
            ) -> CFURLRef;
            fn CFStringCreateWithCString(
                allocator: *const c_void,
                cStr: *const i8,
                encoding: u32,
            ) -> CFStringRef;
            fn CFRelease(cf: *const c_void);

            fn CGImageSourceCreateWithURL(url: CFURLRef, options: *const c_void) -> CGImageSourceRef;
            fn CGImageSourceCreateImageAtIndex(source: CGImageSourceRef, index: usize, options: *const c_void) -> CGImageRef;

            fn CGImageGetWidth(image: CGImageRef) -> usize;
            fn CGImageGetHeight(image: CGImageRef) -> usize;

            fn CGColorSpaceCreateDeviceRGB() -> CGColorSpaceRef;
            fn CGBitmapContextCreate(
                data: *mut c_void,
                width: usize,
                height: usize,
                bitsPerComponent: usize,
                bytesPerRow: usize,
                space: CGColorSpaceRef,
                bitmapInfo: u32,
            ) -> CGContextRef;
            fn CGContextDrawImage(c: CGContextRef, rect: CGRect, image: CGImageRef);
            fn CGContextSetRGBStrokeColor(c: CGContextRef, red: f64, green: f64, blue: f64, alpha: f64);
            fn CGContextSetLineWidth(c: CGContextRef, width: f64);
            fn CGContextStrokeEllipseInRect(c: CGContextRef, rect: CGRect);
            fn CGBitmapContextCreateImage(c: CGContextRef) -> CGImageRef;

            fn CGImageDestinationCreateWithURL(
                url: CFURLRef,
                type_: CFStringRef,
                count: usize,
                options: *const c_void,
            ) -> CGImageDestinationRef;
            fn CGImageDestinationAddImage(idst: CGImageDestinationRef, image: CGImageRef, properties: *const c_void);
            fn CGImageDestinationFinalize(idst: CGImageDestinationRef) -> bool;
        }

        #[repr(C)]
        #[derive(Clone, Copy)]
        struct CGPoint {
            x: f64,
            y: f64,
        }

        #[repr(C)]
        #[derive(Clone, Copy)]
        struct CGSize {
            width: f64,
            height: f64,
        }

        #[repr(C)]
        #[derive(Clone, Copy)]
        struct CGRect {
            origin: CGPoint,
            size: CGSize,
        }

        let in_str = CString::new(input_path.to_str().unwrap_or_default())?;
        let out_str = CString::new(output_path.to_str().unwrap_or_default())?;
        let png_str = CString::new("public.png")?;

        unsafe {
            let k_cf_string_encoding_utf8 = 0x08000100;
            let cf_in_path = CFStringCreateWithCString(std::ptr::null(), in_str.as_ptr(), k_cf_string_encoding_utf8);
            let url_in = CFURLCreateWithFileSystemPath(std::ptr::null(), cf_in_path, 0, false);
            CFRelease(cf_in_path);

            if url_in.is_null() {
                return Err(anyhow!("failed creating input URL"));
            }

            let source = CGImageSourceCreateWithURL(url_in, std::ptr::null());
            CFRelease(url_in);
            if source.is_null() {
                return Err(anyhow!("failed loading image source"));
            }

            let image = CGImageSourceCreateImageAtIndex(source, 0, std::ptr::null());
            CFRelease(source);
            if image.is_null() {
                return Err(anyhow!("failed creating image"));
            }

            let width = CGImageGetWidth(image);
            let height = CGImageGetHeight(image);
            let color_space = CGColorSpaceCreateDeviceRGB();
            let k_cg_image_alpha_premultiplied_last = 1;

            let context = CGBitmapContextCreate(
                std::ptr::null_mut(),
                width,
                height,
                8,
                width * 4,
                color_space,
                k_cg_image_alpha_premultiplied_last,
            );
            CFRelease(color_space);

            if context.is_null() {
                CFRelease(image);
                return Err(anyhow!("failed creating bitmap context"));
            }

            let rect = CGRect {
                origin: CGPoint { x: 0.0, y: 0.0 },
                size: CGSize { width: width as f64, height: height as f64 },
            };
            CGContextDrawImage(context, rect, image);
            CFRelease(image);

            // Compute scaling factor from logical display points to physical captured image pixels
            let display_bounds = CGDisplayBounds(CGMainDisplayID());
            let mut scale_x = if display_bounds.size.width > 0.0 {
                (width as f64) / display_bounds.size.width
            } else {
                1.0
            };
            let mut scale_y = if display_bounds.size.height > 0.0 {
                (height as f64) / display_bounds.size.height
            } else {
                1.0
            };
            if scale_x <= 0.0 || scale_x.is_nan() {
                scale_x = 1.0;
            }
            if scale_y <= 0.0 || scale_y.is_nan() {
                scale_y = 1.0;
            }

            // Draw cyan highlight ring around gesture location, scaled to physical pixels
            let cx_rel = gesture.center.0 - display_bounds.origin.x;
            let cy_rel = gesture.center.1 - display_bounds.origin.y;
            let cx = cx_rel * scale_x;
            let cy = cy_rel * scale_y;
            let r = gesture.radius.max(24.0) * scale_x;
            let highlight_rect = CGRect {
                origin: CGPoint { x: cx - r, y: (height as f64) - cy - r },
                size: CGSize { width: r * 2.0, height: r * 2.0 },
            };

            CGContextSetRGBStrokeColor(context, 0.0, 0.75, 1.0, 0.9); // Cyan-blue stroke
            CGContextSetLineWidth(context, (r * 0.08).clamp(3.0 * scale_x, 8.0 * scale_x));
            CGContextStrokeEllipseInRect(context, highlight_rect);

            let marked_image = CGBitmapContextCreateImage(context);
            CFRelease(context);

            if marked_image.is_null() {
                return Err(anyhow!("failed producing marked image"));
            }

            let cf_out_path = CFStringCreateWithCString(std::ptr::null(), out_str.as_ptr(), k_cf_string_encoding_utf8);
            let url_out = CFURLCreateWithFileSystemPath(std::ptr::null(), cf_out_path, 0, false);
            CFRelease(cf_out_path);

            let type_png = CFStringCreateWithCString(std::ptr::null(), png_str.as_ptr(), k_cf_string_encoding_utf8);
            let dest = CGImageDestinationCreateWithURL(url_out, type_png, 1, std::ptr::null());
            CFRelease(url_out);
            CFRelease(type_png);

            if dest.is_null() {
                CFRelease(marked_image);
                return Err(anyhow!("failed creating image destination"));
            }

            CGImageDestinationAddImage(dest, marked_image, std::ptr::null());
            let ok = CGImageDestinationFinalize(dest);
            CFRelease(dest);
            CFRelease(marked_image);

            if !ok {
                return Err(anyhow!("failed finalizing PNG output"));
            }

            Ok(())
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        std::fs::rename(input_path, output_path)?;
        Ok(())
    }
}
