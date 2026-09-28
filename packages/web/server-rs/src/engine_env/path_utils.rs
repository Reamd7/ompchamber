//! Port of `server/lib/opencode/path-utils.js`.
//!
//! Shared PATH heuristics: whether a PATH string looks user-configured (vs. a
//! bare system default) and order-preserving PATH merges. Pure string
//! functions — the delimiter is passed in like the JS callers pass
//! `path.delimiter` (host-bound, `';'` on Windows, `':'` elsewhere).
//!
//! 中文说明：`server/lib/opencode/path-utils.js` 的移植，提供共享的 PATH
//! 启发式判断——一个 PATH 字符串是否看起来由用户（或其会话管理器）配置过，
//! 以及保持顺序的 PATH 合并。全部为纯字符串函数：分隔符由调用方传入，
//! 对应 JS 调用方传入的 `path.delimiter`（与宿主平台绑定，Windows 为
//! `';'`，其它平台为 `':'`）。

/// JS `TOOLCHAIN_SEGMENTS` — well-known package-manager prefixes.
/// 中文：常见包管理器/工具链的安装前缀（homebrew、pkg、pmk、snap）。
const TOOLCHAIN_SEGMENTS: [&str; 4] = ["/opt/homebrew/", "/opt/pkg/", "/opt/pmk/", "/snap/"];

/// JS `TOOLCHAIN_BASENAMES` — well-known dot-directories inside (or outside)
/// home, e.g. `~/.cargo/bin` or a repo `node_modules/.bin`.
/// 中文：家目录内外皆可出现的知名点目录名（如 `~/.cargo/bin` 中的
/// `.cargo`、仓库内 `node_modules/.bin` 中的 `node_modules`）。
const TOOLCHAIN_BASENAMES: [&str; 12] = [
    ".cargo",
    ".bun",
    ".nvm",
    ".pyenv",
    ".rbenv",
    ".sdkman",
    ".asdf",
    ".volta",
    ".fnm",
    ".local",
    ".opencode",
    "node_modules",
];

/// JS `pathLooksUserConfigured(value, home, delim)`: true when `value`
/// contains at least one segment suggesting the PATH was configured by the
/// user or their session manager — anything under home, a toolchain prefix,
/// or a well-known dot-directory component.
/// 中文：只要 PATH 中含一个"用户配置过"的信号片段即返回 true——家目录
/// 下的路径、工具链前缀，或知名点目录组件；空 PATH 恒为 false。
pub fn path_looks_user_configured(value: &str, home: &str, delim: char) -> bool {
    if value.is_empty() {
        return false;
    }

    let normalized_home = home.replace('\\', "/");
    let home_with_sep = if normalized_home.is_empty() {
        String::new()
    } else {
        format!("{normalized_home}/")
    };

    value.split(delim).any(|segment| {
        if segment.is_empty() {
            return false;
        }
        let normalized_segment = segment.replace('\\', "/");

        // Any path under the user's home directory.
        if !normalized_home.is_empty()
            && (normalized_segment == normalized_home
                || normalized_segment.starts_with(&home_with_sep))
        {
            return true;
        }

        // Well-known package-manager / toolchain prefixes.
        if TOOLCHAIN_SEGMENTS
            .iter()
            .any(|prefix| normalized_segment.starts_with(prefix))
        {
            return true;
        }

        // Well-known dot-directories inside home (e.g. ~/.cargo/bin).
        let parts: Vec<&str> = normalized_segment
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        if parts.iter().any(|part| TOOLCHAIN_BASENAMES.contains(part)) {
            return true;
        }

        false
    })
}

/// JS `mergePathValues(primary, fallback, delim)`: deduplicate segments while
/// preserving `primary`'s order, then append `fallback` segments not already
/// present.
/// 中文：去重合并——先按 primary 的顺序收段，再追加 fallback 中尚未
/// 出现的段；空段一律忽略。
pub fn merge_path_values(primary: &str, fallback: &str, delim: char) -> String {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut result: Vec<&str> = Vec::new();

    // 把 value 按 delim 拆分，跳过空段，把首次出现的段按序加入结果
    // （seen 集合同时承担去重）。
    fn add_segments<'a>(
        value: &'a str,
        delim: char,
        seen: &mut std::collections::HashSet<&'a str>,
        result: &mut Vec<&'a str>,
    ) {
        for segment in value.split(delim) {
            if !segment.is_empty() && seen.insert(segment) {
                result.push(segment);
            }
        }
    }

    add_segments(primary, delim, &mut seen, &mut result);
    add_segments(fallback, delim, &mut seen, &mut result);

    result.join(&delim.to_string())
}

/// 纯字符串函数的行为契约测试：用户配置判定与保序 PATH 合并。
#[cfg(test)]
mod tests {
    use super::*;

    /// 测试统一使用 Unix 分隔符 `:`；Windows 语义用例单独传 `;`。
    const DELIM: char = ':';

    /// 验证：空 PATH 不视为用户配置。
    #[test]
    fn empty_value_is_not_user_configured() {
        assert!(!path_looks_user_configured("", "/home/user", DELIM));
    }

    /// 验证：仅含系统默认目录（/usr/local/bin 等）的 PATH 不视为用户配置。
    #[test]
    fn minimal_system_path_is_not_user_configured() {
        assert!(!path_looks_user_configured(
            "/usr/local/bin:/usr/bin:/bin",
            "/home/user",
            DELIM
        ));
    }

    /// 验证：家目录下的 `.bun`/`.local` 路径被识别为用户配置。
    #[test]
    fn detects_paths_under_home() {
        assert!(path_looks_user_configured(
            "/home/user/.bun/bin:/usr/bin",
            "/home/user",
            DELIM
        ));
        assert!(path_looks_user_configured(
            "/home/user/.local/bin:/usr/bin",
            "/home/user",
            DELIM
        ));
    }

    /// 验证：家目录本身作为 PATH 片段也被识别。
    #[test]
    fn detects_home_directory_itself() {
        assert!(path_looks_user_configured(
            "/home/user:/usr/bin",
            "/home/user",
            DELIM
        ));
    }

    /// 验证：homebrew/pkg/snap 等工具链前缀被识别。
    #[test]
    fn detects_toolchain_prefixes() {
        assert!(path_looks_user_configured(
            "/opt/homebrew/bin:/usr/bin",
            "/home/user",
            DELIM
        ));
        assert!(path_looks_user_configured(
            "/opt/pkg/bin:/usr/bin",
            "/home/user",
            DELIM
        ));
        assert!(path_looks_user_configured(
            "/snap/bin:/usr/bin",
            "/home/user",
            DELIM
        ));
    }

    /// 验证：任意位置出现的知名点目录组件（.cargo/.nvm/.pyenv/.opencode）
    /// 都会触发识别。
    #[test]
    fn detects_dot_directory_basenames() {
        assert!(path_looks_user_configured(
            "/some/path/.cargo/bin:/usr/bin",
            "/home/user",
            DELIM
        ));
        assert!(path_looks_user_configured(
            "/some/path/.nvm/versions/node/v20/bin:/usr/bin",
            "/home/user",
            DELIM
        ));
        assert!(path_looks_user_configured(
            "/some/path/.pyenv/shims:/usr/bin",
            "/home/user",
            DELIM
        ));
        assert!(path_looks_user_configured(
            "/some/path/.opencode/bin:/usr/bin",
            "/home/user",
            DELIM
        ));
    }

    /// 验证：Windows 反斜杠路径与 `;` 分隔符下，家目录及工具链路径同样
    /// 被识别（比较前统一把 `\` 归一化为 `/`）。
    #[test]
    fn detects_windows_home_and_toolchain_paths() {
        let windows_home = "C:\\Users\\agent";
        assert!(path_looks_user_configured(
            "C:\\Users\\agent\\.bun\\bin;C:\\Windows\\System32",
            windows_home,
            ';'
        ));
        assert!(path_looks_user_configured(
            "C:\\tools\\.cargo\\bin;C:\\Windows\\System32",
            windows_home,
            ';'
        ));
    }

    /// 验证：两个输入都为空时合并结果为空字符串。
    #[test]
    fn merge_empty_inputs_yield_empty() {
        assert_eq!(merge_path_values("", "", DELIM), "");
    }

    /// 验证：fallback 为空时结果就是 primary 本身。
    #[test]
    fn merge_returns_primary_when_fallback_empty() {
        assert_eq!(merge_path_values("/a:/b", "", DELIM), "/a:/b");
    }

    /// 验证：primary 为空时结果就是 fallback 本身。
    #[test]
    fn merge_returns_fallback_when_primary_empty() {
        assert_eq!(merge_path_values("", "/a:/b", DELIM), "/a:/b");
    }

    /// 验证：合并去重并保持 primary 顺序，重复片段只保留首次出现。
    #[test]
    fn merge_deduplicates_preserving_primary_order() {
        assert_eq!(
            merge_path_values("/a:/b:/c", "/b:/d:/a", DELIM),
            "/a:/b:/c:/d"
        );
    }

    /// 验证：无重叠时 fallback 片段按原顺序全部追加。
    #[test]
    fn merge_appends_all_fallback_segments_when_no_overlap() {
        assert_eq!(merge_path_values("/a:/b", "/c:/d", DELIM), "/a:/b:/c:/d");
    }
}
