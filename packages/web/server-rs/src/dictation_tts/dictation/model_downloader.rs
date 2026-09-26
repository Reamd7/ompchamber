//! Port of `server/lib/dictation/local/model-downloader.js` — download the
//! k2-fsa `.tar.bz2` archives with reqwest (streaming, with progress) and
//! extract them with the system `tar` into a staged directory that is
//! verified and renamed into place. An interrupted or failed extraction must
//! never leave partial files at the final path (the installed check only
//! verifies file presence).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt;
use tokio::io::AsyncWriteExt;

use super::model_catalog::local_model_spec;

pub type ProgressFn = Arc<dyn Fn(u64, Option<u64>) + Send + Sync>;

async fn has_required_files(model_dir: &Path, required_files: &[&str]) -> bool {
    let mut results = Vec::with_capacity(required_files.len());
    for rel in required_files {
        let path = model_dir.join(rel);
        let ok = match tokio::fs::metadata(&path).await {
            Ok(metadata) => metadata.is_dir() || (metadata.is_file() && metadata.len() > 0),
            Err(_) => false,
        };
        results.push(ok);
    }
    results.into_iter().all(|ok| ok)
}

async fn is_non_empty_file(path: &Path) -> bool {
    match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata.is_file() && metadata.len() > 0,
        Err(_) => false,
    }
}

async fn download_to_file(
    http: &reqwest::Client,
    url: &str,
    output_path: &Path,
    on_progress: Option<&ProgressFn>,
) -> Result<(), String> {
    let response = http
        .get(url)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    if !status.is_success() {
        // JS `${status} ${statusText}` — reqwest renders the canonical phrase.
        return Err(format!("Failed to download {url}: {status}"));
    }
    let total_bytes = response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let mut downloaded_bytes = 0u64;

    let tmp_path = output_path.with_extension(format!("tmp-{}", now_ms()));
    if let Some(parent) = tmp_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| error.to_string())?;
    }

    let mut file = tokio::fs::File::create(&tmp_path)
        .await
        .map_err(|error| error.to_string())?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        file.write_all(&chunk)
            .await
            .map_err(|error| error.to_string())?;
        downloaded_bytes += chunk.len() as u64;
        if let Some(on_progress) = on_progress {
            on_progress(downloaded_bytes, total_bytes);
        }
    }
    file.flush().await.map_err(|error| error.to_string())?;
    drop(file);
    tokio::fs::rename(&tmp_path, output_path)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn extract_tar_archive(archive_path: &Path, dest_dir: &Path) -> Result<(), String> {
    tokio::fs::create_dir_all(dest_dir)
        .await
        .map_err(|error| error.to_string())?;
    let output = tokio::process::Command::new("tar")
        .arg("xf")
        .arg(archive_path)
        .arg("-C")
        .arg(dest_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "tar exited with code {}",
            output.status.code().unwrap_or(-1)
        ));
    }
    Ok(())
}

/// `isLocalSttModelInstalled` — whether a model is fully installed (all
/// required files present).
pub async fn is_local_model_installed(models_dir: &Path, model_id: &str) -> bool {
    let Ok(spec) = local_model_spec(model_id) else {
        return false;
    };
    has_required_files(&models_dir.join(spec.extracted_dir), &spec.required_files()).await
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

/// `ensureLocalSttModel`: ensure a model is downloaded and extracted;
/// resolves with the model dir.
pub async fn ensure_local_model(
    models_dir: &Path,
    model_id: &str,
    on_progress: Option<ProgressFn>,
) -> Result<PathBuf, String> {
    let spec = local_model_spec(model_id)?;
    let model_dir = models_dir.join(spec.extracted_dir);
    let required = spec.required_files();
    if has_required_files(&model_dir, &required).await {
        return Ok(model_dir);
    }

    // A directory that exists but fails the required-files check is a partial
    // extraction from an earlier interrupted attempt — remove it before
    // retrying.
    let _ = tokio::fs::remove_dir_all(&model_dir).await;

    let downloads_dir = models_dir.join(".downloads");
    let archive_filename = spec
        .archive_url
        .rsplit('/')
        .next()
        .unwrap_or("archive.tar.bz2");
    let archive_path = downloads_dir.join(archive_filename);

    if !is_non_empty_file(&archive_path).await {
        let http = reqwest::Client::new();
        download_to_file(&http, spec.archive_url, &archive_path, on_progress.as_ref()).await?;
    }

    let staging_dir = models_dir.join(format!(".staging-{}-{}", spec.extracted_dir, now_ms()));
    let result = async {
        extract_tar_archive(&archive_path, &staging_dir).await?;
        let staged_model_dir = staging_dir.join(spec.extracted_dir);
        if !has_required_files(&staged_model_dir, &required).await {
            // Bad archive (truncated download / corrupt cache): drop it so
            // the next attempt re-downloads instead of re-extracting the
            // same broken bytes.
            let _ = tokio::fs::remove_file(&archive_path).await;
            return Err(format!(
                "Extracted {archive_filename}, but required model files are missing or empty. The archive was discarded; retry to re-download."
            ));
        }
        tokio::fs::rename(&staged_model_dir, &model_dir)
            .await
            .map_err(|error| error.to_string())
    }
    .await;

    if let Err(error) = result {
        // Any extraction failure means the cached archive can't be trusted
        // (corrupt bz2, truncated download). Discard it so retry re-downloads.
        let _ = tokio::fs::remove_dir_all(&staging_dir).await;
        let _ = tokio::fs::remove_file(&archive_path).await;
        return Err(error);
    }
    let _ = tokio::fs::remove_dir_all(&staging_dir).await;
    let _ = tokio::fs::remove_file(&archive_path).await;

    Ok(model_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-dictation-dl-{label}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[tokio::test]
    async fn install_check_requires_every_file_nonempty() {
        let dir = temp_dir("install");
        let model_dir = dir.join("sherpa-onnx-whisper-tiny");
        assert!(!is_local_model_installed(&dir, "whisper-tiny-int8").await);

        tokio::fs::create_dir_all(&model_dir).await.unwrap();
        for file in ["tiny-encoder.int8.onnx", "tiny-decoder.int8.onnx"] {
            tokio::fs::write(model_dir.join(file), b"x").await.unwrap();
        }
        assert!(!is_local_model_installed(&dir, "whisper-tiny-int8").await);

        tokio::fs::write(model_dir.join("tiny-tokens.txt"), b"tokens")
            .await
            .unwrap();
        assert!(is_local_model_installed(&dir, "whisper-tiny-int8").await);

        // An empty file is not an installed model.
        tokio::fs::write(model_dir.join("tiny-tokens.txt"), b"")
            .await
            .unwrap();
        assert!(!is_local_model_installed(&dir, "whisper-tiny-int8").await);
        let _ = tokio::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn install_check_accepts_directory_roles() {
        let dir = temp_dir("dir-role");
        // kokoro requires the espeak-ng-data directory.
        let model_dir = dir.join("kokoro-en-v0_19");
        tokio::fs::create_dir_all(model_dir.join("espeak-ng-data"))
            .await
            .unwrap();
        for file in ["model.onnx", "voices.bin", "tokens.txt"] {
            tokio::fs::write(model_dir.join(file), b"x").await.unwrap();
        }
        assert!(is_local_model_installed(&dir, "kokoro-en-v0_19").await);
        let _ = tokio::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ensure_rejects_unknown_models() {
        let dir = temp_dir("unknown");
        let error = ensure_local_model(&dir, "not-a-model", None)
            .await
            .unwrap_err();
        assert_eq!(error, "Unknown local speech model id: not-a-model");
        let _ = tokio::fs::remove_dir_all(&dir);
    }
}
