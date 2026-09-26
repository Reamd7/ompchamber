//! Port of `server/lib/opencode/path-utils.js`.
//!
//! Shared PATH heuristics: whether a PATH string looks user-configured (vs. a
//! bare system default) and order-preserving PATH merges. Pure string
//! functions — the delimiter is passed in like the JS callers pass
//! `path.delimiter` (host-bound, `';'` on Windows, `':'` elsewhere).

/// JS `TOOLCHAIN_SEGMENTS` — well-known package-manager prefixes.
const TOOLCHAIN_SEGMENTS: [&str; 4] = ["/opt/homebrew/", "/opt/pkg/", "/opt/pmk/", "/snap/"];

/// JS `TOOLCHAIN_BASENAMES` — well-known dot-directories inside (or outside)
/// home, e.g. `~/.cargo/bin` or a repo `node_modules/.bin`.
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
pub fn merge_path_values(primary: &str, fallback: &str, delim: char) -> String {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut result: Vec<&str> = Vec::new();

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

#[cfg(test)]
mod tests {
    use super::*;

    const DELIM: char = ':';

    #[test]
    fn empty_value_is_not_user_configured() {
        assert!(!path_looks_user_configured("", "/home/user", DELIM));
    }

    #[test]
    fn minimal_system_path_is_not_user_configured() {
        assert!(!path_looks_user_configured(
            "/usr/local/bin:/usr/bin:/bin",
            "/home/user",
            DELIM
        ));
    }

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

    #[test]
    fn detects_home_directory_itself() {
        assert!(path_looks_user_configured(
            "/home/user:/usr/bin",
            "/home/user",
            DELIM
        ));
    }

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

    #[test]
    fn merge_empty_inputs_yield_empty() {
        assert_eq!(merge_path_values("", "", DELIM), "");
    }

    #[test]
    fn merge_returns_primary_when_fallback_empty() {
        assert_eq!(merge_path_values("/a:/b", "", DELIM), "/a:/b");
    }

    #[test]
    fn merge_returns_fallback_when_primary_empty() {
        assert_eq!(merge_path_values("", "/a:/b", DELIM), "/a:/b");
    }

    #[test]
    fn merge_deduplicates_preserving_primary_order() {
        assert_eq!(
            merge_path_values("/a:/b:/c", "/b:/d:/a", DELIM),
            "/a:/b:/c:/d"
        );
    }

    #[test]
    fn merge_appends_all_fallback_segments_when_no_overlap() {
        assert_eq!(merge_path_values("/a:/b", "/c:/d", DELIM), "/a:/b:/c:/d");
    }
}
