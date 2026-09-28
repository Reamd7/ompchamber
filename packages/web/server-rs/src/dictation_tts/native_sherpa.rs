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
//!
//! 中文说明：`local-speech` feature 开启时的原生本地引擎实现。识别路径：
//! 会话命令经 std channel 送入专属线程，PCM16 累积在会话缓冲里，每次
//! commit 用新建的 offline stream 全量解码（同样的 AGC：峰值归一到 0.6、
//! 增益上限 50x），每次解码发出一个 Transcript 事件；TTS 路径为一次
//! OfflineTts 调用，产出 16-bit PCM WAV。

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

/// 识别会话固定的输入采样率（16 kHz PCM16）。
const SAMPLE_RATE: i32 = 16_000;
/// AGC 目标峰值（与 JS worker 相同的 0.6）。
const TARGET_PEAK: f32 = 0.6;
/// AGC 增益上限（50 倍），防止极安静输入被过度放大。
const MAX_GAIN: f32 = 50.0;

/// 定位模型目录下指定角色（tokens/encoder/decoder/model/voices）的文件。
/// 未知模型 id、目录缺该角色或文件不存在均返回带下载指引的 Err 文案。
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

/// PCM16 小端字节流转 f32 样本并应用 AGC：峰值低于 TARGET_PEAK 时放大
/// 到目标电平（增益封顶 MAX_GAIN）；空输入返回空向量。
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

/// 发给解码线程的会话命令。
enum DriverCommand {
    /// 追加一段 PCM16 字节到会话缓冲。
    Append(Vec<u8>),
    /// 解码整个缓冲并发出转写事件；`final_segment` 标记是否最后一段。
    Commit { final_segment: bool },
    /// 清空会话缓冲（丢弃纯静音段）。
    Clear,
    /// 结束解码线程。
    Close,
}

/// The engine behind both seams when the feature is enabled.
#[derive(Debug, Clone, Default)]
pub struct SherpaLocalEngine;

/// 会话句柄：把 trait 调用转发为解码线程命令。
struct SherpaSession {
    /// 指向解码线程的命令通道；线程退出后发送失败被静默忽略。
    tx: std_mpsc::Sender<DriverCommand>,
    /// 会话要求的输入采样率（固定 16 kHz）。
    sample_rate: u32,
}

/// `StreamingTranscriptionSession` 实现：所有操作都是向解码线程投递命令。
impl StreamingTranscriptionSession for SherpaSession {
    /// 固定返回 16 kHz。
    fn required_sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// 追加 PCM16 音频到解码缓冲（线程已退出则忽略）。
    fn append_pcm16(&mut self, chunk: Vec<u8>) {
        let _ = self.tx.send(DriverCommand::Append(chunk));
    }

    /// 请求解码当前缓冲（非最终段）。
    fn commit(&mut self) {
        let _ = self.tx.send(DriverCommand::Commit { final_segment: false });
    }

    /// 丢弃当前缓冲中的音频。
    fn clear(&mut self) {
        let _ = self.tx.send(DriverCommand::Clear);
    }

    /// 通知解码线程退出。
    fn close(&mut self) {
        let _ = self.tx.send(DriverCommand::Close);
    }
}

/// `LocalRecognizer` seam 的实现：校验模型文件并驱动原生识别。
impl LocalRecognizer for SherpaLocalEngine {
    /// 创建本地识别会话：按 catalog 校验 tokens/encoder/decoder（NeMo
    /// transducer 额外需要 joiner），构造 OfflineRecognizer 并 spawn 专属
    /// 解码线程。返回会话句柄与事件流；模型缺失或初始化失败时返回与
    /// JS 对齐的错误文案。
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

/// 将 f32 样本编码为 16-bit 单声道 PCM WAV：写 44 字节标准 RIFF 头，
/// 样本裁剪到 [-1, 1] 后缩放为 i16。
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

/// `LocalSpeaker` seam 的实现：构造 OfflineTts 并完成合成。
impl LocalSpeaker for SherpaLocalEngine {
    /// 本地 TTS 合成：按模型类型（kokoro/vits）解析 model、tokens（kokoro
    /// 还有 voices）与 espeak-ng 数据目录，生成语音后封为 WAV 返回。
    /// speaker 缺省 0、speed 缺省 1.0 且限幅到 [0.1, 10]。
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

/// 单元测试：模型缺失/未知 id 的错误文案、WAV 头正确性与 AGC 语义。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证：模型文件缺失时返回带"先下载"指引的错误文案，而不是 panic。
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

    /// 验证：catalog 之外的模型 id 被直接拒绝（unknown local model）。
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

    /// 验证：WAV 封装的 RIFF/WAVE/data 头与长度字段正确。
    #[test]
    fn wav_header_is_wellformed() {
        let wav = encode_wav_pcm16(&[0.0, 0.5, -0.5], 16_000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(wav.len(), 44 + 6);
    }

    /// 验证：AGC 与 JS 语义一致——安静信号被拉到 0.6 峰值，响亮信号不变。
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
