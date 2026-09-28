/**
 * （中文模块说明）共享文本摘要服务：为 TTS 朗读、系统通知、项目笔记
 * 三种模式清洗 Markdown/代码噪音，并在模型摘要 provider 下线后提供
 * 确定性的本地蒸馏兜底（保持既有返回契约不变）。
 */
/**
 * Shared text summarization service.
 *
 * Modes:
 * - tts: concise speakable text
 * - notification: concise notification text
 * - note: distilled project note
 */

/** 为语音朗读清洗文本：剔除代码块、行内代码、Markdown 符号、括号引号、URL 与路径等发音噪音，并压缩空白。 */
export function sanitizeForTTS(text) {
  if (!text || typeof text !== 'string') return '';

  return text
    .replace(/```[\s\S]*?```/g, ' ')
    .replace(/`[^`]*`/g, ' ')
    .replace(/[*_~`#]/g, '')
    .replace(/^\s*[$#>]\s*/gm, '')
    .replace(/[|&;<>]/g, ' ')
    .replace(/\\/g, '')
    .replace(/[\[\]{}()]/g, '')
    .replace(/["']/g, '')
    .replace(/https?:\/\/[^\s]+/g, ' a link ')
    .replace(/\/[\w\-./]+/g, '')
    .replace(/\s+/g, ' ')
    .trim();
}

/** 为系统通知清洗文本：保留行内代码内容，去除 Markdown 标记（列表/标题/强调/链接）并压成单行。 */
function sanitizeForNotification(text) {
  if (!text || typeof text !== 'string') return '';

  return text
    .replace(/```[\s\S]*?```/g, ' ')
    .replace(/`([^`]*)`/g, '$1')
    .replace(/^[\t ]*[-*+]\s+/gm, '')
    .replace(/^#{1,6}\s+/gm, '')
    .replace(/\*\*(.*?)\*\*/g, '$1')
    .replace(/__(.*?)__/g, '$1')
    .replace(/\*(.*?)\*/g, '$1')
    .replace(/_(.*?)_/g, '$1')
    .replace(/\[(.*?)\]\((.*?)\)/g, '$1')
    .replace(/\s*\n\s*/g, ' ')
    .replace(/\s+/g, ' ')
    .trim();
}

/** 为项目笔记清洗文本：同通知清洗，但额外去掉 URL 与引号。 */
export function sanitizeForNote(text) {
  if (!text || typeof text !== 'string') return '';

  return text
    .replace(/```[\s\S]*?```/g, ' ')
    .replace(/`([^`]*)`/g, '$1')
    .replace(/^\s*[-*+]\s+/gm, '')
    .replace(/^#{1,6}\s+/gm, '')
    .replace(/\*\*(.*?)\*\*/g, '$1')
    .replace(/__(.*?)__/g, '$1')
    .replace(/\*(.*?)\*/g, '$1')
    .replace(/_(.*?)_/g, '$1')
    .replace(/\[(.*?)\]\((.*?)\)/g, '$1')
    .replace(/https?:\/\/[^\s]+/g, '')
    .replace(/["']/g, '')
    .replace(/\s+/g, ' ')
    .trim();
}

/** 按 mode（note/notification/tts，默认 tts）选择对应的清洗器。 */
function sanitizeByMode(text, mode) {
  if (mode === 'note') return sanitizeForNote(text);
  if (mode === 'notification') return sanitizeForNotification(text);
  return sanitizeForTTS(text);
}

/** 本地笔记蒸馏兜底：去掉"In summary"类套话，取首个子句，并按与原文长度挂钩的理想上限截断（超长补省略号）。 */
function distillNoteFallback(text, maxLength) {
  const sanitized = sanitizeForNote(text);
  if (!sanitized) return '';

  const normalized = sanitized
    .replace(/^In summary[:,]?\s*/i, '')
    .replace(/^Here(?:s| is) (?:a )?note[:,]?\s*/i, '')
    .trim();

  const sentences = normalized
    .split(/(?<=[.!?])\s+/)
    .map((part) => part.trim())
    .filter(Boolean);

  const best = (sentences[0] || normalized)
    .split(/[;:()-]\s+/)[0]
    .split(/,\s+/)[0]
    .trim();
  const idealLimit = Math.min(maxLength, Math.max(32, Math.floor(normalized.length * 0.65)));

  if (best.length <= idealLimit) return best;

  const clipped = best.slice(0, Math.max(0, idealLimit - 1)).trim();
  return clipped ? `${clipped}…` : best.slice(0, idealLimit).trim();
}

/** 本地通知蒸馏兜底：优先取第一个长度 >=20 的句子，超过 maxLength 截断并补省略号。 */
function distillNotificationFallback(text, maxLength) {
  const sanitized = sanitizeForNotification(text);
  if (!sanitized) return '';

  const sentences = sanitized
    .split(/(?<=[.!?])\s+/)
    .map((part) => part.trim())
    .filter(Boolean);
  const candidate = sentences.find((sentence) => sentence.length >= 20) || sentences[0] || sanitized;
  const limit = Number.isFinite(maxLength) ? Math.max(20, Math.floor(maxLength)) : 100;
  if (candidate.length <= limit) return candidate;

  const clipped = candidate.slice(0, Math.max(0, limit - 1)).trim();
  return clipped ? `${clipped}…` : candidate.slice(0, limit).trim();
}

/** 按 mode 选择本地兜底蒸馏器；tts 模式无蒸馏，直接返回清洗文本。 */
function fallbackByMode(text, maxLength, mode) {
  if (mode === 'note') return distillNoteFallback(text, maxLength);
  if (mode === 'notification') return distillNotificationFallback(text, maxLength);
  return sanitizeByMode(text, mode);
}

/**
 * 文本摘要入口。模型摘要 provider 已下线：总是走本地兜底，仅按
 * threshold/文本是否为空区分 reason，返回结构保持 { summary, summarized, reason, ... } 契约。
 * @param {object} params text 原文；threshold 触发阈值（默认 200，仅影响 reason）；maxLength 兜底截断长度（默认 500）；mode tts|notification|note；zenModel 已废弃忽略
 * @returns {Promise<{ summary: string, summarized: boolean, reason: string, originalLength?: number, summaryLength?: number }>}
 */
export async function summarizeText({ text, threshold = 200, maxLength = 500, zenModel, mode = 'tts' }) {
  void zenModel;

  const summary = fallbackByMode(text || '', maxLength, mode);
  if (!text || text.length <= threshold) {
    return {
      summary,
      summarized: false,
      reason: text ? 'Text under threshold' : 'No text provided',
    };
  }

  return {
    summary,
    summarized: false,
    reason: 'Model summarization provider unavailable',
    originalLength: text.length,
    summaryLength: summary.length,
  };
}
