// File-backed goal objectives. Session metadata must stay light (it rides
// every session.updated event), so the objective TEXT lives in a file under
// the OMPChamber data dir, keyed by the SESSION ID: sessions are globally
// unique and carry at most one goal at a time, so the mapping is fully
// deterministic — the metadata only carries an `objectiveFile: true` flag,
// never a path, and user-writable metadata cannot become a file-read vector.

/**
 * 中文说明：文件形态的目标 objective 存储。会话 metadata 必须保持轻量
 * （它随每条 session.updated 事件传输），因此 objective 正文按 SESSION ID
 * 命名存放在 OMPChamber 数据目录的 goals 目录下：会话全局唯一且同时至多
 * 一个目标，映射完全确定——metadata 只带 objectiveFile: true 标记、不带
 * 路径，用户可写的 metadata 也就无法变成任意文件读取通道。
 */
import fs from 'fs';
import os from 'os';
import path from 'path';

/** objective 文本的字符上限（写出与读入都按此截断）。 */
export const GOAL_OBJECTIVE_CHAR_LIMIT = 5_000;

// OpenCode session ids are URL-safe tokens; anything else is rejected before
// touching the filesystem.
/** 中文补充：合法 session id 模式——4~128 位 URL 安全字符，触碰文件系统前先过这道校验。 */
const SESSION_ID_PATTERN = /^[A-Za-z0-9_-]{4,128}$/;

/** 计算目标文件目录：<数据目录>/goals。 */
const goalsDir = () => path.join(
  process.env.OMPCHAMBER_DATA_DIR
    ? path.resolve(process.env.OMPCHAMBER_DATA_DIR)
    : path.join(os.homedir(), '.config', 'ompchamber'),
  'goals',
);

/** 计算某会话的目标文件路径：<goals>/<sessionId>.md。 */
const objectiveFilePath = (sessionId) => path.join(goalsDir(), `${sessionId}.md`);

/** sessionId 匹配 SESSION_ID_PATTERN 时为 true（触盘前的唯一合法性关口）。 */
export const isValidObjectiveKey = (sessionId) =>
  typeof sessionId === 'string' && SESSION_ID_PATTERN.test(sessionId);

/** 内容归一化：trim 并截断到 GOAL_OBJECTIVE_CHAR_LIMIT。 */
const clampContent = (content) => String(content ?? '').trim().slice(0, GOAL_OBJECTIVE_CHAR_LIMIT);

/** Write (or overwrite — a new goal replaces the old one) the session's objective. */
/** 中文补充：写出（或覆盖——新目标替换旧目标）该会话的 objective；session id 非法或内容为空抛 400（statusCode 附在错误对象上），写前递归建目录。 */
export const writeObjective = async (sessionId, content) => {
  if (!isValidObjectiveKey(sessionId)) {
    throw Object.assign(new Error('invalid session id'), { statusCode: 400 });
  }
  const text = clampContent(content);
  if (!text) {
    throw Object.assign(new Error('objective content is required'), { statusCode: 400 });
  }
  await fs.promises.mkdir(goalsDir(), { recursive: true });
  await fs.promises.writeFile(objectiveFilePath(sessionId), text, 'utf8');
  return { content: text };
};

/** Returns the objective text, or null when missing/invalid. */
/** 中文补充：读取目标文本；文件缺失、id 非法或读取失败一律返回 null，绝不抛出。 */
export const readObjective = async (sessionId) => {
  if (!isValidObjectiveKey(sessionId)) return null;
  try {
    const raw = await fs.promises.readFile(objectiveFilePath(sessionId), 'utf8');
    return clampContent(raw);
  } catch {
    return null;
  }
};

/** Best-effort delete; missing files are fine. */
/** 中文补充：尽力删除目标文件；文件不存在或删除失败静默忽略，幂等。 */
export const deleteObjective = async (sessionId) => {
  if (!isValidObjectiveKey(sessionId)) return;
  await fs.promises.unlink(objectiveFilePath(sessionId)).catch(() => undefined);
};
