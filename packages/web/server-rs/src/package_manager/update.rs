//! Port of the update half of `package-manager.js`: `getUpdateCommand`,
//! `getCurrentVersion`, the GitHub-releases latest-version check, changelog
//! slicing, the optional hosted update API, `checkForUpdates`, and the CLI
//! `executeUpdate` spawn.

use serde::Serialize;
use serde_json::{Value, json};

use super::PackageManagerRuntime;
use super::detect::quote_command;
use super::http::{HttpBody, HttpMethod, HttpRequest};
use super::paths::{
    changelog_section_version, compare_versions, get_ompchamber_config_dir,
    get_or_create_install_id, home_dir, map_arch, map_platform, normalize_app_type, normalize_arch,
    normalize_device_class, normalize_platform, split_h2_sections,
};

/// JS `checkForUpdates(options)` inputs (route query params / CLI options).
#[derive(Debug, Clone, Default)]
pub struct CheckForUpdatesOptions {
    pub current_version: Option<String>,
    pub app_type: Option<String>,
    pub device_class: Option<String>,
    pub platform: Option<String>,
    pub arch: Option<String>,
    pub instance_mode: Option<String>,
    pub install_id: Option<String>,
    /// JS `options.reportUsage !== false` — defaults to true.
    pub report_usage: Option<bool>,
}

/// JS `executeUpdate(pm, options)` inputs.
#[derive(Debug, Clone, Default)]
pub struct ExecuteUpdateOptions {
    pub version: Option<String>,
    pub silent: bool,
}

/// JS `checkForUpdates()` result (JSON shape; `undefined` fields are omitted
/// exactly like `res.json()` drops them).
#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(rename = "currentVersion")]
    pub current_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(rename = "releaseUrl", skip_serializing_if = "Option::is_none")]
    pub release_url: Option<String>,
    #[serde(rename = "downloadUrl", skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
    #[serde(rename = "packageManager", skip_serializing_if = "Option::is_none")]
    pub package_manager: Option<String>,
    #[serde(rename = "updateCommand", skip_serializing_if = "Option::is_none")]
    pub update_command: Option<String>,
    #[serde(
        rename = "nextSuggestedCheckInSec",
        skip_serializing_if = "Option::is_none"
    )]
    pub next_suggested_check_in_sec: Option<serde_json::Number>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// JS `executeUpdate()` result.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateExecution {
    pub success: bool,
    #[serde(rename = "exitCode")]
    pub exit_code: Option<i32>,
}

/// Internal shape of `checkForUpdatesFromApi`'s successful return.
#[derive(Debug, Clone)]
struct RemoteUpdate {
    available: bool,
    version: String,
    body: Option<String>,
    release_url: String,
    download_url: Option<String>,
    next_suggested_check_in_sec: Option<serde_json::Number>,
}

/// JS truthiness for decoded JSON values (`Boolean(value)`).
fn js_truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false))
        && !value.as_str().is_some_and(str::is_empty)
        && !value.as_f64().is_some_and(|number| number == 0.0)
        && !value.as_i64().is_some_and(|number| number == 0)
        && !value.as_u64().is_some_and(|number| number == 0)
}

/// JS `/^OMPChamber-.+-android\.apk$/i`.
fn is_canonical_android_apk(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.starts_with("ompchamber-")
        && lower.ends_with("-android.apk")
        && lower.len() > "ompchamber--android.apk".len()
}

impl PackageManagerRuntime {
    /// JS `getUpdateCommand(pm = detectPackageManager(), version = null)`.
    pub async fn get_update_command(&self, pm: Option<&str>, version: Option<&str>) -> String {
        let pm = match pm {
            Some(pm) => pm.to_string(),
            None => self.detect_package_manager().await,
        };
        let pm_command = quote_command(&self.resolve_package_manager_command(&pm).await);
        let trimmed_version = version.map(str::trim).filter(|value| !value.is_empty());
        let tarball_url = match trimmed_version {
            Some(version) => format!(
                "https://github.com/{}/releases/download/v{version}/ompchamber-web-{version}.tgz",
                super::GITHUB_REPO
            ),
            None => super::RELEASE_TARBALL_LATEST_URL.to_string(),
        };
        match pm.as_str() {
            "pnpm" | "bun" => format!("{pm_command} add -g {tarball_url}"),
            "yarn" => format!("{pm_command} global add {tarball_url}"),
            _ => format!("{pm_command} install -g {tarball_url}"),
        }
    }

    /// JS `getCurrentVersion()` — reads `<web package>/package.json`.
    pub fn get_current_version(&self) -> String {
        let pkg_path = self.package_root().join("package.json");
        let Ok(content) = std::fs::read_to_string(&pkg_path) else {
            return "unknown".to_string();
        };
        let Ok(pkg) = serde_json::from_str::<Value>(&content) else {
            return "unknown".to_string();
        };
        pkg.get("version")
            .and_then(Value::as_str)
            .filter(|version| !version.is_empty())
            .map(String::from)
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// JS `getLatestVersion()` — latest GitHub release tag (no `v`).
    async fn get_latest_version(&self) -> Option<String> {
        let request = HttpRequest {
            method: HttpMethod::Get,
            url: format!("{}/latest", super::GITHUB_RELEASES_API_URL),
            headers: vec![(
                "Accept".to_string(),
                "application/vnd.github+json".to_string(),
            )],
            body: HttpBody::None,
        };
        let response = self.transport.send(request).await.ok()?;
        if !response.is_ok() {
            return None;
        }
        let data = response.json()?;
        let tag = data
            .get("tag_name")
            .and_then(Value::as_str)
            .map(|tag| tag.strip_prefix('v').unwrap_or(tag).to_string())
            .unwrap_or_default();
        (!tag.is_empty()).then_some(tag)
    }

    /// JS `resolveAndroidApkUrl`.
    async fn resolve_android_apk_url(
        &self,
        version: &str,
        candidate_url: Option<&str>,
    ) -> Option<String> {
        if let Some(candidate) = candidate_url
            && let Ok(parsed) = url::Url::parse(candidate)
            && parsed.path().to_lowercase().ends_with(".apk")
        {
            return Some(candidate.to_string());
        }

        let request = HttpRequest {
            method: HttpMethod::Get,
            url: format!("{}/tags/v{version}", super::GITHUB_RELEASES_API_URL),
            headers: vec![
                (
                    "Accept".to_string(),
                    "application/vnd.github+json".to_string(),
                ),
                (
                    "User-Agent".to_string(),
                    "ompchamber-update-check".to_string(),
                ),
            ],
            body: HttpBody::None,
        };
        let response = self.transport.send(request).await.ok()?;
        if !response.is_ok() {
            return None;
        }
        let release = response.json()?;
        let apk_assets: Vec<&Value> = release
            .get("assets")
            .and_then(Value::as_array)
            .map(|assets| {
                assets
                    .iter()
                    .filter(|asset| {
                        let name = asset.get("name").and_then(Value::as_str);
                        let download_url =
                            asset.get("browser_download_url").and_then(Value::as_str);
                        name.is_some_and(|name| name.to_lowercase().ends_with(".apk"))
                            && download_url.is_some()
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let canonical = apk_assets.iter().find(|asset| {
            asset
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(is_canonical_android_apk)
        });
        canonical
            .or_else(|| apk_assets.first())
            .and_then(|asset| asset.get("browser_download_url"))
            .and_then(Value::as_str)
            .map(String::from)
    }

    /// JS `checkForUpdatesFromApi` — the optional hosted update-check API.
    /// Returns `None` when disabled (no `OMPCHAMBER_UPDATE_API_URL`) or when
    /// anything fails, exactly like the JS `catch { return null }` paths.
    async fn check_for_updates_from_api(
        &self,
        current_version: &str,
        options: &CheckForUpdatesOptions,
    ) -> Option<RemoteUpdate> {
        let update_check_url = self
            .env_var("OMPCHAMBER_UPDATE_API_URL")
            .unwrap_or_default();
        if update_check_url.is_empty() {
            return None;
        }

        let app_type = normalize_app_type(options.app_type.as_deref());
        let host_platform = map_platform(super::paths::process_platform());
        let host_arch = map_arch(super::paths::process_arch());
        let should_trust_client_platform = app_type == "desktop-electron"
            || app_type == "vscode"
            || app_type == "mobile-capacitor";
        let platform = if should_trust_client_platform {
            normalize_platform(options.platform.as_deref())
        } else {
            host_platform
        };
        let arch = if should_trust_client_platform {
            normalize_arch(options.arch.as_deref())
        } else {
            host_arch
        };
        let report_usage = options.report_usage.unwrap_or(true);
        let mut payload = json!({
            "appType": app_type,
            "deviceClass": normalize_device_class(options.device_class.as_deref()),
            "platform": platform,
            "arch": arch,
            "channel": "stable",
            "currentVersion": current_version,
            "instanceMode": options
                .instance_mode
                .as_deref()
                .filter(|mode| !mode.is_empty())
                .unwrap_or("unknown"),
            "reportUsage": report_usage,
        });
        if report_usage {
            let install_id = match options.install_id.as_deref() {
                Some(install_id) if !install_id.is_empty() => install_id.to_string(),
                _ => {
                    // JS `getOrCreateInstallId(appType)` — persist under the
                    // OMPChamber config dir; a failure aborts like the catch.
                    let home = home_dir()?;
                    let config_dir =
                        get_ompchamber_config_dir(Some(&home), self.env_var("APPDATA").as_deref())?;
                    get_or_create_install_id(&config_dir, app_type).ok()?
                }
            };
            payload["installId"] = Value::String(install_id);
        }

        let request = HttpRequest {
            method: HttpMethod::Post,
            url: update_check_url,
            headers: vec![
                ("Accept".to_string(), "application/json".to_string()),
                ("Content-Type".to_string(), "application/json".to_string()),
            ],
            body: HttpBody::Json(payload.to_string()),
        };
        let response = self.transport.send(request).await.ok()?;
        if !response.is_ok() {
            return None;
        }
        let data = response.json()?;
        let latest_version = data.get("latestVersion").and_then(Value::as_str)?;

        let version_comparison = compare_versions(Some(latest_version), Some(current_version));
        if version_comparison < 0 {
            return None;
        }

        let release_url = format!("{}/tag/v{latest_version}", super::GITHUB_RELEASES_URL);
        let download_url = data
            .get("downloadUrl")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| {
                data.get("download")
                    .and_then(|download| download.get("url"))
                    .and_then(Value::as_str)
                    .map(String::from)
            });
        let update_available =
            data.get("updateAvailable").is_some_and(js_truthy) && version_comparison > 0;
        let mobile_download_url =
            if update_available && app_type == "mobile-capacitor" && platform == "android" {
                self.resolve_android_apk_url(latest_version, download_url.as_deref())
                    .await
            } else {
                None
            };

        Some(RemoteUpdate {
            available: update_available,
            version: latest_version.to_string(),
            body: data
                .get("releaseNotes")
                .and_then(Value::as_str)
                .map(String::from),
            release_url: data
                .get("releaseNotesUrl")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or(release_url),
            download_url: mobile_download_url,
            next_suggested_check_in_sec: data
                .get("nextSuggestedCheckInSec")
                .and_then(Value::as_number)
                .cloned(),
        })
    }

    /// JS `fetchChangelogNotes(fromVersion, toVersion)`.
    async fn fetch_changelog_notes(&self, from_version: &str, to_version: &str) -> Option<String> {
        let request = HttpRequest {
            method: HttpMethod::Get,
            url: super::CHANGELOG_URL.to_string(),
            headers: Vec::new(),
            body: HttpBody::None,
        };
        let response = self.transport.send(request).await.ok()?;
        if !response.is_ok() {
            return None;
        }
        let changelog = response.text();
        let sections = split_h2_sections(&changelog);

        let relevant: Vec<&String> = sections
            .iter()
            .filter(|section| {
                let Some(version) = changelog_section_version(section) else {
                    return false;
                };
                compare_versions(Some(version), Some(from_version)) > 0
                    && compare_versions(Some(version), Some(to_version)) <= 0
            })
            .collect();

        if relevant.is_empty() {
            return None;
        }

        Some(
            relevant
                .iter()
                .map(|section| format!("## {}", section.trim()))
                .collect::<Vec<_>>()
                .join("\n\n"),
        )
    }

    /// Test-only exposure of the private changelog fetch.
    #[cfg(test)]
    pub(crate) async fn fetch_changelog_notes_for_tests(
        &self,
        from_version: &str,
        to_version: &str,
    ) -> Option<String> {
        self.fetch_changelog_notes(from_version, to_version).await
    }

    /// JS `checkForUpdates(options)`.
    pub async fn check_for_updates(&self, options: CheckForUpdatesOptions) -> UpdateInfo {
        let current_version = options
            .current_version
            .clone()
            .filter(|version| !version.is_empty())
            .unwrap_or_else(|| self.get_current_version());
        let pm = self.detect_package_manager().await;
        let app_type = normalize_app_type(options.app_type.as_deref());
        let platform = normalize_platform(options.platform.as_deref());

        if current_version != "unknown"
            && let Some(mut remote) = self
                .check_for_updates_from_api(&current_version, &options)
                .await
        {
            if remote.available && app_type == "web" {
                let npm_latest = self.get_latest_version().await;
                // JS `!npmLatest || compareVersions(npmLatest, remote.version) < 0`.
                let stale = match &npm_latest {
                    None => true,
                    Some(npm_latest) => {
                        compare_versions(Some(npm_latest), Some(&remote.version)) < 0
                    }
                };
                if stale {
                    remote.available = false;
                }
            }
            return UpdateInfo {
                available: remote.available,
                version: Some(remote.version),
                current_version,
                body: remote.body,
                release_url: Some(remote.release_url),
                download_url: remote.download_url,
                package_manager: Some(pm),
                update_command: Some("ompchamber update".to_string()),
                next_suggested_check_in_sec: remote.next_suggested_check_in_sec,
                error: None,
            };
        }

        let latest_version = self.get_latest_version().await;

        if latest_version.is_none() || current_version == "unknown" {
            return UpdateInfo {
                available: false,
                version: None,
                current_version,
                error: Some("Unable to determine versions".to_string()),
                body: None,
                release_url: None,
                download_url: None,
                package_manager: None,
                update_command: None,
                next_suggested_check_in_sec: None,
            };
        }
        let latest_version = latest_version.unwrap_or_default();

        let available = compare_versions(Some(&latest_version), Some(&current_version)) > 0;
        let (changelog, download_url) = if available {
            let changelog = self
                .fetch_changelog_notes(&current_version, &latest_version)
                .await;
            let download_url = if app_type == "mobile-capacitor" && platform == "android" {
                self.resolve_android_apk_url(&latest_version, None).await
            } else {
                None
            };
            (changelog, download_url)
        } else {
            (None, None)
        };

        UpdateInfo {
            available,
            version: Some(latest_version.clone()),
            current_version,
            body: changelog,
            release_url: Some(format!(
                "{}/tag/v{latest_version}",
                super::GITHUB_RELEASES_URL
            )),
            download_url,
            package_manager: Some(pm),
            // Show our CLI command, not raw package manager command.
            update_command: Some("ompchamber update".to_string()),
            next_suggested_check_in_sec: None,
            error: None,
        }
    }

    /// JS `executeUpdate(pm = detectPackageManager(), options)`.
    pub async fn execute_update(
        &self,
        pm: Option<&str>,
        options: ExecuteUpdateOptions,
    ) -> UpdateExecution {
        let pm = match pm {
            Some(pm) => pm.to_string(),
            None => self.detect_package_manager().await,
        };
        let command = self
            .get_update_command(Some(&pm), options.version.as_deref())
            .await;
        if !options.silent {
            println!("Updating {} using {pm}...", super::PACKAGE_NAME);
            println!("Running: {command}");
        }

        let shell = if super::paths::is_windows() {
            self.env_var("ComSpec")
                .filter(|shell| !shell.is_empty())
                .unwrap_or_else(|| "cmd.exe".to_string())
        } else {
            "sh".to_string()
        };
        let shell_flag = if super::paths::is_windows() {
            "/c"
        } else {
            "-c"
        };
        let mut builder = tokio::process::Command::new(&shell);
        builder.arg(shell_flag).arg(&command);
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            builder.creation_flags(CREATE_NO_WINDOW);
        }
        let status = builder.status().await.ok();
        UpdateExecution {
            success: status.and_then(|status| status.code()) == Some(0),
            exit_code: status.and_then(|status| status.code()),
        }
    }
}
