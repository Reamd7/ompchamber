//! Port of `server/lib/tts/stt.js` — proxy an audio buffer to any
//! OpenAI-compatible `/v1/audio/transcriptions` endpoint.
//!
//! The JS uses the OpenAI Node SDK; the HTTP call is re-created here with
//! reqwest (the crate has no `multipart` feature, so the form body is built
//! by hand — field order and filenames match what the SDK sends: `file`,
//! `model`, `response_format`, and an optional `language`). The seam trait
//! lets route and dictation-session tests inject fakes.

use base64::Engine;
use futures::future::BoxFuture;
use serde_json::Value;

use super::base_url::normalize_custom_openai_base_url;

/// `transcribeAudio` inputs.
#[derive(Debug, Clone)]
pub struct TranscribeParams {
    pub audio_buffer: Vec<u8>,
    pub mime_type: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub language: Option<String>,
}

pub type TranscribeFuture = BoxFuture<'static, Result<String, String>>;

/// Injectable `transcribeAudio`.
pub trait Transcriber: Send + Sync {
    fn transcribe(&self, params: TranscribeParams) -> TranscribeFuture;
}

/// `mimeTypeToExt`.
pub fn mime_type_to_ext(mime_type: &str) -> &'static str {
    let base = mime_type.split(';').next().unwrap_or("").trim();
    match base.to_ascii_lowercase().as_str() {
        "audio/webm" => "webm",
        "audio/ogg" => "ogg",
        "audio/wav" | "audio/wave" => "wav",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/mp4" => "mp4",
        "audio/flac" => "flac",
        _ => "webm",
    }
}

fn random_boundary() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    format!(
        "----ompchamber{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

fn form_part(boundary: &str, name: &str, value: &str, out: &mut Vec<u8>) {
    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    out.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
    );
    out.extend_from_slice(value.as_bytes());
    out.extend_from_slice(b"\r\n");
}

fn file_part(boundary: &str, filename: &str, mime_type: &str, bytes: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    out.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    out.extend_from_slice(format!("Content-Type: {mime_type}\r\n\r\n").as_bytes());
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\r\n");
}

/// Production `transcribeAudio` over reqwest.
pub struct HttpTranscriber {
    pub http: reqwest::Client,
}

impl Default for HttpTranscriber {
    fn default() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Transcriber for HttpTranscriber {
    fn transcribe(&self, params: TranscribeParams) -> TranscribeFuture {
        let http = self.http.clone();
        Box::pin(async move {
            let normalized_base_url =
                match normalize_custom_openai_base_url(params.base_url.as_deref()) {
                    Ok(value) => value,
                    Err(error) => return Err(error),
                };
            let Some(base_url) = normalized_base_url else {
                return Err("Custom server URL is required".to_string());
            };

            let api_key = params
                .api_key
                .clone()
                .filter(|key| !key.is_empty())
                .or_else(|| {
                    std::env::var("OPENAI_API_KEY")
                        .ok()
                        .filter(|key| !key.is_empty())
                })
                .unwrap_or_else(|| "not-required".to_string());

            // Derive a sensible filename extension from the MIME type so the
            // server can infer the codec when it isn't explicit.
            let filename = format!("audio.{}", mime_type_to_ext(&params.mime_type));

            let boundary = random_boundary();
            let mut body = Vec::with_capacity(params.audio_buffer.len() + 512);
            file_part(
                &boundary,
                &filename,
                &params.mime_type,
                &params.audio_buffer,
                &mut body,
            );
            form_part(&boundary, "model", &params.model, &mut body);
            form_part(&boundary, "response_format", "json", &mut body);
            if let Some(language) = params.language.as_deref() {
                form_part(&boundary, "language", language, &mut body);
            }
            body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

            let url = format!("{base_url}/audio/transcriptions");
            let mut request = http
                .post(&url)
                .header(
                    reqwest::header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .header(reqwest::header::AUTHORIZATION, format!("Bearer {api_key}"))
                .body(body);
            // `void 0` retry config: the SDK does not retry here either.
            request = request.timeout(std::time::Duration::from_secs(600));

            let response = request
                .send()
                .await
                .map_err(|error| format!("Failed to transcribe audio: {error}"))?;
            let status = response.status();
            let bytes = response
                .bytes()
                .await
                .map_err(|error| format!("Failed to transcribe audio: {error}"))?;
            if !status.is_success() {
                // Approximation of the SDK's `APIError.message`
                // (`${status} ${statusText}`).
                return Err(format!("{status}"));
            }
            let parsed: Value = serde_json::from_slice(&bytes)
                .map_err(|error| format!("Failed to parse transcription response: {error}"))?;
            Ok(parsed
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string())
        })
    }
}

/// Decode a client `chunk.audio` base64 payload. Node's
/// `Buffer.from(input, 'base64')` is lenient: it skips characters outside the
/// alphabet instead of failing, so a noisy payload decodes to whatever valid
/// run it contains rather than rejecting the chunk.
pub fn lenient_base64_decode(input: &str) -> Vec<u8> {
    let filtered: String = input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '/')
        .collect();
    // Re-pad to a multiple of four so the engine accepts any remainder.
    let mut padded = filtered;
    while !padded.len().is_multiple_of(4) {
        padded.push('=');
    }
    base64::engine::general_purpose::STANDARD
        .decode(padded.as_bytes())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_mime_types_to_extensions() {
        assert_eq!(mime_type_to_ext("audio/webm"), "webm");
        assert_eq!(mime_type_to_ext("audio/webm; codecs=opus"), "webm");
        assert_eq!(mime_type_to_ext("audio/ogg"), "ogg");
        assert_eq!(mime_type_to_ext("audio/wave"), "wav");
        assert_eq!(mime_type_to_ext("audio/mpeg"), "mp3");
        assert_eq!(mime_type_to_ext("audio/MP4"), "mp4");
        assert_eq!(mime_type_to_ext("audio/flac"), "flac");
        assert_eq!(mime_type_to_ext("application/octet-stream"), "webm");
        assert_eq!(mime_type_to_ext(""), "webm");
    }

    #[test]
    fn decodes_standard_and_lenient_base64() {
        use base64::engine::general_purpose::STANDARD;
        let payload: Vec<u8> = (0..64u8).collect();
        let encoded = STANDARD.encode(&payload);
        assert_eq!(lenient_base64_decode(&encoded), payload);
        // Whitespace and stray characters are skipped, like Node: the
        // dropped tail shortens the decoded prefix but never corrupts it.
        let noisy = format!("{}!!\n", &encoded[..encoded.len() - 2]);
        let decoded = lenient_base64_decode(&noisy);
        assert!(decoded.len() >= 60 && decoded.len() <= 64);
        assert_eq!(decoded[..60], payload[..60]);
        assert!(lenient_base64_decode("not base64 at all$$$").len() <= 4);
    }

    #[test]
    fn builds_multipart_bodies_with_expected_shape() {
        let boundary = "testboundary";
        let mut body = Vec::new();
        file_part(boundary, "audio.webm", "audio/webm", b"RIFFxxxx", &mut body);
        form_part(boundary, "model", "m", &mut body);
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        let text = String::from_utf8(body).unwrap();
        assert!(
            text.contains("Content-Disposition: form-data; name=\"file\"; filename=\"audio.webm\"")
        );
        assert!(text.contains("Content-Type: audio/webm"));
        assert!(text.contains("name=\"model\"\r\n\r\nm\r\n"));
        assert!(text.ends_with("--testboundary--\r\n"));
    }
}
