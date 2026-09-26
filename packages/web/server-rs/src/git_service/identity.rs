//! Port of `server/lib/git/identity-storage.js` (git identity profiles
//! persisted at `~/.config/ompchamber/git-identities.json`) and
//! `server/lib/git/credentials.js` (`~/.git-credentials` host/username
//! discovery).
//!
//! The storage root is injectable (`IdentityStorage::default()` pins the JS
//! home-directory path) so route tests can point it at a temp directory.

use std::path::PathBuf;

use serde_json::{Value, json};

pub struct IdentityStorage {
    root: PathBuf,
}

impl Default for IdentityStorage {
    fn default() -> Self {
        Self {
            root: crate::config::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
                .join("ompchamber"),
        }
    }
}

impl IdentityStorage {
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn storage_file(&self) -> PathBuf {
        self.root.join("git-identities.json")
    }

    fn ensure_dir(&self) {
        let _ = std::fs::create_dir_all(&self.root);
    }

    /// JS `loadProfiles` — missing file or parse failure → `{ profiles: [] }`.
    pub fn load(&self) -> Value {
        self.ensure_dir();
        let path = self.storage_file();
        let Ok(content) = std::fs::read_to_string(&path) else {
            return json!({ "profiles": [] });
        };
        match serde_json::from_str::<Value>(&content) {
            Ok(value) => value,
            Err(_) => json!({ "profiles": [] }),
        }
    }

    /// JS `saveProfiles` — pretty-printed write; failure propagates.
    pub fn save(&self, data: &Value) -> Result<(), String> {
        self.ensure_dir();
        let pretty = serde_json::to_string_pretty(data).map_err(|e| e.to_string())?;
        std::fs::write(self.storage_file(), pretty).map_err(|e| e.to_string())
    }

    pub fn get_profiles(&self) -> Vec<Value> {
        self.load()
            .get("profiles")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    pub fn get_profile(&self, id: &str) -> Option<Value> {
        self.get_profiles()
            .into_iter()
            .find(|profile| profile.get("id").and_then(Value::as_str) == Some(id))
    }

    /// JS `createProfile` — validation errors are the exact JS messages.
    pub fn create_profile(&self, profile_data: &Value) -> Result<Value, String> {
        let mut profiles = self.get_profiles();
        let text = |field: &str| {
            profile_data
                .get(field)
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_default()
        };
        let id = text("id");
        let user_name = text("userName");
        let user_email = text("userEmail");

        if profiles
            .iter()
            .any(|profile| profile.get("id").and_then(Value::as_str) == Some(id.as_str()))
        {
            return Err(format!("Profile with ID \"{}\" already exists", id));
        }
        if id.is_empty() || user_name.is_empty() || user_email.is_empty() {
            return Err("Profile must have id, userName, and userEmail".to_string());
        }

        let auth_type = {
            let value = text("authType");
            if value.is_empty() {
                "ssh".to_string()
            } else {
                value
            }
        };
        let name = {
            let value = text("name");
            if value.is_empty() {
                user_name.clone()
            } else {
                value
            }
        };
        let new_profile = json!({
            "id": id,
            "name": name,
            "userName": user_name,
            "userEmail": user_email,
            "authType": auth_type,
            "sshKey": optional_text(profile_data.get("sshKey")),
            "signCommits": profile_data.get("signCommits").cloned(),
            "signingKey": optional_text(profile_data.get("signingKey")),
            "host": optional_text(profile_data.get("host")),
            "color": non_empty_or(text("color"), "keyword"),
            "icon": non_empty_or(text("icon"), "branch"),
        });
        profiles.push(new_profile.clone());
        self.save(&json!({ "profiles": profiles }))?;
        Ok(new_profile)
    }

    /// JS `updateProfile` — shallow merge, `id` immutable.
    pub fn update_profile(&self, id: &str, updates: &Value) -> Result<Value, String> {
        let mut profiles = self.get_profiles();
        let index = profiles
            .iter()
            .position(|profile| profile.get("id").and_then(Value::as_str) == Some(id))
            .ok_or_else(|| format!("Profile with ID \"{}\" not found", id))?;

        let mut merged = profiles[index].clone();
        if let (Some(merged_map), Some(update_map)) = (merged.as_object_mut(), updates.as_object())
        {
            for (key, value) in update_map {
                merged_map.insert(key.clone(), value.clone());
            }
        }
        if let Some(original_id) = profiles[index].get("id").cloned()
            && let Some(merged_map) = merged.as_object_mut()
        {
            merged_map.insert("id".to_string(), original_id);
        }
        profiles[index] = merged.clone();
        self.save(&json!({ "profiles": profiles }))?;
        Ok(merged)
    }

    /// JS `deleteProfile`.
    pub fn delete_profile(&self, id: &str) -> Result<bool, String> {
        let profiles = self.get_profiles();
        let filtered: Vec<Value> = profiles
            .iter()
            .filter(|profile| profile.get("id").and_then(Value::as_str) != Some(id))
            .cloned()
            .collect();
        if filtered.len() == profiles.len() {
            return Err(format!("Profile with ID \"{}\" not found", id));
        }
        self.save(&json!({ "profiles": filtered }))?;
        Ok(true)
    }
}

fn optional_text(value: Option<&Value>) -> Value {
    match value.and_then(Value::as_str) {
        Some(text) if !text.is_empty() => Value::String(text.to_string()),
        _ => Value::Null,
    }
}

fn non_empty_or(value: String, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

// ---------------------------------------------------------------------------
// credentials.js
// ---------------------------------------------------------------------------

/// JS `discoverGitCredentials` — host + username pairs from
/// `~/.git-credentials` lines (`https://user@host/path…`), deduped, invalid
/// lines skipped.
pub fn discover_git_credentials(home: Option<&std::path::Path>) -> Vec<Value> {
    let home = home
        .map(PathBuf::from)
        .or_else(crate::config::home_dir)
        .unwrap_or_default();
    let credentials_path = home.join(".git-credentials");
    let Ok(content) = std::fs::read_to_string(&credentials_path) else {
        return Vec::new();
    };

    let mut credentials: Vec<Value> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(url) = parse_stored_credential_url(trimmed) else {
            continue;
        };
        let (host, username) = url;
        if host.is_empty() || username.is_empty() {
            continue;
        }
        if credentials.iter().any(|entry| {
            entry.get("host") == Some(&Value::String(host.clone()))
                && entry.get("username") == Some(&Value::String(username.clone()))
        }) {
            continue;
        }
        credentials.push(json!({ "host": host, "username": username }));
    }
    credentials
}

/// Minimal WHATWG URL parse of the stored credential line
/// (`scheme://user:pass@host/path`): hostname + pathname (empty for "/"),
/// username; percent-decoded like JS `URL`.
fn parse_stored_credential_url(line: &str) -> Option<(String, String)> {
    let (scheme, rest) = line.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(index) => (&authority[..index], &authority[index + 1..]),
        None => ("", authority),
    };
    let username = match userinfo.split_once(':') {
        Some((user, _)) => user,
        None => userinfo,
    };
    let hostname = hostport
        .split(']')
        .next_back()
        .unwrap_or(hostport)
        .rsplit(':')
        .next_back()
        .unwrap_or("")
        .to_lowercase();
    let pathname = if path.is_empty() || path == "/" {
        String::new()
    } else {
        path.to_string()
    };
    let host = format!("{}{}", hostname, pathname);
    Some((host, percent_decode(username)))
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&String::from_utf8_lossy(&bytes[i + 1..i + 3]), 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-git-identity-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rand_suffix() -> String {
        use rand::Rng;
        let mut rng = rand::rng();
        (0..8)
            .map(|_| format!("{:x}", rng.random_range(0..16)))
            .collect()
    }

    #[test]
    fn profiles_roundtrip_with_validation() {
        let root = temp_root();
        let storage = IdentityStorage::with_root(&root);
        assert_eq!(storage.get_profiles(), Vec::<Value>::new());

        let profile = storage
            .create_profile(&json!({
                "id": "work",
                "userName": "Ada",
                "userEmail": "ada@example.com",
                "sshKey": "/keys/ada",
            }))
            .expect("create");
        assert_eq!(profile.get("authType"), Some(&json!("ssh")));
        assert_eq!(profile.get("color"), Some(&json!("keyword")));
        assert_eq!(profile.get("icon"), Some(&json!("branch")));
        assert_eq!(profile.get("name"), Some(&json!("Ada")));
        assert_eq!(profile.get("sshKey"), Some(&json!("/keys/ada")));
        assert_eq!(profile.get("signCommits"), Some(&Value::Null));

        let err = storage
            .create_profile(&json!({ "id": "work", "userName": "x", "userEmail": "y" }))
            .unwrap_err();
        assert_eq!(err, "Profile with ID \"work\" already exists");
        let err = storage
            .create_profile(&json!({ "id": "partial" }))
            .unwrap_err();
        assert_eq!(err, "Profile must have id, userName, and userEmail");

        let updated = storage
            .update_profile("work", &json!({ "color": "string", "id": "changed" }))
            .expect("update");
        assert_eq!(updated.get("color"), Some(&json!("string")));
        assert_eq!(updated.get("id"), Some(&json!("work")));

        assert!(storage.update_profile("missing", &json!({})).is_err());
        assert!(storage.delete_profile("work").unwrap());
        assert!(storage.delete_profile("work").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn credentials_parse_dedupe_and_skip() {
        let root = temp_root();
        let credentials = root.join(".git-credentials");
        std::fs::write(
            &credentials,
            "https://ada@example.com\nhttps://bob@github.com/org/repo\nhttps://ada@example.com\nnot a url\nhttps://example.com\n",
        )
        .unwrap();
        let found = discover_git_credentials(Some(&root));
        assert_eq!(
            found,
            vec![
                json!({ "host": "example.com", "username": "ada" }),
                json!({ "host": "github.com/org/repo", "username": "bob" }),
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn credentials_missing_file_is_empty() {
        let root = temp_root();
        assert!(discover_git_credentials(Some(&root)).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
