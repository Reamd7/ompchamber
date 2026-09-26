//! Port of `server/lib/walkthrough/generated.js`.
//!
//! Files that are produced by a tool rather than written by a person. Their
//! diffs are enormous, carry no intent, and are exactly the kind of content a
//! reviewer scrolls past — but they are still part of the change, so they are
//! never hidden: they are kept out of the model's input and shown in the
//! uncovered tail instead.
//!
//! The JS regexes are translated to plain string matching (no regex crate in
//! the dependency set). Case-insensitivity follows each pattern's `/i` flag
//! exactly.

/// `LOCKFILES`: exact basenames anywhere in the tree.
const LOCKFILES: &[&str] = &[
    "bun.lock",
    "bun.lockb",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "composer.lock",
    "Gemfile.lock",
    "Pipfile.lock",
    "poetry.lock",
    "uv.lock",
    "Cargo.lock",
    "go.sum",
    "mix.lock",
    "pubspec.lock",
    "flake.lock",
    "gradle.lockfile",
    "packages.lock.json",
    "deno.lock",
];

fn ends_with_ignore_case(value: &str, suffix: &str) -> bool {
    value.len() >= suffix.len()
        && value
            .get(value.len() - suffix.len()..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

/// `GENERATED_PATTERNS.some((pattern) => pattern.test(filePath))`.
fn matches_generated_pattern(path: &str) -> bool {
    // /\.min\.(js|css)$/i
    if ends_with_ignore_case(path, ".min.js") || ends_with_ignore_case(path, ".min.css") {
        return true;
    }
    // /\.(js|css)\.map$/i
    if ends_with_ignore_case(path, ".js.map") || ends_with_ignore_case(path, ".css.map") {
        return true;
    }
    // `\.generated\.[^/]+$` and `\.gen\.[^/]+$`: a "." + marker + "." run that
    // does not cross a "/" before the end of the path. Checking the final
    // path segment is equivalent: `[^/]+$` can only match inside it.
    let name = path.rsplit('/').next().unwrap_or("");
    for marker in [".generated.", ".gen."] {
        if let Some(at) = name.find(marker) {
            let after = &name[at + marker.len()..];
            if !after.is_empty() {
                return true;
            }
        }
    }
    // /(^|\/)generated\//i
    let lowered = path.to_ascii_lowercase();
    if lowered.starts_with("generated/") || lowered.contains("/generated/") {
        return true;
    }
    // /\.pb\.(go|ts|js)$/i
    if ends_with_ignore_case(path, ".pb.go")
        || ends_with_ignore_case(path, ".pb.ts")
        || ends_with_ignore_case(path, ".pb.js")
    {
        return true;
    }
    // /_pb2(_grpc)?\.py$/i
    if ends_with_ignore_case(path, "_pb2.py") || ends_with_ignore_case(path, "_pb2_grpc.py") {
        return true;
    }
    // /\.pb\.cc$|\.pb\.h$/i
    if ends_with_ignore_case(path, ".pb.cc") || ends_with_ignore_case(path, ".pb.h") {
        return true;
    }
    // /(^|\/)__snapshots__\// (case-sensitive)
    if path.starts_with("__snapshots__/") || path.contains("/__snapshots__/") {
        return true;
    }
    // /\.snap$/ (case-sensitive)
    if path.ends_with(".snap") {
        return true;
    }
    false
}

/// `isGeneratedArtifact`: whether a path is a tool-produced artifact rather
/// than authored source.
///
/// Deliberately conservative: a false positive silently removes real code from
/// the review, which is the failure this whole feature exists to prevent.
pub fn is_generated_artifact(file_path: &str) -> bool {
    if file_path.is_empty() {
        return false;
    }
    let name = file_path.rsplit('/').next().unwrap_or("");
    if LOCKFILES.contains(&name) {
        return true;
    }
    matches_generated_pattern(file_path)
}

#[cfg(test)]
mod tests {
    use super::is_generated_artifact;

    #[test]
    fn matches_lockfiles_by_exact_name_anywhere_in_the_tree() {
        assert!(is_generated_artifact("bun.lock"));
        assert!(is_generated_artifact("packages/web/package-lock.json"));
        assert!(is_generated_artifact("Cargo.lock"));
        assert!(is_generated_artifact("go.sum"));
    }

    #[test]
    fn matches_conventional_generated_output() {
        assert!(is_generated_artifact("dist/app.min.js"));
        assert!(is_generated_artifact("src/api.generated.ts"));
        assert!(is_generated_artifact("proto/user.pb.go"));
        assert!(is_generated_artifact("src/__snapshots__/App.test.tsx.snap"));
        assert!(is_generated_artifact("src/generated/client.ts"));
    }

    #[test]
    fn does_not_match_authored_source_that_merely_looks_similar() {
        // A false positive silently removes real code from the review, so these
        // near-misses matter more than the hits.
        assert!(!is_generated_artifact("src/lock.ts"));
        assert!(!is_generated_artifact("src/useLockfile.ts"));
        assert!(!is_generated_artifact("src/generator.ts"));
        assert!(!is_generated_artifact("src/minifier.ts"));
        assert!(!is_generated_artifact(
            "packages/ui/src/lib/i18n/messages/en.ts"
        ));
    }

    #[test]
    fn follows_the_case_sensitivity_of_each_pattern() {
        // `/i` patterns:
        assert!(is_generated_artifact("dist/app.MIN.JS"));
        // Case-sensitive patterns do not fold:
        assert!(!is_generated_artifact("src/__SNAPSHOTS__/a.ts"));
        assert!(!is_generated_artifact("src/App.ts.SNAP"));
        assert!(is_generated_artifact("src/App.ts.snap"));
    }
}
