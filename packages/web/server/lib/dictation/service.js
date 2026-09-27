import { detectTextLanguage } from '../tts/language-detect.js';
/**
 * Dictation service: resolves STT providers, tracks local model download
 * state, and exposes a readiness snapshot for the status route.
 *
 * Providers:
 * - 'local' (default): sherpa-onnx Parakeet running in a worker process.
 *   Models auto-download in the background on first use.
 * - 'openai-compatible': any OpenAI-compatible /v1/audio/transcriptions
 *   endpoint (faster-whisper, whisper.cpp, OpenAI).
 */

/**
 * 听写服务（中文说明）：解析 STT provider、跟踪本地模型的下载状态，
 * 并为状态路由暴露就绪快照；同时承载本地 TTS 合成与模型下载/删除操作。
 * provider 为 'local'（sherpa-onnx Parakeet worker，首次使用自动后台
 * 下载）或 'openai-compatible'（任意 OpenAI 兼容转写端点）。
 */
import { rm } from 'fs/promises';

import { DictationWorkerClient, WorkerBackedTranscriptionSession } from './local/worker-client.js';
import { OpenAICompatibleTranscriptionSession } from './openai-compatible-session.js';
import {
  DEFAULT_LOCAL_STT_MODEL,
  DEFAULT_LOCAL_TTS_MODEL,
  getLocalTtsDefaultSpeaker,
  resolveLocalTtsModelForLanguage,
  LOCAL_STT_MODEL_CATALOG,
  LOCAL_STT_MODEL_IDS,
  LOCAL_TTS_MODEL_CATALOG,
  LOCAL_TTS_MODEL_IDS,
  getLocalSttModelDir,
  isLocalModelId,
  isLocalSttModelId,
  isLocalTtsModelId,
} from './local/model-catalog.js';
import { ensureLocalSttModel, isLocalSttModelInstalled } from './local/model-downloader.js';

/**
 * 创建听写/语音服务实例。
 * @param {{ modelsDir: string }} options 本地模型根目录
 */
export function createDictationService({ modelsDir }) {
  // 与 worker 进程通信的客户端（STT 引擎与 TTS 合成都跑在 worker 里）。
  const workerClient = new DictationWorkerClient();
  /** modelId -> 'downloading' | 'error' */
  /** modelId → 'downloading' | 'error' 的下载状态表。 */
  const downloadStates = new Map();
  /** modelId -> last download error message */
  /** modelId → 最近一次下载错误信息。 */
  const downloadErrors = new Map();
  /** modelId -> in-flight ensure promise */
  /** modelId → 进行中的 ensure promise（并发请求去重）。 */
  const downloadPromises = new Map();
  /** modelId -> 0..100 download percent (null while size unknown) */
  /** modelId → 下载百分比 0..100（总大小未知时为 null）。 */
  const downloadProgress = new Map();

  /**
   * 启动（或复用）某模型的后台下载并维护状态/进度/错误三张表；
   * 成功或失败后清理进行中记录，失败时保留 error 状态供下次重试。
   */
  const startModelDownload = (modelId) => {
    const existing = downloadPromises.get(modelId);
    if (existing) {
      return existing;
    }
    downloadStates.set(modelId, 'downloading');
    downloadErrors.delete(modelId);
    downloadProgress.set(modelId, 0);
    const promise = ensureLocalSttModel({
      modelsDir,
      modelId,
      onProgress: (downloadedBytes, totalBytes) => {
        downloadProgress.set(
          modelId,
          totalBytes ? Math.min(100, Math.round((downloadedBytes / totalBytes) * 100)) : null,
        );
      },
    })
      .then(() => {
        downloadStates.delete(modelId);
        downloadPromises.delete(modelId);
        downloadProgress.delete(modelId);
      })
      .catch((error) => {
        downloadStates.set(modelId, 'error');
        downloadErrors.set(modelId, error?.message || String(error));
        downloadPromises.delete(modelId);
        downloadProgress.delete(modelId);
      });
    downloadPromises.set(modelId, promise);
    return promise;
  };

  /** 请求的本地模型 id 合法则原样返回，否则回退默认 STT 模型。 */
  const resolveLocalModelId = (requested) => {
    return isLocalSttModelId(requested) ? requested : DEFAULT_LOCAL_STT_MODEL;
  };

  /**
   * Create a connected StreamingTranscriptionSession for one dictation.
   * Returns { session } on success or { error, retryable, reasonCode } when
   * the provider is not ready.
   *
   * @param {{ provider?: string, language?: string, localModel?: string,
   *           openaiCompatible?: { baseUrl?: string, model?: string, apiKey?: string } }} options
   */
  /** 中文说明：按 provider 分派——openai-compatible 直连建会话；local 先查安装，未装则触发后台下载并返回可重试的"下载中"错误，损坏模型自动删除以便重下。 */
  const createSttSession = async (options = {}) => {
    const provider = options.provider === 'openai-compatible' ? 'openai-compatible' : 'local';

    if (provider === 'openai-compatible') {
      const config = options.openaiCompatible || {};
      const session = new OpenAICompatibleTranscriptionSession({
        baseURL: config.baseUrl,
        model: config.model,
        apiKey: config.apiKey || undefined,
        language: options.language || undefined,
      });
      try {
        await session.connect();
      } catch (error) {
        return {
          error: error?.message || String(error),
          retryable: false,
          reasonCode: 'stt_not_configured',
        };
      }
      return { session };
    }

    const modelId = resolveLocalModelId(options.localModel);
    const installed = await isLocalSttModelInstalled(modelsDir, modelId);
    if (!installed) {
      const state = downloadStates.get(modelId);
      if (state === 'error') {
        const message = downloadErrors.get(modelId) || 'Model download failed';
        // Allow a retry on the next attempt.
        downloadStates.delete(modelId);
        return {
          error: `Failed to download dictation model: ${message}`,
          retryable: true,
          reasonCode: 'model_download_failed',
        };
      }
      void startModelDownload(modelId);
      return {
        error: 'Dictation model is downloading',
        retryable: true,
        reasonCode: 'model_download_in_progress',
      };
    }

    const session = new WorkerBackedTranscriptionSession(workerClient, { modelsDir, modelId });
    try {
      await session.connect();
    } catch (error) {
      const message = error?.message || String(error);
      // A model that passes the file-presence check but fails to load is
      // corrupt on disk (e.g. truncated by an interrupted extraction). Remove
      // it so the next attempt re-downloads instead of crashing forever.
      if (/Load model|Protobuf parsing failed/i.test(message)) {
        await rm(getLocalSttModelDir(modelsDir, modelId), { recursive: true, force: true })
          .catch(() => undefined);
        return {
          error: 'Dictation model files were corrupt and have been removed; retry to re-download',
          retryable: true,
          reasonCode: 'model_corrupt',
        };
      }
      return {
        error: message,
        retryable: true,
        reasonCode: 'stt_unavailable',
      };
    }
    return { session };
  };

  /**
   * Readiness snapshot for the status route and UI gating.
   * @param {{ provider?: string, localModel?: string }} [options]
   */
  /** 中文说明：返回 provider 就绪快照与全部 STT/TTS 模型的安装/下载状态；未就绪时带 reasonCode（下载中/下载失败/缺失）。 */
  const getStatus = async (options = {}) => {
    const provider = options.provider === 'openai-compatible' ? 'openai-compatible' : 'local';
    const modelId = resolveLocalModelId(options.localModel);

    /** 生成单个模型的状态描述（是否已安装、下载进度与错误）。 */
    const describeModel = async (id, catalog) => ({
      id,
      description: catalog[id].description,
      installed: await isLocalSttModelInstalled(modelsDir, id),
      downloading: downloadStates.get(id) === 'downloading',
      downloadProgress: downloadProgress.get(id) ?? null,
      downloadError: downloadErrors.get(id) || null,
    });

    const models = await Promise.all(
      LOCAL_STT_MODEL_IDS.map((id) => describeModel(id, LOCAL_STT_MODEL_CATALOG)),
    );
    const ttsModels = await Promise.all(
      LOCAL_TTS_MODEL_IDS.map((id) => describeModel(id, LOCAL_TTS_MODEL_CATALOG)),
    );

    if (provider === 'openai-compatible') {
      return { provider, available: true, models, ttsModels };
    }

    const model = models.find((entry) => entry.id === modelId) || null;
    if (model?.installed) {
      return { provider, available: true, activeModel: modelId, models, ttsModels };
    }
    if (model?.downloading) {
      return {
        provider,
        available: false,
        reasonCode: 'model_download_in_progress',
        activeModel: modelId,
        models,
        ttsModels,
      };
    }
    if (model?.downloadError) {
      return {
        provider,
        available: false,
        reasonCode: 'model_download_failed',
        error: model.downloadError,
        activeModel: modelId,
        models,
        ttsModels,
      };
    }
    return {
      provider,
      available: false,
      reasonCode: 'models_missing',
      activeModel: modelId,
      models,
      ttsModels,
    };
  };

  /**
   * Synthesize speech with the local TTS model. Returns WAV bytes, or a
   * readiness error while the model is missing/downloading.
   *
   * With `language: 'auto'` the text's language decides the model: the
   * caller's model when it speaks that language, otherwise the catalog
   * model for it (downloaded on first use, reported as in-progress until it
   * lands). The caller's speaker id is kept only on the caller's model; a
   * substitute model starts from its own default speaker for the language.
   * A language no catalog model covers keeps the caller's model, so text is
   * never silently dropped.
   * `languageSample` is the whole message the chunk belongs to (or a prefix
   * of it): the language is judged on that, never on a short chunk alone.
   * @param {{ text: string, model?: string, speakerId?: number, speed?: number, language?: string, languageSample?: string }} options
   */
  /** 中文说明：用本地 TTS 模型合成 WAV；language 为 'auto' 时按整条消息判语种并可能切换模型/默认说话人，模型缺失时触发下载并返回可重试错误。 */
  const synthesizeSpeech = async ({ text, model, speakerId, speed, language, languageSample }) => {
    const requestedModelId = isLocalTtsModelId(model) ? model : DEFAULT_LOCAL_TTS_MODEL;
    let modelId = requestedModelId;
    let resolvedLanguage = null;
    if (language === 'auto') {
      resolvedLanguage = detectTextLanguage(languageSample || text).language;
      const forLanguage = resolveLocalTtsModelForLanguage(resolvedLanguage, requestedModelId);
      if (forLanguage && forLanguage !== requestedModelId) {
        modelId = forLanguage;
        speakerId = getLocalTtsDefaultSpeaker(modelId, resolvedLanguage);
      }
    }
    const installed = await isLocalSttModelInstalled(modelsDir, modelId);
    if (!installed) {
      const state = downloadStates.get(modelId);
      if (state === 'error') {
        const message = downloadErrors.get(modelId) || 'Model download failed';
        downloadStates.delete(modelId);
        return {
          error: `Failed to download TTS model: ${message}`,
          retryable: true,
          reasonCode: 'model_download_failed',
        };
      }
      void startModelDownload(modelId);
      return {
        error: 'TTS model is downloading',
        retryable: true,
        reasonCode: 'model_download_in_progress',
      };
    }

    const result = await workerClient.synthesizeSpeech({
      modelsDir,
      modelId,
      text,
      speakerId,
      speed,
    });
    return { audio: result.audio, format: result.format, modelId, language: resolvedLanguage };
  };

  /**
   * Kick off a background download for a model (used by the status route's
   * download action so Settings can pre-download models).
   */
  /** 中文说明：为指定模型触发后台下载（已安装则直接返回），供设置页预下载。 */
  const requestModelDownload = async (modelId) => {
    if (!isLocalModelId(modelId)) {
      return { ok: false, error: 'Unknown model id' };
    }
    if (await isLocalSttModelInstalled(modelsDir, modelId)) {
      return { ok: true, installed: true };
    }
    void startModelDownload(modelId);
    return { ok: true, installed: false };
  };

  /**
   * Delete an installed model from disk. A model that is mid-download cannot
   * be deleted. An engine already loaded in the worker keeps its in-memory
   * copy until the worker's idle shutdown; the files are simply re-downloaded
   * on the next use if the model is selected again.
   */
  /** 中文说明：从磁盘删除已安装模型；下载中不允许删除，id 未知返回错误。 */
  const deleteModel = async (modelId) => {
    if (!isLocalModelId(modelId)) {
      return { ok: false, error: 'Unknown model id' };
    }
    if (downloadStates.get(modelId) === 'downloading') {
      return { ok: false, error: 'Model is downloading' };
    }
    await rm(getLocalSttModelDir(modelsDir, modelId), { recursive: true, force: true });
    downloadErrors.delete(modelId);
    return { ok: true };
  };

  /** 关闭 worker 客户端（进程退出时调用）。 */
  const shutdown = () => {
    workerClient.shutdown();
  };

  // 服务对外暴露的接口。
  return {
    createSttSession,
    synthesizeSpeech,
    getStatus,
    requestModelDownload,
    deleteModel,
    shutdown,
  };
}
