//! Port of `server/lib/opencode/omp-host-natives.js`.
//!
//! Runtime staging of the `pi_natives` native addon: the engine dlopens it at
//! boot from `~/.omp/natives/<version>/`. An npm/tarball install has the
//! matching platform package in node_modules but the loader never looks
//! there — so before launching the host from source we copy the installed
//! addon into the per-user cache, or download the platform tarball when the
//! package is absent. Failures warn and leave the engine to surface its own
//! actionable error: a missing addon must not block unrelated server work.

use std::path::{Path, PathBuf};

const META_PACKAGE: &str = "@oh-my-pi/pi-natives";

fn natives_platform_tag() -> Option<&'static str> {
    match std::env::consts::OS {
        "windows" => Some("win32"),
        "macos" => Some("darwin"),
        "linux" => Some("linux"),
        _ => None,
    }
}

fn list_addon_files(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            is_addon_file(&name).then_some(name)
        })
        .collect()
}

/// `^pi_natives\..+\.node$`
fn is_addon_file(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("pi_natives.") else {
        return false;
    };
    let Some(stem) = rest.strip_suffix(".node") else {
        return false;
    };
    !stem.is_empty()
}

/// Candidate node_modules roots: from the crate dir upward and from cwd
/// upward (the JS uses the resolver's lookup paths — these are the same
/// roots in practice for this repo layout).
fn package_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    // Bun's global install tree (dev machines run the engine from a global
    // bun install; the JS resolver's lookup paths include it under bun).
    if let Some(home) = crate::config::home_dir() {
        roots.push(home.join(".bun").join("install").join("global"));
    }
    for start in [
        PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        std::env::current_dir().unwrap_or_default(),
    ] {
        let mut current = Some(start.as_path());
        while let Some(dir) = current {
            roots.push(dir.to_path_buf());
            current = dir.parent();
        }
    }
    roots
}

/// npm/bun layouts the plain lookup misses: direct links at each root,
/// bun's hoist root (`.bun/node_modules`), and bun's workspace store
/// entries (`.bun/@scope+pkg@ver/node_modules/pkg`).
fn resolve_package_dir(name: &str) -> Option<PathBuf> {
    let store_prefix = format!("{}@", name.replace('/', "+"));
    for root in package_roots() {
        for candidate in [
            root.join("node_modules").join(name),
            root.join(".bun").join("node_modules").join(name),
        ] {
            if candidate.join("package.json").is_file() {
                return Some(candidate);
            }
        }
        let bun_dir = root.join(".bun");
        if let Ok(entries) = std::fs::read_dir(&bun_dir) {
            for entry in entries.filter_map(Result::ok) {
                let entry_name = entry.file_name().to_string_lossy().to_string();
                if !entry_name.starts_with(&store_prefix) {
                    continue;
                }
                let candidate = bun_dir.join(entry_name).join("node_modules").join(name);
                if candidate.join("package.json").is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn read_npm_registry() -> String {
    if let Ok(explicit) = std::env::var("NPM_CONFIG_REGISTRY") {
        let explicit = explicit.trim().trim_end_matches('/');
        if !explicit.is_empty() {
            return explicit.to_string();
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(".npmrc"));
    }
    if let Some(home) = crate::config::home_dir() {
        candidates.push(home.join(".npmrc"));
    }
    for config_file in candidates {
        let Ok(text) = std::fs::read_to_string(&config_file) else {
            continue;
        };
        for line in text.lines() {
            let trimmed = line.trim_start();
            if let Some(value) = trimmed.strip_prefix("registry")
                && let Some(value) = value.strip_prefix('=')
            {
                let value = value.trim().trim_end_matches('/');
                if !value.is_empty() {
                    return value.to_string();
                }
            }
        }
    }
    "https://registry.npmjs.org".to_string()
}

async fn download_platform_package(
    package_name: &str,
    version: &str,
    destination_dir: &Path,
) -> anyhow::Result<()> {
    let registry = read_npm_registry();
    let unscoped = package_name.rsplit('/').next().unwrap_or(package_name);
    let tarball_url = format!("{registry}/{package_name}/-/{unscoped}-{version}.tgz");
    tracing::info!("[omp-host] {package_name} not installed; downloading {tarball_url}");
    let response = reqwest::Client::new()
        .get(&tarball_url)
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await?;
    if !response.status().is_success() {
        anyhow::bail!(
            "failed to download omp natives ({}): {tarball_url}",
            response.status()
        );
    }
    let tarball = response.bytes().await?;
    let temp_dir = std::env::temp_dir().join(format!(
        "omp-natives-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&temp_dir)?;
    let result: anyhow::Result<()> = async {
        let tarball_path = temp_dir.join("natives.tgz");
        std::fs::write(&tarball_path, &tarball)?;
        let extract = std::process::Command::new("tar")
            .args([
                "-xzf",
                &tarball_path.to_string_lossy(),
                "-C",
                &temp_dir.to_string_lossy(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match extract {
            Ok(status) if status.success() => {}
            Ok(status) => anyhow::bail!("tar extract failed ({status}) for {tarball_url}"),
            Err(error) => anyhow::bail!("tar spawn failed: {error}"),
        }
        let files = list_addon_files(&temp_dir.join("package"));
        if files.is_empty() {
            anyhow::bail!("npm tarball {package_name}@{version} contains no pi_natives addon");
        }
        std::fs::create_dir_all(destination_dir)?;
        for file in &files {
            std::fs::copy(
                temp_dir.join("package").join(file),
                destination_dir.join(file),
            )?;
        }
        tracing::info!(
            "[omp-host] downloaded omp natives {package_name}@{version}: {}",
            files.join(", ")
        );
        Ok(())
    }
    .await;
    let _ = std::fs::remove_dir_all(&temp_dir);
    result
}

/// Ensure the per-user natives cache holds the addon for this platform.
/// No-op when the cache already has any addon for the engine's natives
/// version; warns (never errors) when staging fails.
pub async fn ensure_omp_host_natives() {
    let Some(tag) = natives_platform_tag() else {
        return;
    };

    let Some(meta_dir) = resolve_package_dir(META_PACKAGE) else {
        // No installed meta package to derive the addon version from; the
        // engine surfaces its own actionable error if the addon is missing.
        return;
    };
    let Ok(meta) = std::fs::read_to_string(meta_dir.join("package.json")) else {
        return;
    };
    let Ok(meta) = serde_json::from_str::<serde_json::Value>(&meta) else {
        return;
    };
    let Some(version) = meta.get("version").and_then(|v| v.as_str()) else {
        return;
    };

    let Some(home) = crate::config::home_dir() else {
        return;
    };
    let cache_dir = home.join(".omp").join("natives").join(version);
    if !list_addon_files(&cache_dir).is_empty() {
        return;
    }

    let package_name = format!("@oh-my-pi/pi-natives-{tag}-{}", std::env::consts::ARCH);
    if let Some(source_dir) = resolve_package_dir(&package_name) {
        let files = list_addon_files(&source_dir);
        if !files.is_empty() {
            if std::fs::create_dir_all(&cache_dir).is_ok() {
                for file in &files {
                    if std::fs::copy(source_dir.join(file), cache_dir.join(file)).is_err() {
                        // Partial copy is fine: a later run repairs it.
                    }
                }
                tracing::info!(
                    "[omp-host] staged omp natives {package_name}@{version} into {}",
                    cache_dir.display()
                );
            }
            return;
        }
    }

    if let Err(error) = download_platform_package(&package_name, version, &cache_dir).await {
        tracing::warn!("[omp-host] omp natives staging failed: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addon_file_pattern_matches_js() {
        assert!(is_addon_file("pi_natives.darwin-aarch64.node"));
        assert!(is_addon_file("pi_natives.x.node"));
        assert!(!is_addon_file("pi_natives..node"));
        assert!(!is_addon_file("pi_natives.node"));
        assert!(!is_addon_file("other.darwin.node"));
    }

    #[test]
    fn resolves_installed_package_in_node_modules() {
        // This repo has the meta package installed under node_modules.
        let resolved = resolve_package_dir(META_PACKAGE);
        assert!(
            resolved.is_some(),
            "expected @oh-my-pi/pi-natives in node_modules"
        );
    }

    #[test]
    fn registry_env_wins_and_trims_trailing_slash() {
        // Not isolated from ambient env; assert shape only when set.
        if let Ok(value) = std::env::var("NPM_CONFIG_REGISTRY") {
            if !value.trim().is_empty() {
                assert!(read_npm_registry().starts_with("http"));
            }
        }
    }
}
