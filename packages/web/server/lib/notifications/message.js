/**
 * 通知消息工具：为各类通知（桌面通知、移动推送、UI 广播）准备"最后一条消息"纯文本预览。
 *
 * 核心流程：把可能包含 Markdown 的原始消息归一化为单行纯文本，再按配置的最大长度
 * 截断。历史上基于 LLM 的摘要能力（settings.summarizeLastMessage 等）已下线，
 * 本模块不再读取摘要相关配置，始终直接处理原始文本。
 */

/** 通知文本默认最大长度（字符数）；超出后截断并追加省略号。 */
const DEFAULT_NOTIFICATION_MESSAGE_MAX_LENGTH = 250;

/**
 * 读取"必须为正有限数"的数值配置：value 非数字、非有限（NaN 或 Infinity）或
 * 小于等于 0 时返回 fallback，否则原样返回。用于容忍 settings 中缺失或非法的长度配置。
 * @param {*} value 待校验的配置值
 * @param {number} fallback 校验失败时的兜底值
 * @returns {number} 有效的原值或 fallback
 */
const resolvePositiveNumber = (value, fallback) => {
  if (typeof value !== 'number' || !Number.isFinite(value) || value <= 0) {
    return fallback;
  }
  return value;
};

/**
 * 将 Markdown 消息归一化为单行纯文本，避免通知里出现原始标记噪音。
 * 依序处理：删除围栏代码块、去掉行内代码的反引号、剥离行首列表标记与 ATX
 * 标题前缀、去掉粗体与斜体定界符（**、__、*、_）、把 [文字](链接) 收敛为链接
 * 文字，最后把换行与连续空白折叠为单个空格并 trim。非字符串输入返回空串。
 * @param {*} text 原始消息文本
 * @returns {string} 归一化后的单行纯文本
 */
const normalizeNotificationPlainText = (text) => {
  if (typeof text !== 'string') {
    return '';
  }

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
};

/**
 * 截断通知文本：长度超过 maxLength 时保留前 maxLength 个字符并追加省略号，
 * 未超过则原样返回。maxLength 经 resolvePositiveNumber 校验，非法时回退到
 * DEFAULT_NOTIFICATION_MESSAGE_MAX_LENGTH；非字符串输入返回空串。
 * @param {*} text 待截断的文本
 * @param {number} [maxLength] 允许的最大字符数
 * @returns {string} 截断后的文本
 */
export const truncateNotificationText = (text, maxLength = DEFAULT_NOTIFICATION_MESSAGE_MAX_LENGTH) => {
  if (typeof text !== 'string') {
    return '';
  }

  const safeMaxLength = resolvePositiveNumber(maxLength, DEFAULT_NOTIFICATION_MESSAGE_MAX_LENGTH);
  if (text.length <= safeMaxLength) {
    return text;
  }

  return `${text.slice(0, safeMaxLength)}...`;
};

/**
 * 组装通知使用的"最后一条消息"预览：先归一化为纯文本，再按
 * settings.maxLastMessageLength（非法时回退默认值 250）截断。
 * message 为空或非字符串时直接返回空串；已废弃的摘要类 settings 一律忽略。
 * @param {Object} params
 * @param {*} params.message 会话中的原始消息文本
 * @param {Object} [params.settings] 通知设置，仅消费 maxLastMessageLength 字段
 * @returns {Promise<string>} 可直接用于通知展示的纯文本
 */
export const prepareNotificationLastMessage = async ({ message, settings }) => {
  const originalMessage = typeof message === 'string' ? message : '';
  if (!originalMessage) {
    return '';
  }

  const maxLastMessageLength = resolvePositiveNumber(settings?.maxLastMessageLength, DEFAULT_NOTIFICATION_MESSAGE_MAX_LENGTH);
  const plainTextMessage = normalizeNotificationPlainText(originalMessage);
  return truncateNotificationText(plainTextMessage, maxLastMessageLength);
};
