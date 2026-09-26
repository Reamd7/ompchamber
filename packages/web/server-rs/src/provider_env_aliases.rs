//! Port of `server/lib/opencode/provider-env-aliases.js`.
//!
//! Mirrors known credential env aliases into the managed engine child env so
//! connection detection and the upstream AI SDK agree on key names (e.g.
//! `GEMINI_API_KEY` present ⇒ `GOOGLE_GENERATIVE_AI_API_KEY` too). Existing
//! non-empty values are never overwritten.

use std::collections::HashMap;

const GOOGLE_API_KEY_ALIASES: [&str; 3] = [
    "GOOGLE_GENERATIVE_AI_API_KEY",
    "GOOGLE_API_KEY",
    "GEMINI_API_KEY",
];

pub fn apply_provider_env_aliases(env: &HashMap<String, String>) -> HashMap<String, String> {
    let mut next = env.clone();
    let google_value = GOOGLE_API_KEY_ALIASES
        .iter()
        .find_map(|key| next.get(*key))
        .filter(|value| !value.trim().is_empty())
        .cloned();

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrors_first_present_alias_into_all_empty_slots() {
        let env: HashMap<String, String> =
            HashMap::from([("GEMINI_API_KEY".to_string(), "k".to_string())]);
        let out = apply_provider_env_aliases(&env);
        assert_eq!(out["GOOGLE_GENERATIVE_AI_API_KEY"], "k");
        assert_eq!(out["GOOGLE_API_KEY"], "k");
        assert_eq!(out["GEMINI_API_KEY"], "k");
    }

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

    #[test]
    fn no_alias_present_leaves_env_unchanged() {
        let env: HashMap<String, String> =
            HashMap::from([("PATH".to_string(), "/bin".to_string())]);
        assert_eq!(apply_provider_env_aliases(&env), env);
    }
}
