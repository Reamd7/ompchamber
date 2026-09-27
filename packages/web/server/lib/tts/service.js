/**
 * （中文模块说明）OpenAI TTS 服务端封装：解析服务端 API key（环境变量
 * 或 opencode auth 文件）、维护共享 client，并支持调用方临时提供
 * apiKey/baseURL 的自定义 OpenAI 兼容服务调用。
 */
/**
 * Server-side Text-to-Speech Service
 *
 * Uses OpenAI's TTS API to generate audio on the server and stream it to clients.
 * This bypasses mobile Safari's audio context restrictions.
 */

import OpenAI from 'openai';
import { readAuthFile } from '../opencode/auth.js';
import { normalizeCustomOpenAIBaseURL } from './base-url.js';

// Voice options from OpenAI
// （中文说明）OpenAI TTS 内置音色列表，供客户端 UI 枚举。
export const TTS_VOICES = [
  'alloy', 'ash', 'ballad', 'coral', 'echo', 'fable',
  'nova', 'onyx', 'sage', 'shimmer', 'verse', 'marin', 'cedar'
];

/** 解析 OpenAI API key：优先 OPENAI_API_KEY 环境变量，其次读 opencode auth 文件（openai/codex/chatgpt 别名；支持纯字符串 token 与 OAuth access 两种形态）。 */
function getOpenAIApiKey() {
  // First check environment variable
  const envKey = process.env.OPENAI_API_KEY;
  if (envKey) {
    return envKey;
  }

  // Then check opencode auth file (same as usage tracker)
  try {
    const auth = readAuthFile();
    // Check for openai, codex, or chatgpt aliases
    const openaiAuth = auth.openai || auth.codex || auth.chatgpt;
    if (openaiAuth) {
      // Handle both string format (just the token) and object format
      if (typeof openaiAuth === 'string') {
        return openaiAuth;
      }
      // Try access token first (OAuth), then regular token
      if (openaiAuth.access) {
        return openaiAuth.access;
      }
      if (openaiAuth.token) {
        return openaiAuth.token;
      }
    }
  } catch (error) {
    console.warn('[TTSService] Failed to read auth file:', error.message);
  }

  return null;
}

/** OpenAI TTS 客户端封装：共享 client 随 key 变化自动重建。 */
class TTSService {
  /** 维护共享 client 与创建它时使用的 key 指纹。 */
  constructor() {
    this._client = null;
    this._lastApiKey = null;
  }

  /** 取共享 OpenAI client；首次调用或 key 变化时（重）建；无可用 key 返回 null。 */
  _getClient() {
    const apiKey = getOpenAIApiKey();

    // If API key changed or client doesn't exist, create new client
    if (apiKey && (!this._client || this._lastApiKey !== apiKey)) {
      this._client = new OpenAI({ apiKey });
      this._lastApiKey = apiKey;
    }

    return this._client;
  }

  /** 服务端是否配置了可用 key（不含调用方临时提供 key 的场景）。 */
  isAvailable() {
    return this._getClient() !== null;
  }

  /**
   * （中文说明）生成语音并一次性取回音频 buffer（方法名中的 stream 为
   * 历史遗留）。调用方提供 apiKey/baseURL 时新建临时 client（自定义
   * baseURL 只发送兼容子集参数）；否则用服务端配置；均不可用则抛错。
   */
  /**
   * Generate speech and return as a stream
   */
  async generateSpeechStream(options) {
    const {
      text,
      voice = 'coral',
      model = 'gpt-4o-mini-tts',
      speed = 1.0,
      instructions,
      apiKey,
      baseURL,
    } = options;

    const normalizedBaseURLResult = normalizeCustomOpenAIBaseURL(baseURL);
    if (normalizedBaseURLResult.error) {
      throw new Error(normalizedBaseURLResult.error);
    }
    const normalizedBaseURL = normalizedBaseURLResult.value;

    // Use provided API key / baseURL or fall back to configured key
    let client;
    if (normalizedBaseURL || apiKey) {
      const clientOpts = {};
      if (apiKey) clientOpts.apiKey = apiKey;
      if (!apiKey) clientOpts.apiKey = 'not-required';
      if (normalizedBaseURL) clientOpts.baseURL = normalizedBaseURL;
      client = new OpenAI(clientOpts);
    } else {
      client = this._getClient();
    }

    if (!client) {
      throw new Error('TTS service not available. Configure OpenAI in OpenCode, provide an API key, or set a custom server URL in settings.');
    }

    if (!text.trim()) {
      throw new Error('Text is required for TTS');
    }

    try {
      // OpenAI-compatible servers (custom baseURL) may not support `instructions`
      // or `response_format`, but do support `speed`. Send the safe subset.
      const speechParams = normalizedBaseURL
        ? { model, voice, input: text, speed }
        : {
            model,
            voice,
            input: text,
            speed,
            ...(instructions && { instructions }),
            response_format: 'mp3',
          };

      console.log('[TTSService] Generating speech — model:', model, 'voice:', voice, 'baseURL:', normalizedBaseURL ?? '(openai)');
      const response = await client.audio.speech.create(speechParams);

      const arrayBuffer = await response.arrayBuffer();
      return {
        buffer: Buffer.from(arrayBuffer),
        contentType: 'audio/mpeg',
      };
    } catch (error) {
      console.error('[TTSService] Error generating speech:', error);
      throw new Error(`Failed to generate speech: ${error.message || 'Unknown error'}`);
    }
  }

  /**
   * （中文说明）仅用服务端配置生成语音 buffer（缓存场景）；
   * 未配置 key 抛错，底层错误原样上抛。
   */
  /**
   * Generate speech and return as a buffer (for caching)
   */
  async generateSpeechBuffer(options) {
    const client = this._getClient();
    if (!client) {
      throw new Error('OpenAI API key not configured. Set OPENAI_API_KEY environment variable or configure OpenAI in OpenCode.');
    }

    const {
      text,
      voice = 'coral',
      model = 'gpt-4o-mini-tts',
      speed = 1.0,
      instructions
    } = options;

    try {
      const response = await client.audio.speech.create({
        model,
        voice,
        input: text,
        speed,
        ...(instructions && { instructions }),
        response_format: 'mp3',
      });

      const arrayBuffer = await response.arrayBuffer();
      return Buffer.from(arrayBuffer);
    } catch (error) {
      console.error('[TTSService] Error generating speech buffer:', error);
      throw error;
    }
  }
}

// （中文说明）全局单例：路由层复用同一 client。
// Export singleton instance
export const ttsService = new TTSService();
// 具名导出类本身（测试/自定义实例用）。
export { TTSService };
