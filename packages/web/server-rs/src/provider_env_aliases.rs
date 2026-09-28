//! Port of `server/lib/opencode/provider-env-aliases.js`.
//!
//! Mirrors known credential env aliases into the managed engine child env so
//! connection detection and the upstream AI SDK agree on key names (e.g.
//! `GEMINI_API_KEY` present ⇒ `GOOGLE_GENERATIVE_AI_API_KEY` too). Existing
//! non-empty values are never overwritten.
//!
//! 中文说明：本模块把已知的凭证环境变量别名镜像写入受管引擎子进程的环境，
//! 使连接检测与上游 AI SDK 对 key 的命名达成一致（例如存在 `GEMINI_API_KEY`
//! 时同时补上 `GOOGLE_GENERATIVE_AI_API_KEY`）。已有非空值绝不覆盖。

use std::collections::HashMap;

/// Google API key 的已知别名列表；列表顺序决定镜像源（第一个非空者胜出）。
const GOOGLE_API_KEY_ALIASES: [&str; 3] = [
    "GOOGLE_GENERATIVE_AI_API_KEY",
    "GOOGLE_API_KEY",
    "GEMINI_API_KEY",
];

/// 对环境变量表应用凭证别名镜像：取列表中第一个非空的 Google API key
/// 别名值，回填到所有仍缺失（不存在或为空）的别名槽位。
///
/// 返回新的 HashMap（输入不被修改）；无任何别名存在时原样返回副本。
pub fn apply_provider_env_aliases(env: &HashMap<String, String>) -> HashMap<String, String> {
    let mut next = env.clone();
    // 找出第一个非空的别名值作为镜像源。
    let google_value = GOOGLE_API_KEY_ALIASES
        .iter()
        .find_map(|key| next.get(*key))
        .filter(|value| !value.trim().is_empty())
        .cloned();

    // 只回填缺失/为空的槽位，绝不覆盖已有非空值。
    if let Some(google_value) = google_value {
        for key in GOOGLE_API_KEY_ALIASES {
            let missing = next.get(key).map(|v| v.trim().is_empty()).unwrap_or(true);
            if missing {
                next.insert(key.to_string(), google_value.clone());
            }
        }
    }
    next
}

/// 凭证别名镜像的单元测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证：单个已存在的别名值会镜像填充到其余所有空缺的别名槽位。
    #[test]
    fn mirrors_first_present_alias_into_all_empty_slots() {
        let env: HashMap<String, String> =
            HashMap::from([("GEMINI_API_KEY".to_string(), "k".to_string())]);
        let out = apply_provider_env_aliases(&env);
        assert_eq!(out["GOOGLE_GENERATIVE_AI_API_KEY"], "k");
        assert_eq!(out["GOOGLE_API_KEY"], "k");
        assert_eq!(out["GEMINI_API_KEY"], "k");
    }

    /// 验证：已有非空值不被覆盖，且列表顺序中靠前的别名值胜出镜像源。
    #[test]
    fn never_overwrites_existing_non_empty_values() {
        let env: HashMap<String, String> = HashMap::from([
            ("GEMINI_API_KEY".to_string(), "gemini".to_string()),
            ("GOOGLE_API_KEY".to_string(), "google".to_string()),
        ]);
        let out = apply_provider_env_aliases(&env);
        assert_eq!(out["GOOGLE_API_KEY"], "google");
        assert_eq!(out["GEMINI_API_KEY"], "gemini");
        // First present alias in list order wins the mirror.
        assert_eq!(out["GOOGLE_GENERATIVE_AI_API_KEY"], "google");
    }

    /// 验证：无任何别名存在时环境表保持不变。
    #[test]
    fn no_alias_present_leaves_env_unchanged() {
        let env: HashMap<String, String> =
            HashMap::from([("PATH".to_string(), "/bin".to_string())]);
        assert_eq!(apply_provider_env_aliases(&env), env);
    }
}
