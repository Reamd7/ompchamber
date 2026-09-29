//! Pure helpers ported from `package-manager.js`: path normalization and
//! comparison, platform/arch mapping, the install-id file, and the
//! semver-like comparison used by update checks.
//!
//! 中文说明：从 `package-manager.js` 移植的纯函数集合——路径归一化与比较、
//! platform/arch 映射、安装 ID 文件的读写，以及更新检查用的类 semver 比较。
//! 本文件不触碰网络与子进程，只做确定性数据变换，便于直接单测。

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

// ---------------------------------------------------------------------------
// process.* shims
// ---------------------------------------------------------------------------

/// `process.platform` (`std::env::consts::OS` mapped onto the Node names).
/// 返回 win32 / darwin / linux 之一。
pub fn process_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

/// 是否运行在 Windows 上（决定路径比较是否转小写、更新命令用哪个 shell）。
pub fn is_windows() -> bool {
    cfg!(target_os = "windows")
}

/// `process.arch`.
/// 返回 arm64 / x64 之一，无法识别的架构记为 "unknown"。
pub fn process_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "x64"
    } else {
        "unknown"
    }
}

/// `os.homedir()` (Node resolves USERPROFILE on Windows, HOME elsewhere).
/// 环境变量未设置时返回 `None`（调用方据此放弃安装 ID 持久化）。
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

// ---------------------------------------------------------------------------
// map*/normalize* helpers
// ---------------------------------------------------------------------------

/// JS `mapPlatform`：把 Node 平台名归并为发布矩阵取值——darwin→macos、
/// win32→windows，其余一律归为 "web"。
pub fn map_platform(value: &str) -> &'static str {
    match value {
        "darwin" => "macos",
        "win32" => "windows",
        "linux" => "linux",
        _ => "web",
    }
}

/// JS `mapArch`：把 Node/常见架构别名归并为 arm64 / x64 / unknown。
pub fn map_arch(value: &str) -> &'static str {
    match value {
        "arm64" | "aarch64" => "arm64",
        "x64" | "amd64" => "x64",
        _ => "unknown",
    }
}

/// JS `normalizeAppType`：非法或缺失的 appType 一律归一为 "web"。
pub fn normalize_app_type(value: Option<&str>) -> &'static str {
    match value {
        Some("web") => "web",
        Some("desktop-electron") => "desktop-electron",
        Some("vscode") => "vscode",
        Some("mobile-capacitor") => "mobile-capacitor",
        _ => "web",
    }
}

/// JS `normalizeDeviceClass`：非法或缺失的设备类别归一为 "unknown"。
pub fn normalize_device_class(value: Option<&str>) -> &'static str {
    match value {
        Some("mobile") => "mobile",
        Some("tablet") => "tablet",
        Some("desktop") => "desktop",
        Some("unknown") => "unknown",
        _ => "unknown",
    }
}

/// JS `normalizePlatform`：仅接受已知平台名，否则回退到宿主平台
/// （先经 `map_platform` 归并，保证非 Node 命名也能命中）。
pub fn normalize_platform(value: Option<&str>) -> &'static str {
    match value {
        Some("macos") => "macos",
        Some("windows") => "windows",
        Some("linux") => "linux",
        Some("web") => "web",
        Some("android") => "android",
        Some("ios") => "ios",
        _ => map_platform(process_platform()),
    }
}

/// JS `normalizeArch`：仅接受已知架构名，否则回退到宿主架构
/// （先经 `map_arch` 归并）。
pub fn normalize_arch(value: Option<&str>) -> &'static str {
    match value {
        Some("aarch64" | "arm64") => "arm64",
        Some("x64") => "x64",
        Some("unknown") => "unknown",
        _ => map_arch(process_arch()),
    }
}

/// `path.resolve(input)` — absolutize against the cwd and lexically remove
/// `.`/`..` segments (no symlink resolution, exactly like Node).
/// cwd 不可得时以 `/` 兜底，保证返回值始终是绝对路径。
pub fn resolve_lexically(input: &Path) -> PathBuf {
    let joined = if input.is_absolute() {
        input.to_path_buf()
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        cwd.join(input)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// JS `normalizePathForComparison`: resolved + normalized (lowercased on
/// Windows); `None` for empty input.
/// 空输入返回 `None`，调用方以「不可比较」处理。
pub fn normalize_path_for_comparison(input: &str) -> Option<String> {
    if input.is_empty() {
        return None;
    }
    let mut normalized = resolve_lexically(Path::new(input))
        .to_string_lossy()
        .into_owned();
    if is_windows() {
        normalized = normalized.to_lowercase();
    }
    Some(normalized)
}

/// JS `getComparablePaths`: the normalized path plus its realpath (when it
/// exists), for symlink-insensitive comparison.
/// realpath 不存在（文件尚未创建）时集合里只有归一化路径本身。
pub fn get_comparable_paths(input: &str) -> HashSet<String> {
    let mut paths = HashSet::new();
    if let Some(normalized) = normalize_path_for_comparison(input) {
        paths.insert(normalized);
    }
    if let Ok(real) = std::fs::canonicalize(input)
        && let Some(normalized_real) = normalize_path_for_comparison(&real.to_string_lossy())
    {
        paths.insert(normalized_real);
    }
    paths
}

/// JS `pathSetContains`.
/// 任一元素相同即返回 true，用于判断两个路径集合是否指向同一位置。
pub fn path_set_contains(a: &HashSet<String>, b: &HashSet<String>) -> bool {
    a.iter().any(|value| b.contains(value))
}

/// JS `getUniquePaths`: dedupe by normalized form, keep the resolved original.
/// 无法归一化（空串）的条目被直接丢弃。
pub fn get_unique_paths(paths: &[String]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for value in paths {
        let Some(normalized) = normalize_path_for_comparison(value) else {
            continue;
        };
        if seen.contains(&normalized) {
            continue;
        }
        seen.insert(normalized);
        result.push(resolve_lexically(Path::new(value)));
    }
    result
}

// ---------------------------------------------------------------------------
// install id (getOMPChamberConfigDir / getOrCreateInstallId)
// ---------------------------------------------------------------------------

/// JS `getOMPChamberConfigDir`: `%APPDATA%/ompchamber` on Windows, otherwise
/// `~/.config/ompchamber` (deliberately ignoring XDG_CONFIG_HOME).
/// 返回 `None` 表示拿不到主目录/APPDATA，调用方应跳过安装 ID 逻辑。
pub fn get_ompchamber_config_dir(home: Option<&Path>, appdata: Option<&str>) -> Option<PathBuf> {
    if is_windows() {
        if let Some(appdata) = appdata {
            return Some(Path::new(appdata).join("ompchamber"));
        }
        // Node falls through to os.homedir() when APPDATA is unset.
        return home.map(|home| home.join(".config").join("ompchamber"));
    }
    home.map(|home| home.join(".config").join("ompchamber"))
}

/// JS `sanitizeInstallScope`.
/// 白名单外的值（含空串、拼写错误）都归一为 "web"，避免生成任意文件名。
pub fn sanitize_install_scope(scope: &str) -> &str {
    match scope {
        "desktop-electron" | "vscode" | "web" | "mobile-capacitor" => scope,
        _ => "web",
    }
}

/// `crypto.randomUUID()` (v4), hand-rolled over `rand`.
/// 手工置 version/variant 位而非依赖 uuid crate，与 JS 输出格式逐字符一致。
pub fn random_uuid_v4() -> String {
    let mut bytes = rand::random::<u128>().to_le_bytes();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// JS `getOrCreateInstallId` against an explicit config dir (the runtime
/// resolves the dir from the environment seam).
/// unix 上对写入的文件附带 0o600 权限；建目录或写盘失败原样返回 `io::Error`。
pub fn get_or_create_install_id(config_dir: &Path, scope: &str) -> std::io::Result<String> {
    let normalized_scope = sanitize_install_scope(scope);
    let id_path = config_dir.join(format!("install-id-{normalized_scope}"));

    if let Ok(existing) = std::fs::read_to_string(&id_path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    let install_id = random_uuid_v4();
    std::fs::create_dir_all(config_dir)?;
    std::fs::write(&id_path, format!("{install_id}\n"))?;
    #[cfg(unix)]
    {
        use crate::os_compat::PermissionsExt;
        let _ = std::fs::set_permissions(&id_path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(install_id)
}

// ---------------------------------------------------------------------------
// version comparison
// ---------------------------------------------------------------------------

/// JS `parseVersionForComparison`: strip `v`, drop build metadata, split the
/// core on `.`, parseInt each part (invalid → 0), remember prerelease.
/// 仅在本模块（及 update.rs 的比较逻辑）内部使用。
pub(crate) struct ParsedVersion {
    /// 点分核心版本各段的 parseInt 结果（非法段为 0）。
    pub parts: Vec<i64>,
    /// 是否含 `-` 预发布后缀（预发布排序低于同核心版本）。
    pub prerelease: bool,
}

/// `Number.parseInt(part || '0', 10)` — leading whitespace, optional sign,
/// leading digits; anything else parses to 0.
/// 数字串溢出 i64 时 `unwrap_or(0)` 兜底，与 JS 语义有偏差但可忽略。
fn js_parse_int(part: &str) -> i64 {
    let trimmed = part.trim_start();
    let mut digits = String::new();
    let mut chars = trimmed.chars();
    if let Some(first) = chars.next() {
        if first == '+' || first == '-' || first.is_ascii_digit() {
            digits.push(first);
        } else {
            return 0;
        }
    }
    for ch in chars {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            break;
        }
    }
    digits.parse::<i64>().unwrap_or(0)
}

/// JS `parseVersionForComparison`：去掉 `v` 前缀与 `+build` 元数据，按 `.` 拆段
/// parseInt，并记录是否带 `-` 预发布后缀。
pub(crate) fn parse_version_for_comparison(value: Option<&str>) -> ParsedVersion {
    let raw = value.unwrap_or("");
    let without_v = raw.strip_prefix('v').unwrap_or(raw);
    let normalized = without_v.split('+').next().unwrap_or("");
    let prerelease_index = normalized.find('-');
    let core = match prerelease_index {
        Some(index) => &normalized[..index],
        None => normalized,
    };
    let parts = core.split('.').map(js_parse_int).collect();
    ParsedVersion {
        parts,
        prerelease: prerelease_index.is_some(),
    }
}

/// JS `compareVersions`: >0 when `left` is newer, <0 older, 0 equal. Any
/// prerelease sorts below the same core version.
/// 长度不等的版本按缺失段补 0 比较；数值相等但预发布标志不同时，预发布更旧。
pub(crate) fn compare_versions(left: Option<&str>, right: Option<&str>) -> i32 {
    let a = parse_version_for_comparison(left);
    let b = parse_version_for_comparison(right);
    let length = a.parts.len().max(b.parts.len());
    for index in 0..length {
        let left_part = a.parts.get(index).copied().unwrap_or(0);
        let right_part = b.parts.get(index).copied().unwrap_or(0);
        let diff = left_part.checked_sub(right_part).unwrap_or(1);
        if diff != 0 {
            return diff.clamp(-1, 1) as i32;
        }
    }
    if a.prerelease != b.prerelease {
        return if a.prerelease { -1 } else { 1 };
    }
    0
}

// ---------------------------------------------------------------------------
// changelog section parsing
// ---------------------------------------------------------------------------

/// JS `changelog.split(/^## /m).slice(1)` — sections start after each
/// line-leading `## ` separator; the preamble is dropped.
/// 分隔符行本身保留在小节开头（含行尾换行），与 JS split 的捕获行为一致。
pub(crate) fn split_h2_sections(text: &str) -> Vec<String> {
    let mut sections: Vec<String> = vec![String::new()];
    for line in text.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("## ") {
            sections.push(String::from(rest));
        } else if let Some(last) = sections.last_mut() {
            last.push_str(line);
        }
    }
    sections.remove(0);
    sections
}

/// The `^\[(\d+\.\d+\.\d+)\]` header of a changelog section, if any.
/// 仅当三段均为纯数字时返回 `Some(版本串)`，否则 `None`（该小节不参与比较）。
pub(crate) fn changelog_section_version(section: &str) -> Option<&str> {
    let rest = section.strip_prefix('[')?;
    let end = rest.find(']')?;
    let inner = &rest[..end];
    let parts: Vec<&str> = inner.split('.').collect();
    if parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
    {
        Some(inner)
    } else {
        None
    }
}
