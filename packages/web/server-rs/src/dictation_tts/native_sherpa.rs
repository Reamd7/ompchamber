//! Native sherpa-onnx engine for local dictation and TTS.
//!
//! Behind the `local-speech` feature. Implements the [`LocalRecognizer`] and
//! [`LocalSpeaker`] seams that default to `UnavailableLocalEngine`, using the
//! official `sherpa-onnx` Rust binding — the same model files the JS worker
//! downloads (the catalog is all sherpa-onnx archives: NeMo transducer,
//! Whisper, VITS, Kokoro), so model management (download/install/status)
//! needs no changes.
//!
//! Recognition mirrors the JS worker's semantics exactly: PCM16 accumulates
//! in the session; every commit decodes the full buffer through a fresh
//! offline stream with the same automatic gain control (peak-normalize to
//! 0.6, capped at 50x), emitting one Transcript event per decode. TTS is a
//! direct OfflineTts call returning 16-bit PCM WAV.

use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;

use futures::future::BoxFuture;
use sherpa_onnx::{
    OfflineModelConfig, OfflineRecognizer, OfflineRecognizerConfig, OfflineTts, OfflineTtsConfig,
    OfflineTtsKokoroModelConfig, OfflineTtsModelConfig, OfflineTtsVitsModelConfig, OfflineTransducerModelConfig,
};

use super::dictation::local::{
    LocalRecognizer, LocalSession, LocalSpeechParams, LocalSpeechOutput, LocalSpeaker,
};
use super::dictation::model_catalog::catalog_spec;
use super::dictation::session::{random_uuid, SessionEvent, StreamingTranscriptionSession};
use tokio::sync::mpsc;

const SAMPLE_RATE: i32 = 16_000;
const TARGET_PEAK: f32 = 0.6;
const MAX_GAIN: f32 = 50.0;

fn model_file(models_dir: &Path, model_id: &str, role: &str) -> Result<PathBuf, String> {
    let spec = catalog_spec(model_id).ok_or_else(|| format!("unknown local model: {model_id}"))?;
    let file = spec
        .files
        .iter()
        .find(|(r, _)| *r == role)
        .map(|(_, name)| *name)
        .ok_or_else(|| format!("model {model_id} has no {role} file"))?;
    let path = models_dir.join(spec.extracted_dir).join(file);
    if !path.exists() {
        return Err(format!(
            "model file missing ({role}): {} — download the model first",
            path.display()
        ));
    }
    Ok(path)
}

fn pcm16_to_normalized_f32(pcm16: &[u8]) -> Vec<f32> {
    let samples: Vec<i16> = pcm16
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    if samples.is_empty() {
        return Vec::new();
    }
    let peak = samples
        .iter()
        .map(|s| (*s as i32).abs())
        .max()
        .unwrap_or(0) as f32
        / 32768.0;
    let gain = if peak > 0.0 && peak < TARGET_PEAK {
        (TARGET_PEAK / peak).min(MAX_GAIN)
    } else {
        1.0
    };
    samples
        .iter()
        .map(|s| (*s as f32 / 32768.0) * gain)
        .collect()
}

enum DriverCommand {
    Append(Vec<u8>),
    Commit { final_segment: bool },
    Clear,
    Close,
}

/// The engine behind both seams when the feature is enabled.
#[derive(Debug, Clone, Default)]
pub struct SherpaLocalEngine;

struct SherpaSession {
    tx: std_mpsc::Sender<DriverCommand>,
    sample_rate: u32,
}

impl StreamingTranscriptionSession for SherpaSession {
    fn required_sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn append_pcm16(&mut self, chunk: Vec<u8>) {
        let _ = self.tx.send(DriverCommand::Append(chunk));
    }

    fn commit(&mut self) {
        let _ = self.tx.send(DriverCommand::Commit { final_segment: false });
    }

    fn clear(&mut self) {
        let _ = self.tx.send(DriverCommand::Clear);
    }

    fn close(&mut self) {
        let _ = self.tx.send(DriverCommand::Close);
    }
}

impl LocalRecognizer for SherpaLocalEngine {
    fn create_session(
        &self,
        models_dir: &Path,
        model_id: &str,
    ) -> BoxFuture<'static, Result<LocalSession, String>> {
        let models_dir = models_dir.to_path_buf();
        let model_id = model_id.to_string();
        Box::pin(async move {
            let spec = catalog_spec(&model_id)
                .ok_or_else(|| format!("unknown local model: {model_id}"))?;
            let tokens = model_file(&models_dir, &model_id, "tokens")?;
            let encoder = model_file(&models_dir, &model_id, "encoder")?;
            let decoder = model_file(&models_dir, &model_id, "decoder")?;
            let joiner = spec
                .model_type
                .eq("nemo_transducer")
                .then(|| model_file(&models_dir, &model_id, "joiner"))
                .transpose()?;

            let model_config = match spec.model_type {
                "whisper" => OfflineModelConfig {
                    tokens: Some(tokens.to_string_lossy().into_owned()),
                    num_threads: 2,
                    debug: false,
                    provider: None,
                    ..OfflineModelConfig::default()
                },
                _ => OfflineModelConfig {
                    transducer: OfflineTransducerModelConfig {
                        encoder: Some(encoder.to_string_lossy().into_owned()),
                        decoder: Some(decoder.to_string_lossy().into_owned()),
                        joiner: joiner.map(|p| p.to_string_lossy().into_owned()),
                    },
                    tokens: Some(tokens.to_string_lossy().into_owned()),
                    num_threads: 2,
                    debug: false,
                    provider: None,
                    ..OfflineModelConfig::default()
                },
            };
            let recognizer = OfflineRecognizer::create(&OfflineRecognizerConfig {
                model_config,
                ..OfflineRecognizerConfig::default()
            })
            .ok_or_else(|| "failed to init recognizer (invalid model files?)".to_string())?;

            let (tx, rx) = std_mpsc::channel::<DriverCommand>();
            let (events_tx, events_rx) = mpsc::unbounded_channel::<SessionEvent>();
            std::thread::spawn(move || {
                let recognizer = recognizer;
                let mut buffer: Vec<u8> = Vec::new();
                let mut segment_id = random_uuid();
                let mut closed = false;
                while let Ok(command) = rx.recv() {
                    match command {
                        DriverCommand::Append(chunk) => buffer.extend_from_slice(&chunk),
                        DriverCommand::Commit { final_segment } => {
                            let samples = pcm16_to_normalized_f32(&buffer);
                            if samples.is_empty() {
                                continue;
                            }
                            let stream = recognizer.create_stream();
                            stream.accept_waveform(SAMPLE_RATE, &samples);
                            recognizer.decode(&stream);
                            if let Some(result) = stream.get_result() {
                                let _ = events_tx.send(SessionEvent::Committed {
                                    segment_id: segment_id.clone(),
                                    previous_segment_id: None,
                                });
                                let _ = events_tx.send(SessionEvent::Transcript {
                                    segment_id: segment_id.clone(),
                                    transcript: result.text.trim().to_string(),
                                    is_final: final_segment,
                                });
                            }
                            segment_id = random_uuid();
                        }
                        DriverCommand::Clear => buffer.clear(),
                        DriverCommand::Close => {
                            closed = true;
                        }
                    }
                    if closed {
                        break;
                    }
                }
            });

            let session: Box<dyn StreamingTranscriptionSession> = Box::new(SherpaSession { tx, sample_rate: SAMPLE_RATE as u32 });
            Ok((session, events_rx))
        })
    }
}

fn encode_wav_pcm16(samples: &[f32], sample_rate: i32) -> Vec<u8> {
    let data_len = samples.len() * 2;
    let mut wav = Vec::with_capacity(44 + data_len);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&(sample_rate as u32).to_le_bytes());
    wav.extend_from_slice(&((sample_rate as u32) * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data_len as u32).to_le_bytes());
    for sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        wav.extend_from_slice(&((clamped * 32767.0) as i16).to_le_bytes());
    }
    wav
}

impl LocalSpeaker for SherpaLocalEngine {
    fn synthesize(
        &self,
        params: LocalSpeechParams,
    ) -> BoxFuture<'static, Result<LocalSpeechOutput, String>> {
        Box::pin(async move {
            let spec = catalog_spec(&params.model_id)
                .ok_or_else(|| format!("unknown local model: {}", params.model_id))?;
            let model = model_file(&params.models_dir, &params.model_id, "model")?;
            let tokens = model_file(&params.models_dir, &params.model_id, "tokens")?;
            let espeak = params
                .models_dir
                .join(spec.extracted_dir)
                .join("espeak-ng-data");

            let model_config = match spec.model_type {
                "kokoro" => OfflineTtsModelConfig {
                    kokoro: OfflineTtsKokoroModelConfig {
                        model: Some(model.to_string_lossy().into_owned()),
                        voices: model_file(&params.models_dir, &params.model_id, "voices")
                            .ok()
                            .map(|p| p.to_string_lossy().into_owned()),
                        tokens: Some(tokens.to_string_lossy().into_owned()),
                        data_dir: espeak.to_string_lossy().into_owned().into(),
                        length_scale: 1.0,
                        ..OfflineTtsKokoroModelConfig::default()
                    },
                    num_threads: 2,
                    debug: false,
                    provider: None,
                    ..OfflineTtsModelConfig::default()
                },
                // vits (piper-style single-speaker voices)
                _ => OfflineTtsModelConfig {
                    vits: OfflineTtsVitsModelConfig {
                        model: Some(model.to_string_lossy().into_owned()),
                        tokens: Some(tokens.to_string_lossy().into_owned()),
                        data_dir: espeak.to_string_lossy().into_owned().into(),
                        ..OfflineTtsVitsModelConfig::default()
                    },
                    num_threads: 2,
                    debug: false,
                    provider: None,
                    ..OfflineTtsModelConfig::default()
                },
            };

            let tts = OfflineTts::create(&OfflineTtsConfig {
                model: model_config,
                ..OfflineTtsConfig::default()
            })
            .ok_or_else(|| "failed to init tts (invalid model files?)".to_string())?;
            let audio = tts
                .generate_with_config::<fn(&[f32], f32) -> bool>(
                    &params.text,
                    &sherpa_onnx::GenerationConfig {
                        sid: params.speaker_id.unwrap_or(0) as i32,
                        speed: params.speed.unwrap_or(1.0).clamp(0.1, 10.0) as f32,
                        ..sherpa_onnx::GenerationConfig::default()
                    },
                    None,
                )
                .ok_or_else(|| "tts generate failed".to_string())?;

            Ok(LocalSpeechOutput {
                audio: encode_wav_pcm16(audio.samples(), audio.sample_rate()),
                format: "wav".to_string(),
            })
        })
    }
}

/// The engine pair used by the service when the feature is enabled.
pub fn engine_pair() -> (Arc<SherpaLocalEngine>, Arc<SherpaLocalEngine>) {
    (Arc::new(SherpaLocalEngine), Arc::new(SherpaLocalEngine))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_model_files_surface_actionable_errors() {
        let engine = SherpaLocalEngine;
        let tmp = std::env::temp_dir();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let error = match rt.block_on(async {
            use futures::future::FutureExt;
            LocalRecognizer::create_session(&engine, &tmp, "whisper-base-int8")
                .boxed()
                .await
        }) {
            Err(error) => error,
            Ok(_) => panic!("expected a missing-model error"),
        };
        assert!(
            error.contains("missing") || error.contains("download"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn unknown_model_id_is_rejected() {
        let engine = SherpaLocalEngine;
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let error = match rt.block_on(async {
            use futures::future::FutureExt;
            LocalRecognizer::create_session(&engine, Path::new("/tmp"), "no-such-model")
                .boxed()
                .await
        }) {
            Err(error) => error,
            Ok(_) => panic!("expected an unknown-model error"),
        };
        assert!(error.contains("unknown local model"));
    }

    #[test]
    fn wav_header_is_wellformed() {
        let wav = encode_wav_pcm16(&[0.0, 0.5, -0.5], 16_000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(wav.len(), 44 + 6);
    }

    #[test]
    fn pcm_gain_matches_js_semantics() {
        // A quiet signal (peak 0.1) gains 6x to reach the 0.6 target.
        let quiet: Vec<u8> = vec![0x0c, 0x0c]; // 3072/32768 = 0.09375 → gain ×6.4 → 0.6
        let out = pcm16_to_normalized_f32(&quiet);
        assert!((out[0].abs() - 0.6).abs() < 0.01, "got {}", out[0]);
        // A loud signal stays untouched.
        let loud: Vec<u8> = vec![0x00, 0x7f]; // ~32512 → 0.99
        let out = pcm16_to_normalized_f32(&loud);
        assert!((out[0].abs() - 0.99).abs() < 0.01);
    }
}
