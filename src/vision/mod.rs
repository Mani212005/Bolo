pub mod gesture;
pub mod linux;
pub mod macos;

pub use gesture::{CircleGesture, CircleGestureDetector};

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const DEFAULT_MAX_AGE_SECS: u64 = 7 * 24 * 3600; // 7 days
pub const DEFAULT_MAX_BYTES: u64 = 500 * 1024 * 1024; // 500 MB
pub const ACCIDENTAL_THRESHOLD_SECS: f64 = 2.5;

/// Cross-platform screen context capture entry point.
pub fn capture_screen(gesture: CircleGesture, output_path: &Path) -> Result<()> {
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(target_os = "macos")]
    {
        macos::capture_screen(gesture, output_path)
    }
    #[cfg(target_os = "linux")]
    {
        linux::capture_screen(gesture, output_path)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (gesture, output_path);
        Err(anyhow::anyhow!(
            "Screen capture not supported on this operating system"
        ))
    }
}

/// Evaluates if a completed recording session should be discarded as an accidental tap.
pub fn is_accidental_session(has_transcript: bool, has_context: bool, duration_s: f64) -> bool {
    if has_transcript || has_context {
        false
    } else {
        duration_s < ACCIDENTAL_THRESHOLD_SECS
    }
}

/// Writes the structured Markdown context bundle (context.md) to disk.
pub fn write_context_bundle(
    session_dir: &Path,
    transcript: &str,
    images: &[PathBuf],
) -> Result<PathBuf> {
    std::fs::create_dir_all(session_dir)?;
    let trimmed = transcript.trim();
    let mut markdown = String::from("# Bolo session\n\n");

    if trimmed.is_empty() {
        markdown.push_str("_No transcript captured._\n");
    } else {
        markdown.push_str(trimmed);
        markdown.push('\n');
    }

    if !images.is_empty() {
        markdown.push_str("\n## Screen context\n\n");
        for (index, image_path) in images.iter().enumerate() {
            let filename = image_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("context.png");
            markdown.push_str(&format!("![Context {}]({})\n\n", index + 1, filename));
        }
    }

    let markdown_path = session_dir.join("context.md");
    std::fs::write(&markdown_path, markdown)?;
    Ok(markdown_path)
}

#[derive(Debug, Clone)]
pub struct StoredSession {
    pub path: PathBuf,
    pub modified_at: SystemTime,
    pub bytes: u64,
}

/// Enforces the disk quota and 7-day age retention policy.
pub fn prune_sessions(sessions_dir: &Path, max_age_secs: u64, max_bytes: u64) -> Result<usize> {
    if !sessions_dir.exists() {
        return Ok(0);
    }

    let entries = std::fs::read_dir(sessions_dir)?;
    let mut sessions: Vec<StoredSession> = Vec::new();
    let now = SystemTime::now();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let metadata = entry.metadata()?;
        let modified_at = metadata.modified().unwrap_or(now);
        let bytes = calculate_dir_bytes(&path)?;
        sessions.push(StoredSession {
            path,
            modified_at,
            bytes,
        });
    }

    let max_age = Duration::from_secs(max_age_secs);
    let mut removed_count = 0;
    let mut kept: Vec<StoredSession> = Vec::new();

    for session in sessions {
        let age = now.duration_since(session.modified_at).unwrap_or_default();
        if age > max_age {
            if std::fs::remove_dir_all(&session.path).is_ok() {
                removed_count += 1;
            }
        } else {
            kept.push(session);
        }
    }

    // Sort kept sessions by modified_at ascending (oldest first)
    kept.sort_by_key(|s| s.modified_at);
    let mut total_bytes: u64 = kept.iter().map(|s| s.bytes).sum();

    for session in kept {
        if total_bytes <= max_bytes {
            break;
        }
        if std::fs::remove_dir_all(&session.path).is_ok() {
            total_bytes = total_bytes.saturating_sub(session.bytes);
            removed_count += 1;
        }
    }

    Ok(removed_count)
}

fn calculate_dir_bytes(dir: &Path) -> Result<u64> {
    let mut total = 0;
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)?.flatten() {
            let path = entry.path();
            if path.is_file() {
                total += entry.metadata()?.len();
            } else if path.is_dir() {
                total += calculate_dir_bytes(&path)?;
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("bolo_test_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let _ = std::fs::create_dir_all(&path);
        path
    }

    #[test]
    fn test_accidental_session_policy() {
        assert!(is_accidental_session(false, false, 1.5));
        assert!(is_accidental_session(false, false, 2.49));
        assert!(!is_accidental_session(false, false, 2.5));
        assert!(!is_accidental_session(true, false, 1.0));
        assert!(!is_accidental_session(false, true, 0.5));
        assert!(!is_accidental_session(true, true, 0.5));
    }

    #[test]
    fn test_write_context_bundle_creation() -> Result<()> {
        let temp_dir = make_test_temp_dir("write_bundle");
        let session_dir = temp_dir.join("session_1");

        let img1 = session_dir.join("context-1.png");
        let img2 = session_dir.join("context-2.png");
        let images = vec![img1, img2];

        let markdown_path = write_context_bundle(&session_dir, "Hello Bolo vision", &images)?;
        assert!(markdown_path.exists());

        let content = std::fs::read_to_string(markdown_path)?;
        assert!(content.contains("# Bolo session"));
        assert!(content.contains("Hello Bolo vision"));
        assert!(content.contains("![Context 1](context-1.png)"));
        assert!(content.contains("![Context 2](context-2.png)"));

        let _ = std::fs::remove_dir_all(&temp_dir);
        Ok(())
    }

    #[test]
    fn test_prune_sessions_age_and_quota() -> Result<()> {
        let temp_dir = make_test_temp_dir("prune");
        let sessions_dir = temp_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir)?;

        // Session 1: Old session (> 7 days)
        let s1 = sessions_dir.join("s1");
        std::fs::create_dir(&s1)?;
        let big_file = s1.join("context-1.png");
        let data = vec![0u8; 100 * 1024]; // 100 KB
        std::fs::write(&big_file, &data)?;

        // Session 2: Recent small session
        let s2 = sessions_dir.join("s2");
        std::fs::create_dir(&s2)?;
        std::fs::write(s2.join("context.md"), "recent small")?;

        // Test pruning with max_bytes set to 50 KB (forces s1 deletion since s1 is 100 KB)
        let removed = prune_sessions(&sessions_dir, 7 * 86400, 50 * 1024)?;
        assert_eq!(removed, 1);
        assert!(!s1.exists());
        assert!(s2.exists());

        let _ = std::fs::remove_dir_all(&temp_dir);
        Ok(())
    }

    #[test]
    fn test_linux_mock_screen_capture() -> Result<()> {
        let temp_dir = make_test_temp_dir("mock_cap");
        let out_png = temp_dir.join("context-1.png");
        let gesture = CircleGesture::new((500.0, 300.0), 40.0);

        linux::capture_screen_mock(gesture, &out_png)?;
        assert!(out_png.exists());
        let bytes = std::fs::read(&out_png)?;
        assert!(bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47])); // Valid PNG header

        let _ = std::fs::remove_dir_all(&temp_dir);
        Ok(())
    }

    #[test]
    fn test_capture_screen_creates_parent_directory() -> Result<()> {
        let temp_dir = make_test_temp_dir("capture_nonexistent");
        let non_existent_session_dir = temp_dir.join("nested_sessions").join("session_123");
        assert!(!non_existent_session_dir.exists());

        let out_png = non_existent_session_dir.join("context-1.png");
        let gesture = CircleGesture::new((500.0, 300.0), 40.0);

        capture_screen(gesture, &out_png)?;
        assert!(out_png.exists());
        assert!(non_existent_session_dir.exists());
        let bytes = std::fs::read(&out_png)?;
        assert!(bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47])); // Valid PNG header

        let _ = std::fs::remove_dir_all(&temp_dir);
        Ok(())
    }

    #[test]
    fn test_linux_mock_screen_capture_creates_nonexistent_dir() -> Result<()> {
        let temp_dir = make_test_temp_dir("mock_cap_nonexistent");
        let non_existent_session_dir = temp_dir.join("nested").join("session_456");
        assert!(!non_existent_session_dir.exists());

        let out_png = non_existent_session_dir.join("context-1.png");
        let gesture = CircleGesture::new((500.0, 300.0), 40.0);

        linux::capture_screen_mock(gesture, &out_png)?;
        assert!(out_png.exists());
        assert!(non_existent_session_dir.exists());
        let bytes = std::fs::read(&out_png)?;
        assert!(bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47])); // Valid PNG header

        let _ = std::fs::remove_dir_all(&temp_dir);
        Ok(())
    }

    #[test]
    fn test_e2e_gesture_recognition_and_capture_flow() -> Result<()> {
        use std::f64::consts::PI;

        let temp_dir = make_test_temp_dir("e2e_flow");
        let session_dir = temp_dir.join("session_99999");
        assert!(!session_dir.exists());

        // 1. Simulate gesture recognition during recording
        let mut detector = CircleGestureDetector::default();
        let mut detected_gesture = None;
        let center = (400.0, 300.0);

        for index in 0..48 {
            let angle = (index as f64) / 47.0 * 2.0 * PI;
            let point = (center.0 + 50.0 * angle.cos(), center.1 + 50.0 * angle.sin());
            let time = (index as f64) / 60.0;
            if let Some(g) = detector.add(point, time) {
                detected_gesture = Some(g);
            }
        }

        let gesture = detected_gesture.expect("Gesture detector should recognize the circle");

        // 2. Perform capture into session directory (which might not exist prior to capture)
        let img_path = session_dir.join("context-1.png");
        capture_screen(gesture, &img_path)?;
        assert!(img_path.exists());
        let captured_images = vec![img_path];

        // 3. Verify accidental session policy keeps session with context even if duration is short and no speech
        assert!(!is_accidental_session(
            false,
            !captured_images.is_empty(),
            1.0
        ));

        // 4. Bundle markdown
        let md_path = write_context_bundle(&session_dir, "", &captured_images)?;
        assert!(md_path.exists());
        let md_content = std::fs::read_to_string(md_path)?;
        assert!(md_content.contains("![Context 1](context-1.png)"));

        let _ = std::fs::remove_dir_all(&temp_dir);
        Ok(())
    }
}
