//! macOS screen capture: the region the user points at, cropped from the
//! native-resolution frame of the display under the pointer.

use super::shot::{self, Budget, Rect, Shot};
use anyhow::Result;

/// One display at its native pixels, and where it sits in logical points.
pub struct Frame {
    display: Rect,
    image: image::RgbImage,
}

/// Grabs the whole display under `pointer` now (see [`capture_shot`] for how).
pub fn grab_frame(pointer: (f64, f64)) -> Option<Frame> {
    let (display_id, display) = ffi::display_at(pointer)?;
    let image = grab(display_id, display, &|| {}).ok()?;
    Some(Frame { display, image })
}

/// Captures the region at `pointer` (global logical points; `None` means the
/// current pointer position). `on_captured` runs the moment the screen has
/// been grabbed, before the pixels are converted or the accessibility tree is
/// asked, so the user's "screenshot taken" cue is not held up by either.
///
/// When `frozen` shows the display under `pointer`, the region is cut from
/// it and nothing is grabbed now. Otherwise the grab is in-process through
/// CoreGraphics (about 10 ms); where that is unavailable it falls back to the
/// `screencapture` tool (about 150 ms).
///
/// The region is the accessibility element under the pointer when it is a
/// sensible size, otherwise a 640x400 box around the pointer
/// ([`shot::choose_region`]); a table row gets its table's header and
/// columns instead ([`shot::choose_region_in_frame`]). Only the display containing the pointer is
/// captured, and its scale comes from the captured pixels, so secondary and
/// non-Retina displays crop correctly.
pub fn capture_shot(
    pointer: Option<(f64, f64)>,
    frozen: Option<&Frame>,
    budget: Budget,
    on_captured: &dyn Fn(),
) -> Result<Shot> {
    let pointer = match pointer {
        Some(p) => p,
        None => ffi::pointer_location()
            .ok_or_else(|| anyhow::anyhow!("could not read the pointer position"))?,
    };
    let (display_id, display) =
        ffi::display_at(pointer).ok_or_else(|| anyhow::anyhow!("no display under the pointer"))?;
    let live;
    let frame = match frozen_image(frozen, display) {
        Some(image) => {
            eprintln!("[vision] cut from the frame frozen at the Control press");
            on_captured();
            image
        }
        None => {
            live = grab(display_id, display, on_captured)?;
            &live
        }
    };
    let elements = ffi::element_frames_at(pointer);
    Ok(shot::render_pointed(
        frame, display, pointer, &elements, budget,
    ))
}

/// The frozen pixels, when they are of `display`: a circle on another display
/// than the one frozen at the Control press is captured live.
fn frozen_image(frozen: Option<&Frame>, display: Rect) -> Option<&image::RgbImage> {
    frozen
        .filter(|frame| frame.display == display)
        .map(|frame| &frame.image)
}

/// The whole display now, in-process or through `screencapture`;
/// `on_captured` runs once, the moment the pixels exist.
fn grab(display_id: u32, display: Rect, on_captured: &dyn Fn()) -> Result<image::RgbImage> {
    // Once, whichever path takes the picture.
    let fired = std::cell::Cell::new(false);
    let notify = || {
        if !fired.replace(true) {
            on_captured();
        }
    };
    let in_process = if std::env::var_os("BOLO_CAPTURE_FALLBACK").is_some() {
        None // debugging and tests: force the `screencapture` path
    } else {
        ffi::grab_display(display_id, display, &notify)
    };
    match in_process {
        Some(frame) => Ok(frame),
        None => capture_rect(display, &notify),
    }
}

/// `screencapture` of one display's rectangle, at the display's native
/// pixels, without the cursor or the system shutter sound. `on_captured`
/// runs as soon as `screencapture` has written the frame.
fn capture_rect(rect: Rect, on_captured: &dyn Fn()) -> Result<image::RgbImage> {
    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    use std::os::unix::fs::DirBuilderExt;
    let raw_dir = std::env::temp_dir().join(format!("bolo_raw_{now_nanos}"));
    std::fs::DirBuilder::new().mode(0o700).create(&raw_dir)?;
    let raw = raw_dir.join("capture.png");
    let status = std::process::Command::new("screencapture")
        .arg("-x")
        .arg(format!(
            "-R{},{},{},{}",
            rect.x.round(),
            rect.y.round(),
            rect.w.round(),
            rect.h.round()
        ))
        .arg(&raw)
        .status();
    let decoded = match status {
        Ok(s) if s.success() && raw.exists() => {
            on_captured();
            image::open(&raw).map(|img| img.to_rgb8())
        }
        _ => {
            let _ = std::fs::remove_dir_all(&raw_dir);
            return Err(anyhow::anyhow!(
                "screencapture failed or screen recording permission denied"
            ));
        }
    };
    let _ = std::fs::remove_dir_all(&raw_dir);
    Ok(decoded?)
}

/// Pays CoreGraphics' one-time start-up cost (about 50 ms on the first grab of
/// a process) before anyone is waiting for a screenshot: a throwaway 8x8 grab.
pub fn prewarm() {
    ffi::prewarm();
}

/// RGB from a BGRA frame whose rows are `stride` bytes apart; `None` when
/// `bytes` is too short for `width` x `height`.
fn bgra_to_rgb(
    bytes: &[u8],
    width: usize,
    height: usize,
    stride: usize,
) -> Option<image::RgbImage> {
    if width == 0
        || height == 0
        || stride < width * 4
        || bytes.len() < stride * (height - 1) + width * 4
    {
        return None;
    }
    let mut rgb = Vec::with_capacity(width * height * 3);
    for row in bytes.chunks(stride).take(height) {
        for px in row[..width * 4].chunks_exact(4) {
            rgb.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    image::RgbImage::from_raw(width as u32, height as u32, rgb)
}

mod ffi {
    use super::Rect;
    use std::ffi::CString;
    use std::os::raw::{c_char, c_void};

    type CFTypeRef = *const c_void;
    type CFStringRef = *const c_void;
    type AXUIElementRef = *const c_void;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CGSize {
        width: f64,
        height: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    /// kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little: bytes B, G, R, A.
    const K_BGRA_PREMULTIPLIED_FIRST_LITTLE: u32 = 0x2002;
    const K_AX_VALUE_CG_POINT_TYPE: u32 = 1;
    const K_AX_VALUE_CG_SIZE_TYPE: u32 = 2;
    const K_AX_ERROR_SUCCESS: i32 = 0;
    /// Hit element plus parents; deeper than this is window chrome.
    const MAX_ANCESTORS: usize = 8;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGGetDisplaysWithPoint(
            point: CGPoint,
            max_displays: u32,
            displays: *mut u32,
            matching_display_count: *mut u32,
        ) -> i32;
        fn CGDisplayBounds(display: u32) -> CGRect;
        fn CGMainDisplayID() -> u32;
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGImageGetWidth(image: CFTypeRef) -> usize;
        fn CGImageGetHeight(image: CFTypeRef) -> usize;
        fn CGImageGetBytesPerRow(image: CFTypeRef) -> usize;
        fn CGImageGetBitsPerPixel(image: CFTypeRef) -> usize;
        fn CGImageGetBitmapInfo(image: CFTypeRef) -> u32;
        fn CGImageGetDataProvider(image: CFTypeRef) -> CFTypeRef;
        fn CGDataProviderCopyData(provider: CFTypeRef) -> CFTypeRef;
        fn CGEventCreate(source: *const c_void) -> *mut c_void;
        fn CGEventGetLocation(event: *mut c_void) -> CGPoint;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            c_str: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFRelease(cf: CFTypeRef);
        fn CFDataGetLength(data: CFTypeRef) -> isize;
        fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
    }

    extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXUIElementCreateSystemWide() -> AXUIElementRef;
        fn AXUIElementSetMessagingTimeout(element: AXUIElementRef, timeout_seconds: f32) -> i32;
        fn AXUIElementCopyElementAtPosition(
            application: AXUIElementRef,
            x: f32,
            y: f32,
            element: *mut AXUIElementRef,
        ) -> i32;
        fn AXUIElementCopyAttributeValue(
            element: AXUIElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> i32;
        fn AXValueGetValue(value: CFTypeRef, the_type: u32, value_ptr: *mut c_void) -> bool;
    }

    /// The current pointer position in global logical points.
    pub fn pointer_location() -> Option<(f64, f64)> {
        unsafe {
            let event = CGEventCreate(std::ptr::null());
            if event.is_null() {
                return None;
            }
            let p = CGEventGetLocation(event);
            CFRelease(event as CFTypeRef);
            Some((p.x, p.y))
        }
    }

    /// The display containing `p`: its id and logical bounds.
    pub fn display_at(p: (f64, f64)) -> Option<(u32, Rect)> {
        unsafe {
            let mut display = 0u32;
            let mut count = 0u32;
            let err =
                CGGetDisplaysWithPoint(CGPoint { x: p.0, y: p.1 }, 1, &mut display, &mut count);
            if err != 0 || count == 0 {
                return None;
            }
            let b = CGDisplayBounds(display);
            Some((
                display,
                Rect::new(b.origin.x, b.origin.y, b.size.width, b.size.height),
            ))
        }
    }

    type CreateImageForRect = unsafe extern "C" fn(u32, CGRect) -> CFTypeRef;

    /// `CGDisplayCreateImageForRect`, looked up at run time: Apple deprecated
    /// it, so a macOS release that drops it must send Bolo to the
    /// `screencapture` fallback rather than stop it from launching.
    fn create_image_for_rect() -> Option<CreateImageForRect> {
        const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;
        let name = CString::new("CGDisplayCreateImageForRect").ok()?;
        let symbol = unsafe { dlsym(RTLD_DEFAULT, name.as_ptr()) };
        // SAFETY: the symbol has this signature in CoreGraphics.
        (!symbol.is_null()).then(|| unsafe { std::mem::transmute(symbol) })
    }

    pub fn prewarm() {
        unsafe {
            if !CGPreflightScreenCaptureAccess() {
                return;
            }
            let Some(create) = create_image_for_rect() else {
                return;
            };
            let rect = CGRect {
                origin: CGPoint::default(),
                size: CGSize {
                    width: 8.0,
                    height: 8.0,
                },
            };
            let image = create(CGMainDisplayID(), rect);
            if !image.is_null() {
                CFRelease(image);
            }
        }
    }

    /// Grabs the whole display in-process at its native pixels, without the
    /// cursor, and calls `on_captured` the moment the image exists. `None`
    /// (nothing called) when Screen Recording is not granted (without it the
    /// grab would silently be the wallpaper alone), when the symbol is gone, or
    /// when the pixels are not the 32-bit BGRA this converts.
    pub fn grab_display(
        display: u32,
        bounds: Rect,
        on_captured: &dyn Fn(),
    ) -> Option<image::RgbImage> {
        unsafe {
            if !CGPreflightScreenCaptureAccess() {
                return None;
            }
            let create = create_image_for_rect()?;
            let rect = CGRect {
                origin: CGPoint::default(),
                size: CGSize {
                    width: bounds.w,
                    height: bounds.h,
                },
            };
            let image = create(display, rect);
            if image.is_null() {
                return None;
            }
            let (width, height) = (CGImageGetWidth(image), CGImageGetHeight(image));
            let stride = CGImageGetBytesPerRow(image);
            let bgra = CGImageGetBitsPerPixel(image) == 32
                && CGImageGetBitmapInfo(image) == K_BGRA_PREMULTIPLIED_FIRST_LITTLE;
            if !bgra || width == 0 || height == 0 {
                CFRelease(image);
                return None;
            }
            on_captured();
            let data = CGDataProviderCopyData(CGImageGetDataProvider(image));
            let frame = if data.is_null() {
                None
            } else {
                let len = CFDataGetLength(data) as usize;
                let bytes = std::slice::from_raw_parts(CFDataGetBytePtr(data), len);
                let frame = super::bgra_to_rgb(bytes, width, height, stride);
                CFRelease(data);
                frame
            };
            CFRelease(image);
            frame
        }
    }

    struct CfString(CFStringRef);

    impl CfString {
        fn new(s: &str) -> Option<Self> {
            let c = CString::new(s).ok()?;
            let r = unsafe {
                CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), K_CF_STRING_ENCODING_UTF8)
            };
            (!r.is_null()).then_some(Self(r))
        }
    }

    impl Drop for CfString {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0) }
        }
    }

    /// Copies an attribute; the caller releases the result.
    unsafe fn copy_attribute(element: AXUIElementRef, name: &CfString) -> Option<CFTypeRef> {
        let mut value: CFTypeRef = std::ptr::null();
        let err = AXUIElementCopyAttributeValue(element, name.0, &mut value);
        (err == K_AX_ERROR_SUCCESS && !value.is_null()).then_some(value)
    }

    unsafe fn frame_of(element: AXUIElementRef, pos: &CfString, size: &CfString) -> Option<Rect> {
        let pos_value = copy_attribute(element, pos)?;
        let mut origin = CGPoint::default();
        let ok_pos = AXValueGetValue(
            pos_value,
            K_AX_VALUE_CG_POINT_TYPE,
            &mut origin as *mut CGPoint as *mut c_void,
        );
        CFRelease(pos_value);
        let size_value = copy_attribute(element, size)?;
        let mut extent = CGSize::default();
        let ok_size = AXValueGetValue(
            size_value,
            K_AX_VALUE_CG_SIZE_TYPE,
            &mut extent as *mut CGSize as *mut c_void,
        );
        CFRelease(size_value);
        (ok_pos && ok_size).then(|| Rect::new(origin.x, origin.y, extent.width, extent.height))
    }

    /// Frames of the accessibility element under `p` and its parents,
    /// innermost first. Empty without the Accessibility permission or when
    /// the app exposes nothing there.
    pub fn element_frames_at(p: (f64, f64)) -> Vec<Rect> {
        let mut frames = Vec::new();
        let (Some(pos), Some(size), Some(parent)) = (
            CfString::new("AXPosition"),
            CfString::new("AXSize"),
            CfString::new("AXParent"),
        ) else {
            return frames;
        };
        unsafe {
            let system = AXUIElementCreateSystemWide();
            if system.is_null() {
                return frames;
            }
            AXUIElementSetMessagingTimeout(system, 0.25);
            let mut element: AXUIElementRef = std::ptr::null();
            let err =
                AXUIElementCopyElementAtPosition(system, p.0 as f32, p.1 as f32, &mut element);
            CFRelease(system);
            if err != K_AX_ERROR_SUCCESS || element.is_null() {
                return frames;
            }
            for _ in 0..MAX_ANCESTORS {
                if let Some(frame) = frame_of(element, &pos, &size) {
                    frames.push(frame);
                }
                let next = copy_attribute(element, &parent);
                CFRelease(element);
                match next {
                    Some(next) => element = next,
                    None => return frames,
                }
            }
            CFRelease(element);
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_rows_become_rgb_and_skip_stride_padding() {
        // 2x2 frame, 12-byte rows (8 pixel bytes + 4 padding).
        let bytes = [
            1, 2, 3, 255, 4, 5, 6, 255, 9, 9, 9, 9, //
            7, 8, 9, 255, 10, 11, 12, 255, 9, 9, 9, 9,
        ];
        let img = bgra_to_rgb(&bytes, 2, 2, 12).unwrap();
        assert_eq!(img.dimensions(), (2, 2));
        assert_eq!(img.get_pixel(0, 0).0, [3, 2, 1]);
        assert_eq!(img.get_pixel(1, 0).0, [6, 5, 4]);
        assert_eq!(img.get_pixel(0, 1).0, [9, 8, 7]);
        assert_eq!(img.get_pixel(1, 1).0, [12, 11, 10]);
    }

    #[test]
    fn a_short_buffer_is_refused() {
        assert!(bgra_to_rgb(&[0; 15], 2, 2, 8).is_none());
        assert!(
            bgra_to_rgb(&[0; 16], 2, 2, 4).is_none(),
            "stride under a row"
        );
        assert!(bgra_to_rgb(&[], 0, 0, 0).is_none());
    }

    #[test]
    fn a_frozen_frame_is_used_only_for_its_own_display() {
        // This Mac's built-in display, and a second display to its right.
        let built_in = Rect::new(0.0, 0.0, 1470.0, 956.0);
        let external = Rect::new(1470.0, 0.0, 1920.0, 1080.0);
        let frozen = Frame {
            display: built_in,
            image: image::RgbImage::new(2940, 1912),
        };
        let used = frozen_image(Some(&frozen), built_in).expect("same display");
        assert!(std::ptr::eq(used, &frozen.image));
        assert!(frozen_image(Some(&frozen), external).is_none());
        assert!(frozen_image(None, built_in).is_none());
    }

    #[test]
    #[ignore = "takes a real screenshot; needs Screen Recording for the test runner"]
    fn grabbing_the_main_display_matches_the_screencapture_fallback() {
        let (id, bounds) = ffi::display_at((10.0, 10.0)).expect("a display at (10, 10)");
        let fired = std::cell::Cell::new(0);
        let fast = ffi::grab_display(id, bounds, &|| fired.set(fired.get() + 1))
            .expect("in-process grab (is Screen Recording granted?)");
        assert_eq!(fired.get(), 1, "the cue hook fires once, at the grab");
        let slow = capture_rect(bounds, &|| {}).unwrap();
        assert_eq!(fast.dimensions(), slow.dimensions());
    }
}
