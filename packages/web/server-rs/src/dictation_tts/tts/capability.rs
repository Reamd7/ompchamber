//! Port of `server/lib/tts/capability-runtime.js` — probe the local macOS
//! `say` command for available voices and share the one authoritative result
//! (a promise in the JS) with the status + speak routes.

use std::sync::Arc;

use serde_json::{Value, json};

use super::language_detect::VoiceEntry;

/// The shared `sayTTSCapability` promise. `get()` awaits the single probe
/// started at construction (JS `index.js` starts it at boot and every caller
/// awaits the same promise).
#[derive(Clone)]
pub struct SayCapabilityProbe {
    cell: Arc<tokio::sync::OnceCell<Value>>,
}

impl SayCapabilityProbe {
    /// Start the probe for the given platform (`processLike.platform`).
    pub fn spawn(platform: &str) -> Self {
        let cell = Arc::new(tokio::sync::OnceCell::new());
        let probe = Self { cell };
        let platform = platform.to_string();
        let cell = Arc::clone(&probe.cell);
        tokio::spawn(async move {
            let capability = detect_say_tts_capability(&platform).await;
            let _ = cell.set(capability);
        });
        probe
    }

    /// An already-resolved probe (tests / deterministic wiring).
    pub fn resolved(value: Value) -> Self {
        let cell = Arc::new(tokio::sync::OnceCell::new());
        let _ = cell.set(value);
        Self { cell }
    }

    pub async fn get(&self) -> Value {
        self.cell
            .get_or_init(|| async {
                json!({ "available": false, "voices": [], "reason": "Not checked" })
            })
            .await
            .clone()
    }
}

/// `detectSayTtsCapability`.
pub async fn detect_say_tts_capability(platform: &str) -> Value {
    if platform == "darwin" {
        match probe_say_voices().await {
            Ok(voices) => {
                tracing::info!("macOS Say TTS available with {} voices", voices.len());
                json!({ "available": true, "voices": voices })
            }
            Err(_) => {
                json!({ "available": false, "voices": [], "reason": "say command not available" })
            }
        }
    } else {
        json!({ "available": false, "voices": [], "reason": "Not macOS" })
    }
}

/// `say -v "?"` → the `voices` array (`{name, locale}` per line that carries
/// `name <locale> # comment`).
async fn probe_say_voices() -> Result<Vec<Value>, String> {
    let output = tokio::process::Command::new("say")
        .arg("-v")
        .arg("?")
        .output()
        .await
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!("say exited with {}", output.status));
    }
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let mut voices = Vec::new();
    for line in stdout.split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(voice) = parse_say_voice_line(line) {
            voices.push(json!({ "name": voice.name, "locale": voice.locale }));
        }
    }
    Ok(voices)
}

/// The JS line regex `/^(.+?)\s+([a-zA-Z]{2}_[a-zA-Z]{2,3})\s+#/`: a name,
/// whitespace, a `xx_YY(YY)` locale, whitespace, then the `#` that starts the
/// comment column of `say -v '?'` output.
fn parse_say_voice_line(line: &str) -> Option<VoiceEntry> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    // Find the locale token: exactly one token is directly followed by the
    // `#` comment token.
    for index in 0..tokens.len().saturating_sub(1) {
        if tokens[index + 1] != "#" || !is_say_locale(tokens[index]) {
            continue;
        }
        if index == 0 {
            // The lazy `(.+?)\s+` requires a non-empty name.
            return None;
        }
        let name = tokens[..index].join(" ");
        return Some(VoiceEntry {
            name: name.trim().to_string(),
            locale: tokens[index].to_string(),
        });
    }
    None
}

fn is_say_locale(token: &str) -> bool {
    let Some((language, region)) = token.split_once('_') else {
        return false;
    };
    language.len() == 2
        && language.chars().all(|c| c.is_ascii_alphabetic())
        && (region.len() == 2 || region.len() == 3)
        && region.chars().all(|c| c.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_voice_lines() {
        // Cyrillic names parse fine; only the ASCII locale token matters.
        let cyrillic = "Лєся              uk_UA    # Привіт";
        let voice = parse_say_voice_line(cyrillic).unwrap();
        assert_eq!(voice.locale, "uk_UA");

        // Lines without the comment marker are dropped.
        assert!(parse_say_voice_line("Maged             ar_SA").is_none());
        // A bare locale with no name is dropped.
        assert!(parse_say_voice_line("en_US    # x").is_none());
    }

    #[test]
    fn locale_shape_matches_the_js_regex() {
        assert!(is_say_locale("en_US"));
        assert!(is_say_locale("pt_BR"));
        assert!(!is_say_locale("ar_001")); // digits are not [a-zA-Z]
        assert!(!is_say_locale("enus"));
        assert!(!is_say_locale("e_US"));
    }

    #[tokio::test]
    async fn non_darwin_platforms_report_not_macos() {
        let capability = detect_say_tts_capability("linux").await;
        assert_eq!(capability["reason"], "Not macOS");
        assert_eq!(capability["available"], false);
    }

    #[tokio::test]
    async fn resolved_probe_shares_one_result() {
        let probe = SayCapabilityProbe::resolved(json!({
            "available": true,
            "voices": [{ "name": "Samantha", "locale": "en_US" }]
        }));
        let first = probe.get().await;
        let second = probe.get().await;
        assert_eq!(first, second);
        assert_eq!(first["voices"][0]["name"], "Samantha");
    }
}
