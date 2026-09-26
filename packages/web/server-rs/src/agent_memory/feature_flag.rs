//! Port of `server/lib/agent-memory/feature-flag.js`.
//!
//! Whether agent memory exists at all in this build. The feature is complete
//! but not released: it ships dark so it can be tested against real work
//! without appearing to users who have not asked for it. With the flag unset
//! there is no tool, no routes, no session index and no settings row — not a
//! switch left in the off position, which would invite someone to turn on
//! something unannounced.
//!
//! Read per call rather than captured at startup, so a process started with
//! the variable set is the only thing that decides.

/// `isAgentMemoryFeatureAvailable()`.
pub fn is_agent_memory_feature_available() -> bool {
    let Ok(raw) = std::env::var("OMPCHAMBER_MEMORY_ENABLE") else {
        return false;
    };
    matches!(
        raw.trim().to_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};

    /// Environment mutation is process-global; serialize the flag tests.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
        &LOCK
    }

    fn with_var(value: Option<&str>, run: impl FnOnce() -> bool) -> bool {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let original = std::env::var("OMPCHAMBER_MEMORY_ENABLE").ok();
        match value {
            Some(v) => unsafe { std::env::set_var("OMPCHAMBER_MEMORY_ENABLE", v) },
            None => unsafe { std::env::remove_var("OMPCHAMBER_MEMORY_ENABLE") },
        }
        let outcome = run();
        match original {
            Some(v) => unsafe { std::env::set_var("OMPCHAMBER_MEMORY_ENABLE", v) },
            None => unsafe { std::env::remove_var("OMPCHAMBER_MEMORY_ENABLE") },
        }
        outcome
    }

    #[test]
    fn closed_when_the_variable_is_unset() {
        assert!(!with_var(None, is_agent_memory_feature_available));
    }

    #[test]
    fn opens_for_the_usual_truthy_spellings() {
        for value in ["1", "true", "TRUE", "yes", "on", " true "] {
            assert!(
                with_var(Some(value), is_agent_memory_feature_available),
                "should open for {value:?}"
            );
        }
    }

    #[test]
    fn stays_closed_for_anything_else_including_false() {
        for value in ["", "0", "false", "no", "off", "maybe"] {
            assert!(
                !with_var(Some(value), is_agent_memory_feature_available),
                "should stay closed for {value:?}"
            );
        }
    }

    #[test]
    fn is_read_per_call() {
        assert!(!with_var(None, is_agent_memory_feature_available));
        assert!(with_var(Some("1"), is_agent_memory_feature_available));
    }
}
