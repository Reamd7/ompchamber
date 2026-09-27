//! Port of `server/lib/quota/credentials/` — the managed-credential store
//! (`store.js`) and its provider normalizers (`providers.js`).
//!
//! Ollama Cloud and Cursor credentials live under
//! `<OMPCHAMBER_DATA_DIR || ~/.config/ompchamber>/quota/<provider>.json`:
//! 0700 directory, atomic 0600 writes via a `pid.timestamp` temp file, secrets
//! never returned through the API (status responses mask them).
//!
//! 中文说明：本模块是 `server/lib/quota/credentials/` 的 Rust 移植，实现受管
//! 凭据的存取（`store.js`）与各 provider 的凭据归一化器（`providers.js`）。
//! 凭据落在 `<OMPCHAMBER_DATA_DIR || ~/.config/ompchamber>/quota/<provider>.json`：
//! 目录权限 0700，通过 `pid.时间戳` 临时文件原子写入 0600 文件；密钥绝不
//! 通过 API 返回（status 响应只包含掩码）。

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::quota::deps::QuotaDeps;
use crate::quota::utils::field;

/// 受管 quota provider 白名单：仅这些 provider 的凭据允许写入 quota 目录；
/// 同时用于拒绝集合外的 provider ID，防止 `../` 之类的路径逃逸。
const MANAGED_QUOTA_PROVIDERS: [&str; 2] = ["ollama-cloud", "cursor"];

/// `clean` — a single-line string, trimmed; anything else is unusable.
///
/// 中文说明：只接受单行字符串并去除首尾空白；包含 `\r`/`\n` 或 trim 后
/// 为空的值一律视为不可用（返回 `None`）。
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
///
/// 中文说明：提取并清洗 `cookie` 字段；清洗失败（缺失/多行/为空）时整个
/// 凭据视为无效。成功时返回只含 `cookie` 的归一化 JSON。
pub fn normalize_ollama_cloud(value: &Value) -> Option<Value> {
    let cookie = clean(field(value, "cookie"))?;
    Some(json!({ "cookie": cookie }))
}

/// `normalizers['cursor']` — missing halves serialize as empty strings.
///
/// 中文说明：分别清洗 `accessToken` 与 `refreshToken`；两者同时缺失才视为
/// 无效，仅存在一半时缺失的那半序列化为空字符串（与 JS 行为一致）。
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

/// 凭据归一化器类型别名：输入原始 JSON，返回清洗后的凭据值；
/// 返回 `None` 表示输入不足以构成有效凭据。
pub type Normalizer = fn(&Value) -> Option<Value>;

/// 按 provider ID 查找对应的归一化器；不在受管集合内的 provider 返回 `None`。
pub fn normalizer_for(provider_id: &str) -> Option<Normalizer> {
    match provider_id {
        "ollama-cloud" => Some(normalize_ollama_cloud),
        "cursor" => Some(normalize_cursor),
        _ => None,
    }
}

/// `credentialsDirectory()` — resolved per call like the JS.
///
/// 中文说明：每次调用都重新解析目录（与 JS 一致）：优先取环境变量
/// `OMPCHAMBER_DATA_DIR`（相对路径按当前工作目录补全），否则退回
/// `<home>/.config/ompchamber`，最后追加 `quota` 子目录。
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
///
/// 中文说明：把 provider ID 映射为 `<凭据目录>/<provider>.json`；不在受管
/// 集合内的 ID（可能构成路径穿越）直接返回 `Err("Unsupported credential provider")`。
fn credential_path(deps: &QuotaDeps, provider_id: &str) -> Result<PathBuf, String> {
    if !MANAGED_QUOTA_PROVIDERS.contains(&provider_id) {
        return Err("Unsupported credential provider".to_string());
    }
    Ok(credentials_directory(deps).join(format!("{provider_id}.json")))
}

/// `readQuotaCredential` — any read/parse failure reads as absent.
///
/// 中文说明：读取并解析凭据文件，再交给归一化器清洗；路径非法、IO 失败、
/// JSON 解析失败或归一化拒绝任何一步出错都按"未配置"处理，返回 `None`。
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
///
/// 中文说明：先确保 0700 目录，再把 pretty JSON 写入 `pid.时间戳` 命名的
/// 临时文件（0600），rename 到目标路径并再次收紧权限，实现原子替换；随后
/// 模仿 JS 的 `finally { unlinkSync }` 清理临时文件（rename 成功后该删除
/// 静默失败）。非 Unix 平台退化为普通写入。IO 错误以字符串形式返回。
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
///
/// 中文说明：删除凭据文件；文件本就不存在视为成功（`Ok`），其余错误
/// 原样向上传递。
pub fn delete_quota_credential(deps: &QuotaDeps, provider_id: &str) -> Result<(), String> {
    let path = credential_path(deps, provider_id)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

/// `deleteLegacyOpenCodeGoCredential` — removed without ever reading it.
///
/// 中文说明：无条件删除遗留的 `opencode-go.json`（从不读取或解析其内容），
/// 文件不存在或删除失败均静默忽略。
pub fn delete_legacy_opencode_go_credential(deps: &QuotaDeps) {
    let path = credentials_directory(deps).join("opencode-go.json");
    match std::fs::remove_file(&path) {
        Ok(()) | Err(_) => {}
    }
}

/// `readManagedCredential`.
///
/// 中文说明：受管凭据读取入口：按 provider 找到归一化器后委托
/// `read_quota_credential`；无归一化器或读取失败都返回 `None`。
pub fn read_managed_credential(deps: &QuotaDeps, provider_id: &str) -> Option<Value> {
    let normalize = normalizer_for(provider_id)?;
    read_quota_credential(deps, provider_id, normalize)
}

/// `writeManagedCredential` — returns the post-write status payload.
///
/// 中文说明：归一化并落盘受管凭据（provider 不受管或输入无效均返回
/// `Err("Invalid credential")`），写入成功后返回最新的 status 掩码载荷。
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
///
/// 中文说明：未配置时返回 `{ "configured": false }`；已配置时返回
/// `configured: true` 与固定掩码 `"••••••••"`，cursor 额外携带
/// `hasRefreshToken`（refresh token 是否非空，供前端判断能否刷新）。
/// 密钥明文绝不出现在返回值中。
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
///
/// 中文说明：受管凭据删除入口，直接委托 `delete_quota_credential`。
pub fn delete_managed_credential(deps: &QuotaDeps, provider_id: &str) -> Result<(), String> {
    delete_quota_credential(deps, provider_id)
}

#[cfg(test)]
/// 凭据存储单元测试：0700/0600 权限与路径逃逸防护、遗留文件清理、
/// 归一化器边界行为以及 status 掩码契约。
mod tests {
    use super::*;
    use crate::quota::runtime::QuotaRuntime;
    use std::sync::Arc;

    /// 构造指向独立临时目录的 `QuotaRuntime`（`OMPCHAMBER_DATA_DIR` 指向该
    /// 目录），返回 runtime 与目录路径，供测试断言文件系统副作用。
    fn temp_runtime() -> (Arc<QuotaRuntime>, PathBuf) {
        // 进程内递增计数器，保证并发测试拿到互不冲突的临时目录名。
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
    /// 验证凭据写入/读取/删除往返成功、quota 目录与文件分别为 0700/0600
    /// 权限，且受管集合外的 provider ID（如 `../escape`）被拒绝。
    fn store_roundtrips_with_owner_only_permissions_and_rejects_escapes() {
        let (runtime, dir) = temp_runtime();
        let deps = &runtime.deps;
        write_quota_credential(deps, "ollama-cloud", &json!({"cookie": "secret"})).unwrap();

        let quota_dir = dir.join("quota");
        #[cfg(unix)]
        {
use crate::os_compat::MetadataExt;
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
    /// 验证遗留 `opencode-go.json` 即使内容非法 JSON 也被无条件删除，
    /// 且文件已缺失时再次删除不报错。
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
    /// 验证归一化器拒绝多行与空值：ollama-cloud 的 cookie 必须是非空单行；
    /// cursor 允许只有一半 token，缺失的一半序列化为空字符串。
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
    /// 验证 status 掩码契约：未配置返回 configured=false；配置后密钥只以
    /// 掩码出现且 cursor 额外报告 hasRefreshToken；无效输入的写入被拒绝。
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
