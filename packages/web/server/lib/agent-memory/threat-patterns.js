/**
 * Text that tries to talk to the model rather than describe something.
 *
 * Memory is the one place where text from outside can settle permanently. The
 * agent browses a page, decides a line on it is worth keeping, and saves it —
 * from then on it rides into every session in every project. An injection
 * anywhere else lives for one conversation; here it lives until someone
 * notices.
 *
 * Patterns, not a model: this runs on every write and every index build, and a
 * classifier there would cost more than the whole feature. That buys only the
 * blunt cases, which is the honest expectation — it raises the floor rather
 * than closing the door.
 *
 * A match never deletes anything. The entry is stored, kept out of what the
 * model is shown, and flagged for the user, because a silently dropped entry
 * hides the attempt from the only party who can judge it.
 */
/**
 * 注入检测：识别"试图对模型说话"而非"描述事实"的文本（中文说明）。
 *
 * 记忆是外部文本唯一能永久沉淀的地方：agent 浏览网页、看中某一行就存
 * 下来，从此它跟着每个项目里的每次会话。别处的注入只活一段对话，这里
 * 的注入会一直活到有人发现为止。
 *
 * 用正则而非模型分类：这段代码跑在每次写入和每次索引构建上，分类器的
 * 开销会比整个功能还贵。代价是只覆盖最直白的情况——它抬高下限，并不
 * 关死大门。
 *
 * 命中绝不删除任何内容：条目照常存储，但不再展示给模型，并向用户标
 * 记；静默丢弃会把攻击企图藏在唯一有能力判断它的人面前。
 */

/**
 * 威胁模式列表。覆盖五类：顶替既有指令、重设模型身份、伪造对话轮次
 * 结构、绕过安全护栏、索要系统提示词或搬运凭据。均大小写不敏感（部分
 * 还允许多行），写入与索引构建时逐条对文本执行 test。
 */
const PATTERNS = [
  // Trying to displace instructions already in play.
  /\bignore\s+(?:all\s+|any\s+)?(?:previous|prior|earlier|above)\s+(?:instructions?|prompts?|rules?|context)\b/i,
  /\bdisregard\s+(?:all\s+|any\s+)?(?:previous|prior|earlier|above)\s+(?:instructions?|prompts?|rules?)\b/i,
  /\bforget\s+(?:everything|all)\s+(?:you|above|before)\b/i,
  /\boverrid(?:e|ing)\s+(?:your\s+)?(?:system\s+)?(?:prompt|instructions?)\b/i,

  // Trying to reassign who the model is.
  /\byou\s+are\s+now\s+(?:a|an|the)\b/i,
  /\bfrom\s+now\s+on[,\s]+(?:you|act|behave|respond)\b/i,
  /\bact\s+as\s+(?:if\s+you\s+are\s+)?(?:a|an|the)\s+\w+\s+with\s+no\s+(?:restrictions?|limits?|rules?)\b/i,

  // Forging turn structure so the text reads as a different speaker.
  /^\s*(?:system|assistant|developer)\s*:/im,
  /<\|(?:im_start|im_end|system|endoftext)\|>/i,
  /\[\/?(?:INST|SYS)\]/,

  // Aimed at the guardrails themselves.
  /\b(?:bypass|disable|turn\s+off)\s+(?:all\s+)?(?:safety|security|guardrails?|filters?|restrictions?)\b/i,
  /\bdeveloper\s+mode\s+(?:enabled|on|activated)\b/i,

  // Asking for what the model was told, or for credentials to travel.
  /\b(?:print|reveal|repeat|output|show)\s+(?:me\s+)?(?:your|the)\s+(?:system\s+prompt|instructions|initial\s+prompt)\b/i,
  /\b(?:send|post|upload|exfiltrate)\s+(?:the\s+|your\s+)?(?:api\s+key|token|credentials?|secrets?|env)\b/i,
];

/**
 * The first pattern this text trips, or null. The name is returned rather than
 * a boolean so the panel can tell the user what was matched instead of leaving
 * them with an unexplained warning.
 */
/**
 * 返回传入文本命中的第一条模式（返回其 source，截断到 80 字符），未命
 * 中返回 null。返回模式内容而非布尔值，是为了让面板能告诉用户究竟匹
 * 配到了什么，而不是丢下一个无法解释的警告。非字符串或空串直接返回
 * null。
 */
export const findThreatPattern = (value) => {
  if (typeof value !== 'string' || value.length === 0) {
    return null;
  }
  const match = PATTERNS.find((pattern) => pattern.test(value));
  return match ? match.source.slice(0, 80) : null;
};

/**
 * 一次检查多个字段（如 title 与 body）：任一字段命中注入模式即返回
 * true，全部干净才返回 false。用于 save 路径对整条记忆的联合检查。
 */
export const looksLikeInjection = (...values) => (
  values.some((value) => findThreatPattern(value) !== null)
);
