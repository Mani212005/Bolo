//! macOS screen capture: the region the user points at, cropped from the
//! native-resolution frame of the display under the pointer.

use super::shot::{self, Budget, Rect, Shot};
use anyhow::Result;

/// Captures the region at `pointer` (global logical points; `None` means the
/// current pointer position).
///
/// The region is the accessibility element under the pointer when it is a
/// sensible size, otherwise a 640x400 box around the pointer
/// ([`shot::choose_region`]). Only the display containing the pointer is
/// captured, and its scale comes from the captured pixels, so secondary and
/// non-Retina displays crop correctly.
pub fn capture_shot(pointer: Option<(f64, f64)>, budget: Budget) -> Result<Shot> {
    let pointer = match pointer {
        Some(p) => p,
        None => ffi::pointer_location()
            .ok_or_else(|| anyhow::anyhow!("could not read the pointer position"))?,
    };
    let display = ffi::display_bounds_at(pointer)
        .ok_or_else(|| anyhow::anyhow!("no display under the pointer"))?;
    let elements = ffi::element_frames_at(pointer);
    let frame = capture_rect(display)?;
    Ok(shot::render_pointed(
        &frame, display, pointer, &elements, budget,
    ))
}

/// `screencapture` of one display's rectangle, at the display's native
/// pixels, without the cursor or the shutter sound.
fn capture_rect(rect: Rect) -> Result<image::RgbImage> {
    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let raw = std::env::temp_dir().join(format!("bolo_raw_{now_nanos}.png"));
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
        Ok(s) if s.success() && raw.exists() => image::open(&raw).map(|img| img.to_rgb8()),
        _ => {
            let _ = std::fs::remove_file(&raw);
            return Err(anyhow::anyhow!(
                "screencapture failed or screen recording permission denied"
            ));
        }
    };
    let _ = std::fs::remove_file(&raw);
    Ok(decoded?)
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
        fn CGEventCreate(source: *const c_void) -> *const c_void;
        fn CGEventGetLocation(event: *const c_void) -> CGPoint;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            c_str: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFRelease(cf: CFTypeRef);
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
            CFRelease(event);
            Some((p.x, p.y))
        }
    }

    /// Logical bounds of the display containing `p`.
    pub fn display_bounds_at(p: (f64, f64)) -> Option<Rect> {
        unsafe {
            let mut display = 0u32;
            let mut count = 0u32;
            let err =
                CGGetDisplaysWithPoint(CGPoint { x: p.0, y: p.1 }, 1, &mut display, &mut count);
            if err != 0 || count == 0 {
                return None;
            }
            let b = CGDisplayBounds(display);
            Some(Rect::new(
                b.origin.x,
                b.origin.y,
                b.size.width,
                b.size.height,
            ))
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
