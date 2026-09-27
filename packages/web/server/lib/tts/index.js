/**
 * （中文模块说明）TTS 域模块的公共出口：服务单例与音色表、
 * 文本清洗/摘要工具、以及服务端语音转写。
 */
/**
 * TTS Module Entry Point
 *
 * Public export surface for the Text-to-Speech domain module.
 */

// TTS 服务：单例、类与 OpenAI 音色列表。
export {
  ttsService,
  TTSService,
  TTS_VOICES,
} from './service.js';

// 文本清洗与本地摘要（TTS/通知/笔记三种模式）。
export {
  summarizeText,
  sanitizeForTTS,
  sanitizeForNote,
} from '../text/summarization.js';

// 服务端语音转文字（OpenAI 兼容转写代理）。
export { transcribeAudio } from './stt.js';
