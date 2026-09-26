//! Port of `server/lib/opencode/plugin-spec.js` — npm/path plugin spec
//! parsing. Pure functions; no filesystem access.

use super::http_util::resolve_path;

/// `parseNpmSpec` on a string input.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NpmSpec {
    Name {
        name: String,
        version: Option<String>,
    },
    Malformed {
        raw: String,
    },
}

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
pub(crate) fn is_exact_semver(version: &str) -> bool {
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

fn split_semver_suffix(version: &str) -> Option<(&str, Option<&str>)> {
    let idx = version.find(['-', '+']).unwrap_or(version.len());
    let (core, suffix) = version.split_at(idx);
    Some((core, (idx < version.len()).then_some(suffix)))
}

/// `isPathSpec`: '/', './', '../', '~' prefixes or a Windows absolute path.
pub(crate) fn is_path_spec(spec: &str) -> bool {
    spec.starts_with('/')
        || spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with('~')
        || is_windows_absolute(spec)
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn npm(spec: &str) -> NpmSpec {
        parse_npm_spec(spec)
    }

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

    #[test]
    fn exact_semver() {
        for version in ["1.2.3", "1.2.3-beta.1", "1.2.3+build.5"] {
            assert!(is_exact_semver(version), "version: {version}");
        }
        for version in ["^1.2.0", "latest", "", "1.2", "1", "1.2.3.4", "1.2.x"] {
            assert!(!is_exact_semver(version), "version: {version}");
        }
    }

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
