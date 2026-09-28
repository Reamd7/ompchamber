//! Port of `server/lib/openchamber-control/screenshots.js` — where an
//! agent's page screenshots land.
//!
//! The image is written on the server, next to the code it is evidence
//! for, because that is the machine holding the repository — the client
//! that took the picture may be somewhere else entirely. A file in the
//! project is also the only form of this that survives past the chat: it
//! can be referenced from an answer, committed, or attached to a review.
//! （中文说明）本文件是 `server/lib/openchamber-control/screenshots.js`
//! 的移植，决定 agent 的页面截图落在哪里。图片写在服务器上、紧挨着它
//! 作为证据的代码，因为持有仓库的机器是服务器——拍照的客户端可能远在
//! 别处。落进项目的文件也是唯一能在聊天结束后继续存在的形式：可以在
//! 回答里引用、提交，或附到 review 上。

use std::path::PathBuf;

use base64::Engine as _;

/// Project-relative home for agent screenshots.
/// 截图在项目内的固定相对目录。
pub const SCREENSHOT_DIRECTORY: &str = ".ompchamber/screenshots";

/// slug 的最大字符数；更长的 label 截断到该长度。
const MAX_LABEL_LENGTH: usize = 48;

/// `screenshotSlug` — turns a label into a filename fragment.
///
/// Everything outside a small safe set is dropped rather than escaped:
/// this value reaches the filesystem, and a label is a name, never a
/// path. `..`, a separator, or a leading dot cannot survive this.
/// 小写化后只保留 ASCII 字母与数字，其它字符折叠为单个 `-`，再截断到
/// 48 字符并去掉首尾 `-`；结果为空时回退 `"page"`。安全集合之外的字符
/// 直接丢弃而不是转义，因此 `..`、路径分隔符、前导点都无法存活——
/// label 是名字，永远不是路径。
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
/// 由 Unix 毫秒时间戳生成文件名安全的日期串，按字典序即时间序排列。
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
/// Howard Hinnant 的"天数转公历日期"算法（UTC）：把 Unix 纪元起的天数
/// 换算为 `(年, 月, 日)`，不引入时区依赖。
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
/// mime 到扩展名的映射；无法识别的 mime 回退 `.jpg`，与 JS 的
/// `EXTENSIONS.get(mime) || '.jpg'` 行为一致。
fn extension_for_mime(mime: Option<&str>) -> &'static str {
    match mime {
        Some("image/jpeg") => ".jpg",
        Some("image/png") => ".png",
        Some("image/webp") => ".webp",
        _ => ".jpg",
    }
}

/// Where a written screenshot went.
/// 截图写入结果的坐标：相对路径供引用，绝对路径供直接读文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedScreenshot {
    /// Project-relative, posix separators: it is written into Markdown and
    /// commit messages, where a Windows separator is an escape character.
    /// 写入 Markdown 与 commit message 使用，必须保持 posix 分隔符。
    pub path: String,
    /// Absolute location for callers that need the file itself.
    /// 需要文件本体时的绝对位置。
    pub absolute_path: PathBuf,
}

/// `writeScreenshot` — writes one capture into the project and reports
/// where it went. Fails (message exactly as the JS `Error`s) when there is
/// no project to save into or the browser returned no image.
/// 把一次截图写入项目并回报位置。目录为空报 "A project directory is
/// required to save a screenshot"，图片为空或 base64 解码失败报
/// "The browser returned no image"（消息与 JS `Error` 逐字一致）；
/// 成功时按需创建目录并写文件。
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

/// slug/时间戳/写入行为与 JS screenshots.test.js 对齐的单元测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 创建唯一的临时目录（标签 + 进程号 + 时间戳），避免用例间串扰。
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
    /// 验证：普通 label 转成可读的 kebab-case slug。
    #[test]
    fn slug_keeps_a_readable_name() {
        assert_eq!(screenshot_slug(Some("Before fix")), "before-fix");
    }

    /// 验证：label 无论怎样拼出路径片段（`..`、绝对路径、隐藏文件）都
    /// 无法变成路径。
    #[test]
    fn slug_never_lets_a_label_become_a_path() {
        assert_eq!(screenshot_slug(Some("../../etc/passwd")), "etc-passwd");
        assert_eq!(screenshot_slug(Some("/absolute")), "absolute");
        assert_eq!(screenshot_slug(Some("..")), "page");
        assert_eq!(screenshot_slug(Some(".hidden")), "hidden");
    }

    /// 验证：空、纯符号、缺失的 label 都回退 `"page"` 而不是空文件名。
    #[test]
    fn slug_falls_back_to_a_name_rather_than_an_empty_one() {
        assert_eq!(screenshot_slug(Some("")), "page");
        assert_eq!(screenshot_slug(Some("!!!")), "page");
        assert_eq!(screenshot_slug(None), "page");
    }

    /// 验证：超长 label 截断到上限且不残留尾部 `-`。
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

    /// 固定时间戳（对应 2026-08-13T09-37-00.000Z），使文件名可精确断言。
    const FIXED_NOW_MS: i64 = 1_786_613_820_000;
    /// "image-bytes" 的 base64 编码，作为假图片数据。
    const PIXEL_BASE64: &str = "aW1hZ2UtYnl0ZXM="; // "image-bytes"

    /// 验证：截图写入项目目录，返回 posix 相对路径与绝对路径，且写出的
    /// 字节与解码输入一致。
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

    /// 验证：扩展名跟随实际 mime（png 用 `.png`），未知 mime 回退 `.jpg`，
    /// 缺 label 时文件名用 `page`。
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

    /// 验证：目录为空时拒绝写入并返回与 JS 一致的错误消息。
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

    /// 验证：空截图按错误上报而不是写出零字节文件（目录保持为空）。
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
