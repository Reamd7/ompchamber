//! Port of `server/lib/quota/credentials/` — the managed-credential store
//! (`store.js`) and its provider normalizers (`providers.js`).
//!
//! Ollama Cloud and Cursor credentials live under
//! `<OMPCHAMBER_DATA_DIR || ~/.config/ompchamber>/quota/<provider>.json`:
//! 0700 directory, atomic 0600 writes via a `pid.timestamp` temp file, secrets
//! never returned through the API (status responses mask them).

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::quota::deps::QuotaDeps;
use crate::quota::utils::field;

const MANAGED_QUOTA_PROVIDERS: [&str; 2] = ["ollama-cloud", "cursor"];

/// `clean` — a single-line string, trimmed; anything else is unusable.
fn clean(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?;
    if text.contains('\r') || text.contains('\n') {
        return None;
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// `normalizers['ollama-cloud']`.
pub fn normalize_ollama_cloud(value: &Value) -> Option<Value> {
    let cookie = clean(field(value, "cookie"))?;
    Some(json!({ "cookie": cookie }))
}

/// `normalizers['cursor']` — missing halves serialize as empty strings.
pub fn normalize_cursor(value: &Value) -> Option<Value> {
    let access_token = clean(field(value, "accessToken"));
    let refresh_token = clean(field(value, "refreshToken"));
    if access_token.is_none() && refresh_token.is_none() {
        return None;
    }
    Some(json!({
        "accessToken": access_token.unwrap_or_default(),
        "refreshToken": refresh_token.unwrap_or_default(),
    }))
}

pub type Normalizer = fn(&Value) -> Option<Value>;

pub fn normalizer_for(provider_id: &str) -> Option<Normalizer> {
    match provider_id {
        "ollama-cloud" => Some(normalize_ollama_cloud),
        "cursor" => Some(normalize_cursor),
        _ => None,
    }
}

/// `credentialsDirectory()` — resolved per call like the JS.
pub fn credentials_directory(deps: &QuotaDeps) -> PathBuf {
    let base = match (deps.env)("OMPCHAMBER_DATA_DIR") {
        Some(dir) => {
            let path = PathBuf::from(dir);
            if path.is_absolute() {
                path
            } else {
                std::env::current_dir().unwrap_or_default().join(path)
            }
        }
        None => (deps.home_dir)()
            .unwrap_or_default()
            .join(".config")
            .join("ompchamber"),
    };
    base.join("quota")
}

/// `credentialPath` — rejects provider IDs outside the managed set.
fn credential_path(deps: &QuotaDeps, provider_id: &str) -> Result<PathBuf, String> {
    if !MANAGED_QUOTA_PROVIDERS.contains(&provider_id) {
        return Err("Unsupported credential provider".to_string());
    }
    Ok(credentials_directory(deps).join(format!("{provider_id}.json")))
}

/// `readQuotaCredential` — any read/parse failure reads as absent.
pub fn read_quota_credential(
    deps: &QuotaDeps,
    provider_id: &str,
    normalize: Normalizer,
) -> Option<Value> {
    let path = credential_path(deps, provider_id).ok()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    normalize(&parsed)
}

/// `writeQuotaCredential` — 0700 dir, 0600 atomic write.
pub fn write_quota_credential(
    deps: &QuotaDeps,
    provider_id: &str,
    credential: &Value,
) -> Result<(), String> {
    let target = credential_path(deps, provider_id)?;
    let directory = target
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let temporary = directory.join(format!(
        "{}.{}.{}.tmp",
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("credential"),
        std::process::id(),
        deps.now_ms()
    ));

    let write_result = (|| -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700).recursive(true);
            builder.create(&directory)?;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
            let body = format!(
                "{}\n",
                serde_json::to_string_pretty(credential).unwrap_or_default()
            );
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)
                .and_then(|mut file| std::io::Write::write_all(&mut file, body.as_bytes()))?;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
            std::fs::rename(&temporary, &target)?;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600))?;
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(&directory)?;
            let body = format!(
                "{}\n",
                serde_json::to_string_pretty(credential).unwrap_or_default()
            );
            std::fs::write(&temporary, body)?;
            std::fs::rename(&temporary, &target)?;
        }
        Ok(())
    })();

    // JS `finally { unlinkSync(temporary) }` — after a successful rename the
    // temp file is gone and the unlink is silently ignored.
    let _ = std::fs::remove_file(&temporary);
    write_result.map_err(|error| error.to_string())
}

/// `deleteQuotaCredential` — missing files are fine, other errors surface.
pub fn delete_quota_credential(deps: &QuotaDeps, provider_id: &str) -> Result<(), String> {
    let path = credential_path(deps, provider_id)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

/// `deleteLegacyOpenCodeGoCredential` — removed without ever reading it.
pub fn delete_legacy_opencode_go_credential(deps: &QuotaDeps) {
    let path = credentials_directory(deps).join("opencode-go.json");
    match std::fs::remove_file(&path) {
        Ok(()) | Err(_) => {}
    }
}

/// `readManagedCredential`.
pub fn read_managed_credential(deps: &QuotaDeps, provider_id: &str) -> Option<Value> {
    let normalize = normalizer_for(provider_id)?;
    read_quota_credential(deps, provider_id, normalize)
}

/// `writeManagedCredential` — returns the post-write status payload.
pub fn write_managed_credential(
    deps: &QuotaDeps,
    provider_id: &str,
    value: &Value,
) -> Result<Value, String> {
    let normalize = normalizer_for(provider_id).ok_or("Invalid credential")?;
    let credential = normalize(value).ok_or("Invalid credential")?;
    write_quota_credential(deps, provider_id, &credential)?;
    Ok(managed_credential_status(deps, provider_id))
}

/// `getManagedCredentialStatus` — configured flag plus masked secret.
pub fn managed_credential_status(deps: &QuotaDeps, provider_id: &str) -> Value {
    let Some(credential) = read_managed_credential(deps, provider_id) else {
        return json!({ "configured": false });
    };
    if provider_id == "cursor" {
        let has_refresh_token = field(&credential, "refreshToken")
            .and_then(Value::as_str)
            .is_some_and(|token| !token.is_empty());
        return json!({
            "configured": true,
            "hasRefreshToken": has_refresh_token,
            "secretMasked": "••••••••",
        });
    }
    json!({ "configured": true, "secretMasked": "••••••••" })
}

/// `deleteManagedCredential`.
pub fn delete_managed_credential(deps: &QuotaDeps, provider_id: &str) -> Result<(), String> {
    delete_quota_credential(deps, provider_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::runtime::QuotaRuntime;
    use std::sync::Arc;

    fn temp_runtime() -> (Arc<QuotaRuntime>, PathBuf) {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
            + COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ompchamber-quota-creds-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        let data_dir = dir.clone();
        let runtime = Arc::new(QuotaRuntime::new(crate::quota::deps::QuotaDeps {
            http: crate::quota::tests_support::unused_http(),
            now: Arc::new(|| 1_700_000_000_000),
            env: Arc::new(move |name| {
                if name == "OMPCHAMBER_DATA_DIR" {
                    Some(data_dir.to_string_lossy().into_owned())
                } else {
                    None
                }
            }),
            home_dir: Arc::new(|| None),
            read_auth: Arc::new(|| Ok(json!({}))),
            write_auth: Arc::new(|_| Ok(())),
            keychain: Arc::new(|| None),
            sqlite_value: Arc::new(|_, _| None),
        }));
        (runtime, dir)
    }

    #[test]
    fn store_roundtrips_with_owner_only_permissions_and_rejects_escapes() {
        let (runtime, dir) = temp_runtime();
        let deps = &runtime.deps;
        write_quota_credential(deps, "ollama-cloud", &json!({"cookie": "secret"})).unwrap();

        let quota_dir = dir.join("quota");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mode = std::fs::metadata(&quota_dir).unwrap().mode() & 0o777;
            assert_eq!(mode, 0o700);
            let file_mode = std::fs::metadata(quota_dir.join("ollama-cloud.json"))
                .unwrap()
                .mode()
                & 0o777;
            assert_eq!(file_mode, 0o600);
        }

        let read =
            read_quota_credential(deps, "ollama-cloud", |value| Some(value.clone())).unwrap();
        assert_eq!(read, json!({"cookie": "secret"}));

        assert_eq!(
            write_quota_credential(deps, "../escape", &json!({})),
            Err("Unsupported credential provider".to_string())
        );

        delete_quota_credential(deps, "ollama-cloud").unwrap();
        assert!(read_quota_credential(deps, "ollama-cloud", |v| Some(v.clone())).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn removes_the_obsolete_opencode_go_credential_without_parsing_it() {
        let (runtime, dir) = temp_runtime();
        let quota_dir = dir.join("quota");
        std::fs::create_dir_all(&quota_dir).unwrap();
        let legacy = quota_dir.join("opencode-go.json");
        std::fs::write(&legacy, "{not valid json").unwrap();
        delete_legacy_opencode_go_credential(&runtime.deps);
        assert!(!legacy.exists());
        // Missing legacy files are fine too.
        delete_legacy_opencode_go_credential(&runtime.deps);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn normalizers_reject_multiline_and_empty_values() {
        assert_eq!(
            normalize_ollama_cloud(&json!({"cookie": "  abc  "})),
            Some(json!({"cookie": "abc"}))
        );
        assert_eq!(normalize_ollama_cloud(&json!({"cookie": "a\nb"})), None);
        assert_eq!(normalize_ollama_cloud(&json!({"cookie": ""})), None);
        assert_eq!(normalize_ollama_cloud(&json!({})), None);

        assert_eq!(
            normalize_cursor(&json!({"accessToken": "a", "refreshToken": "r"})),
            Some(json!({"accessToken": "a", "refreshToken": "r"}))
        );
        // A refresh token alone is usable; the missing half stays an empty string.
        assert_eq!(
            normalize_cursor(&json!({"refreshToken": "r"})),
            Some(json!({"accessToken": "", "refreshToken": "r"}))
        );
        assert_eq!(normalize_cursor(&json!({})), None);
        assert_eq!(normalize_cursor(&json!("string")), None);
    }

    #[test]
    fn managed_status_masks_secrets_and_reports_cursor_refresh_presence() {
        let (runtime, dir) = temp_runtime();
        let deps = &runtime.deps;

        assert_eq!(
            managed_credential_status(deps, "ollama-cloud"),
            json!({"configured": false})
        );

        write_managed_credential(deps, "cursor", &json!({"refreshToken": "r"})).unwrap();
        assert_eq!(
            managed_credential_status(deps, "cursor"),
            json!({"configured": true, "hasRefreshToken": true, "secretMasked": "••••••••"})
        );

        write_managed_credential(deps, "ollama-cloud", &json!({"cookie": "c"})).unwrap();
        assert_eq!(
            managed_credential_status(deps, "ollama-cloud"),
            json!({"configured": true, "secretMasked": "••••••••"})
        );

        assert_eq!(
            write_managed_credential(deps, "ollama-cloud", &json!({"cookie": ""})),
            Err("Invalid credential".to_string())
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
