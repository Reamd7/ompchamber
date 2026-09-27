//! Port of `server/lib/opencode/plugin-spec.js` — npm/path plugin spec
//! parsing. Pure functions; no filesystem access.
//!
//! 中文概述：插件描述符（spec）解析——区分 npm 包名与本地文件路径两种
//! 形态，并提供精确 semver 判定与 `~`/相对路径的归一化解析。
//! 与 JS 版 `plugin-spec.js` 逐函数对应；纯函数，不做任何文件系统访问。

use super::http_util::resolve_path;

/// `parseNpmSpec` on a string input.
///
/// 中文：npm 包描述符解析结果——合法时给出包名与可选版本，
/// 非法时原样保留输入串供上层回显错误。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NpmSpec {
    /// 合法 spec：包名（scoped 形如 `@scope/name`）与可选的 `@version` 部分。
    Name {
        /// 不含版本部分的完整包名；scoped 包保留 `@scope/` 前缀。
        name: String,
        /// `@` 之后的版本片段（精确 semver、range 或 dist-tag 均可）；无版本为 `None`。
        version: Option<String>,
    },
    /// 非法 spec：空串、裸 `@`、`@scope/` 缺名、`name@` 缺版本等形态。
    Malformed {
        /// 原始输入字符串，供错误信息原样回显。
        raw: String,
    },
}

/// 解析 npm 包描述符字符串（对应 JS `parseNpmSpec`）。
///
/// scoped 形态要求 `@scope/name[@version]`，非 scoped 形态取第一个 `@`
/// 之后为版本；包名或版本缺失（含空串、裸 `@`）一律归入 [`NpmSpec::Malformed`]。
pub(crate) fn parse_npm_spec(spec: &str) -> NpmSpec {
    if let Some(scoped) = spec.strip_prefix('@') {
        // scoped: '@scope/name' or '@scope/name@version'
        let Some(slash_idx) = scoped.find('/') else {
            return NpmSpec::Malformed {
                raw: spec.to_string(),
            };
        };
        if slash_idx < 1 {
            // '@' (no scope) or '@/foo'
            return NpmSpec::Malformed {
                raw: spec.to_string(),
            };
        }
        let after_slash = &scoped[slash_idx + 1..];
        if after_slash.is_empty() {
            return NpmSpec::Malformed {
                raw: spec.to_string(),
            };
        }
        match after_slash.find('@') {
            None => NpmSpec::Name {
                name: spec.to_string(),
                version: None,
            },
            Some(at_idx) => {
                let version_part = &after_slash[at_idx + 1..];
                if version_part.is_empty() {
                    return NpmSpec::Malformed {
                        raw: spec.to_string(),
                    };
                }
                let name_part = &spec[..1 + slash_idx + 1 + at_idx];
                NpmSpec::Name {
                    name: name_part.to_string(),
                    version: Some(version_part.to_string()),
                }
            }
        }
    } else {
        // unscoped
        if spec.is_empty() {
            return NpmSpec::Malformed {
                raw: spec.to_string(),
            };
        }
        match spec.find('@') {
            None => NpmSpec::Name {
                name: spec.to_string(),
                version: None,
            },
            Some(0) => NpmSpec::Malformed {
                raw: spec.to_string(),
            }, // bare '@'
            Some(at_idx) => {
                let version_part = &spec[at_idx + 1..];
                if version_part.is_empty() {
                    return NpmSpec::Malformed {
                        raw: spec.to_string(),
                    };
                }
                NpmSpec::Name {
                    name: spec[..at_idx].to_string(),
                    version: Some(version_part.to_string()),
                }
            }
        }
    }
}

/// `isExactSemver`: `/^\d+\.\d+\.\d+([-+][\w.-]+)?$/`.
///
/// 中文：判定版本串是否为精确三段 semver（可带 prerelease/build 后缀）。
/// range（如 `^1.2.0`）、dist-tag（如 `latest`）、缺段、多段、通配符均返回 `false`。
pub(crate) fn is_exact_semver(version: &str) -> bool {
    // 匹配 JS 正则的 `[\w.-]` 字符类：字母数字、下划线、点、连字符。
    fn is_word_dot_dash(ch: char) -> bool {
        ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' || ch == '-'
    }
    let Some((core, suffix)) = split_semver_suffix(version) else {
        return false;
    };
    let mut parts = core.split('.');
    let mut count = 0;
    let mut all_digits = true;
    for part in parts.by_ref() {
        count += 1;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            all_digits = false;
            break;
        }
    }
    if !all_digits || count != 3 {
        return false;
    }
    match suffix {
        None => true,
        Some(rest) if rest.starts_with('-') || rest.starts_with('+') => {
            !rest[1..].is_empty() && rest[1..].chars().all(is_word_dot_dash)
        }
        Some(_) => false,
    }
}

/// 把版本串在首个 `-` 或 `+` 处切分为（核心三段, 可选后缀）。
/// 无后缀时第二个元素为 `None`；切分本身不会失败，恒返回 `Some`。
fn split_semver_suffix(version: &str) -> Option<(&str, Option<&str>)> {
    let idx = version.find(['-', '+']).unwrap_or(version.len());
    let (core, suffix) = version.split_at(idx);
    Some((core, (idx < version.len()).then_some(suffix)))
}

/// `isPathSpec`: '/', './', '../', '~' prefixes or a Windows absolute path.
///
/// 中文：判定 spec 是否为本地路径——以 `/`、`./`、`../`、`~` 开头，
/// 或形如盘符 / UNC 的 Windows 绝对路径。
pub(crate) fn is_path_spec(spec: &str) -> bool {
    spec.starts_with('/')
        || spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with('~')
        || is_windows_absolute(spec)
}

/// 判定 Windows 绝对路径：盘符形式（`C:` 后跟斜杠或反斜杠）或 UNC 形式
/// （双反斜杠开头）。仅按字节前缀判断，不校验后续内容。
fn is_windows_absolute(spec: &str) -> bool {
    let bytes = spec.as_bytes();
    // Drive letter: `C:\` / `C:/`
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return true;
    }
    // UNC: `\\server\share`
    bytes.len() >= 2 && bytes[0] == b'\\' && bytes[1] == b'\\'
}

/// `parsePathSpec(spec, { homedir, cwd })`.
///
/// 中文：把路径 spec 归一化为绝对路径——`~` 与 `~/x` 基于 homedir 展开，
/// `./`、`../` 相对 cwd（未提供时回落 homedir），Windows 绝对路径原样保留，
/// 其余交给 [`resolve_path`] 做词法归一化。
pub(crate) fn parse_path_spec(
    spec: &str,
    homedir: &std::path::Path,
    cwd: Option<&std::path::Path>,
) -> std::path::PathBuf {
    if spec == "~" {
        return resolve_path(&homedir.to_string_lossy());
    }
    if let Some(rest) = spec.strip_prefix("~/") {
        return resolve_path(&homedir.join(rest).to_string_lossy());
    }
    if spec.starts_with("./") || spec.starts_with("../") {
        let base = cwd.unwrap_or(homedir);
        return resolve_path(&base.join(spec).to_string_lossy());
    }
    if is_windows_absolute(spec) {
        return std::path::PathBuf::from(spec);
    }
    resolve_path(spec)
}

/// spec 解析的单元测试：覆盖与 JS `plugin-spec.js` 测试相同的输入矩阵。
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// 断言辅助：对输入执行 [`parse_npm_spec`]，等价 JS 测试里的 `parse()` 包装。
    fn npm(spec: &str) -> NpmSpec {
        parse_npm_spec(spec)
    }

    /// 验证非 scoped spec：`foo`、`foo@1.2.3`、`foo@^1.2.0`、`foo@latest` 正确切分名称与版本。
    #[test]
    fn unscoped_specs() {
        assert_eq!(
            npm("foo"),
            NpmSpec::Name {
                name: "foo".into(),
                version: None
            }
        );
        assert_eq!(
            npm("foo@1.2.3"),
            NpmSpec::Name {
                name: "foo".into(),
                version: Some("1.2.3".into())
            }
        );
        assert_eq!(
            npm("foo@^1.2.0"),
            NpmSpec::Name {
                name: "foo".into(),
                version: Some("^1.2.0".into())
            }
        );
        assert_eq!(
            npm("foo@latest"),
            NpmSpec::Name {
                name: "foo".into(),
                version: Some("latest".into())
            }
        );
    }

    /// 验证 scoped spec：`@scope/foo` 及带 semver / dist-tag 版本的形态正确解析。
    #[test]
    fn scoped_specs() {
        assert_eq!(
            npm("@scope/foo"),
            NpmSpec::Name {
                name: "@scope/foo".into(),
                version: None
            }
        );
        assert_eq!(
            npm("@scope/foo@1.2.3"),
            NpmSpec::Name {
                name: "@scope/foo".into(),
                version: Some("1.2.3".into())
            }
        );
        assert_eq!(
            npm("@scope/foo@beta"),
            NpmSpec::Name {
                name: "@scope/foo".into(),
                version: Some("beta".into())
            }
        );
    }

    /// 验证各非法输入（空串、裸 `@`、缺名、缺版本等）统一归入 `Malformed` 并保留原始串。
    #[test]
    fn malformed_specs() {
        for input in [
            "",
            "@",
            "@@",
            "foo@",
            "@scope/",
            "@scope/foo@",
            "@/foo",
            "@/",
        ] {
            assert_eq!(
                npm(input),
                NpmSpec::Malformed {
                    raw: input.to_string()
                },
                "input: {input}"
            );
        }
    }

    /// 验证精确 semver 判定：三段数字加 `-`/`+` 后缀通过；range、tag、缺段、多段、通配拒绝。
    #[test]
    fn exact_semver() {
        for version in ["1.2.3", "1.2.3-beta.1", "1.2.3+build.5"] {
            assert!(is_exact_semver(version), "version: {version}");
        }
        for version in ["^1.2.0", "latest", "", "1.2", "1", "1.2.3.4", "1.2.x"] {
            assert!(!is_exact_semver(version), "version: {version}");
        }
    }

    /// 验证路径 spec 判定：UNIX 前缀、`~`、盘符与 UNC 路径识别为路径，npm 包名不算。
    #[test]
    fn path_spec_detection() {
        assert!(is_path_spec("/abs/plugin.js"));
        assert!(is_path_spec("./local.js"));
        assert!(is_path_spec("../up.js"));
        assert!(is_path_spec("~/home.js"));
        assert!(is_path_spec("~"));
        assert!(is_path_spec("C:\\Users\\me\\plugin.js"));
        assert!(is_path_spec("\\\\server\\share\\plugin.js"));
        assert!(!is_path_spec("@scope/plugin"));
        assert!(!is_path_spec("npm-package"));
    }

    /// 验证路径 spec 解析：`~` 展开、相对 cwd 归一、绝对路径与 Windows 路径语义保持。
    #[test]
    fn path_spec_resolution() {
        let home = Path::new("/home/u");
        assert_eq!(
            parse_path_spec("~/x.js", home, None),
            Path::new("/home/u/x.js")
        );
        assert_eq!(parse_path_spec("~", home, None), Path::new("/home/u"));
        assert_eq!(
            parse_path_spec("./x.js", home, Some(Path::new("/p"))),
            Path::new("/p/x.js")
        );
        assert_eq!(
            parse_path_spec("../x.js", home, Some(Path::new("/p/a"))),
            Path::new("/p/x.js")
        );
        assert_eq!(
            parse_path_spec("/abs/x.js", home, None),
            Path::new("/abs/x.js")
        );
        assert_eq!(
            parse_path_spec("C:\\Users\\me\\plugin.js", home, None),
            Path::new("C:\\Users\\me\\plugin.js")
        );
    }
}
