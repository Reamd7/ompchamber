//! Port of `server/lib/openchamber-control/screenshots.js` — where an
//! agent's page screenshots land.
//!
//! The image is written on the server, next to the code it is evidence
//! for, because that is the machine holding the repository — the client
//! that took the picture may be somewhere else entirely. A file in the
//! project is also the only form of this that survives past the chat: it
//! can be referenced from an answer, committed, or attached to a review.

use std::path::PathBuf;

use base64::Engine as _;

/// Project-relative home for agent screenshots.
pub const SCREENSHOT_DIRECTORY: &str = ".ompchamber/screenshots";

const MAX_LABEL_LENGTH: usize = 48;

/// `screenshotSlug` — turns a label into a filename fragment.
///
/// Everything outside a small safe set is dropped rather than escaped:
/// this value reaches the filesystem, and a label is a name, never a
/// path. `..`, a separator, or a leading dot cannot survive this.
pub fn screenshot_slug(label: Option<&str>) -> String {
    let lowered = label.unwrap_or("").to_lowercase();
    let mut slug = String::with_capacity(lowered.len());
    let mut in_run = false;
    for character in lowered.chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            slug.push(character);
            in_run = false;
        } else if !in_run {
            slug.push('-');
            in_run = true;
        }
    }
    let trimmed = slug.trim_matches('-');
    let mut bounded: String = trimmed.chars().take(MAX_LABEL_LENGTH).collect();
    // A slice can end mid-run; trim the trailing separator it introduced.
    while bounded.ends_with('-') {
        bounded.pop();
    }
    if bounded.is_empty() {
        "page".to_string()
    } else {
        bounded
    }
}

/// `screenshotStamp` — file-safe timestamp: sorts chronologically and
/// reads as a date (`2026-08-13T09-37-00-000`).
fn screenshot_stamp(now_unix_ms: i64) -> String {
    let seconds = now_unix_ms.div_euclid(1000);
    let millis = now_unix_ms.rem_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let time_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = time_of_day / 3600;
    let minute = (time_of_day % 3600) / 60;
    let second = time_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}-{minute:02}-{second:02}-{millis:03}")
}

/// Civil-from-days (Howard Hinnant's algorithm), UTC.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The extension the image's own mime type maps to (`.jpg` for anything
/// unrecognized, mirroring `EXTENSIONS.get(mime) || '.jpg'`).
fn extension_for_mime(mime: Option<&str>) -> &'static str {
    match mime {
        Some("image/jpeg") => ".jpg",
        Some("image/png") => ".png",
        Some("image/webp") => ".webp",
        _ => ".jpg",
    }
}

/// Where a written screenshot went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedScreenshot {
    /// Project-relative, posix separators: it is written into Markdown and
    /// commit messages, where a Windows separator is an escape character.
    pub path: String,
    /// Absolute location for callers that need the file itself.
    pub absolute_path: PathBuf,
}

/// `writeScreenshot` — writes one capture into the project and reports
/// where it went. Fails (message exactly as the JS `Error`s) when there is
/// no project to save into or the browser returned no image.
pub async fn write_screenshot(
    directory: &str,
    base64_image: &str,
    mime: Option<&str>,
    label: Option<&str>,
    now_unix_ms: i64,
) -> Result<SavedScreenshot, String> {
    if directory.trim().is_empty() {
        return Err("A project directory is required to save a screenshot".to_string());
    }
    if base64_image.is_empty() {
        return Err("The browser returned no image".to_string());
    }

    let file_name = format!(
        "{}-{}{}",
        screenshot_slug(label),
        screenshot_stamp(now_unix_ms),
        extension_for_mime(mime)
    );
    // Posix separators in the reported path (see SavedScreenshot::path).
    let relative_path = format!("{SCREENSHOT_DIRECTORY}/{file_name}");
    let absolute_path = PathBuf::from(directory).join(&relative_path);

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_image.as_bytes())
        .map_err(|_| "The browser returned no image".to_string())?;
    let parent = absolute_path
        .parent()
        .ok_or_else(|| "A project directory is required to save a screenshot".to_string())?
        .to_path_buf();
    tokio::fs::create_dir_all(&parent)
        .await
        .map_err(|error| error.to_string())?;
    tokio::fs::write(&absolute_path, bytes)
        .await
        .map_err(|error| error.to_string())?;

    Ok(SavedScreenshot {
        path: relative_path,
        absolute_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "oc-screenshots-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&directory).expect("temp dir");
        directory
    }

    // screenshots.test.js — slug behavior.
    #[test]
    fn slug_keeps_a_readable_name() {
        assert_eq!(screenshot_slug(Some("Before fix")), "before-fix");
    }

    #[test]
    fn slug_never_lets_a_label_become_a_path() {
        assert_eq!(screenshot_slug(Some("../../etc/passwd")), "etc-passwd");
        assert_eq!(screenshot_slug(Some("/absolute")), "absolute");
        assert_eq!(screenshot_slug(Some("..")), "page");
        assert_eq!(screenshot_slug(Some(".hidden")), "hidden");
    }

    #[test]
    fn slug_falls_back_to_a_name_rather_than_an_empty_one() {
        assert_eq!(screenshot_slug(Some("")), "page");
        assert_eq!(screenshot_slug(Some("!!!")), "page");
        assert_eq!(screenshot_slug(None), "page");
    }

    #[test]
    fn slug_caps_length_without_a_trailing_separator() {
        let long = "a".repeat(60);
        assert_eq!(screenshot_slug(Some(&long)).len(), MAX_LABEL_LENGTH);
        // A slice landing right after a separator must not keep it.
        let sevens = "ab-".repeat(20);
        let slug = screenshot_slug(Some(&sevens));
        assert_eq!(slug.len(), 47);
        assert!(!slug.ends_with('-'));
    }

    const FIXED_NOW_MS: i64 = 1_786_613_820_000;
    const PIXEL_BASE64: &str = "aW1hZ2UtYnl0ZXM="; // "image-bytes"

    #[tokio::test]
    async fn writes_into_the_project_and_reports_a_portable_relative_path() {
        let directory = temp_dir("write");
        let saved = write_screenshot(
            directory.to_str().unwrap_or_default(),
            PIXEL_BASE64,
            Some("image/jpeg"),
            Some("After fix"),
            FIXED_NOW_MS,
        )
        .await
        .expect("screenshot write");

        assert_eq!(
            saved.path,
            ".ompchamber/screenshots/after-fix-2026-08-13T09-37-00-000.jpg"
        );
        assert_eq!(
            saved.absolute_path,
            directory
                .join(SCREENSHOT_DIRECTORY)
                .join("after-fix-2026-08-13T09-37-00-000.jpg")
        );
        let written = tokio::fs::read(&saved.absolute_path).await.expect("bytes");
        assert_eq!(written, b"image-bytes");
    }

    #[tokio::test]
    async fn names_the_file_after_the_image_it_actually_holds() {
        let directory = temp_dir("png");
        let saved = write_screenshot(
            directory.to_str().unwrap_or_default(),
            PIXEL_BASE64,
            Some("image/png"),
            None,
            FIXED_NOW_MS,
        )
        .await
        .expect("screenshot write");
        assert!(saved.path.ends_with(".png"), "path: {}", saved.path);
        // An unrecognized mime falls back to .jpg, and a missing label
        // still names the file.
        let fallback = write_screenshot(
            directory.to_str().unwrap_or_default(),
            PIXEL_BASE64,
            Some("image/avif"),
            None,
            FIXED_NOW_MS,
        )
        .await
        .expect("screenshot write");
        assert!(fallback.path.starts_with(".ompchamber/screenshots/page-"));
        assert!(fallback.path.ends_with(".jpg"));
    }

    #[tokio::test]
    async fn refuses_to_write_without_a_project_directory() {
        let error = write_screenshot("", PIXEL_BASE64, None, None, FIXED_NOW_MS)
            .await
            .expect_err("empty directory must fail");
        assert_eq!(
            error,
            "A project directory is required to save a screenshot"
        );
    }

    #[tokio::test]
    async fn reports_an_empty_capture_instead_of_writing_a_zero_byte_file() {
        let directory = temp_dir("empty");
        let error = write_screenshot(
            directory.to_str().unwrap_or_default(),
            "",
            None,
            None,
            FIXED_NOW_MS,
        )
        .await
        .expect_err("empty capture must fail");
        assert_eq!(error, "The browser returned no image");
        assert!(
            tokio::fs::read_dir(&directory)
                .await
                .expect("dir")
                .next_entry()
                .await
                .expect("entries")
                .is_none()
        );
    }
}
