/**
 * 会话事件到 Linear 状态评论的运行时桥接模块。
 *
 * 订阅 OpenChamber 会话事件流（session.status / session.error），从事件 payload
 * 中提取 sessionId 与状态，转换成 Linear issue 上的 completed/failure 状态评论；
 * 评论发布异步进行、失败仅告警，绝不阻断会话事件流本身。
 */
import { isPlainObject, readTrimmedString } from './parse.js';
import { postLinearSessionStatus } from './status.js';

/** 读取事件 payload 的 properties 子对象（payload 非法时返回空对象兜底）。 */
function readProperties(payload) {
  if (!isPlainObject(payload)) return {};
  return isPlainObject(payload.properties) ? payload.properties : {};
}

/** 读取 properties 中指定键的嵌套对象（不存在或非对象时返回空对象兜底）。 */
function readNested(properties, key) {
  return isPlainObject(properties[key]) ? properties[key] : {};
}

/**
 * 按优先级从事件中提取 sessionId：info.sessionID、info.sessionId、
 * 顶层 sessionID、sessionId、session；均缺失时返回空字符串。
 */
function extractSessionId(payload) {
  const properties = readProperties(payload);
  const info = readNested(properties, 'info');
  return readTrimmedString(info.sessionID)
    || readTrimmedString(info.sessionId)
    || readTrimmedString(properties.sessionID)
    || readTrimmedString(properties.sessionId)
    || readTrimmedString(properties.session);
}

/** 仅对 session.status 事件提取状态类型（status.type 或 info.type），其余事件返回空串。 */
function extractStatusType(payload) {
  if (!isPlainObject(payload) || payload.type !== 'session.status') return '';
  const properties = readProperties(payload);
  const status = readNested(properties, 'status');
  const info = readNested(properties, 'info');
  return readTrimmedString(status.type) || readTrimmedString(info.type);
}

/** 仅对 session.error 事件提取错误名（error.name），其余事件返回空串。 */
function extractErrorName(payload) {
  if (!isPlainObject(payload) || payload.type !== 'session.error') return '';
  const properties = readProperties(payload);
  return readTrimmedString(readNested(properties, 'error').name);
}

/**
 * 创建一个会话状态评论运行时：processPayload 消费单条会话事件——
 * session.error（MessageAbortedError 主动中止除外）发布 failure 评论，
 * session.status 且状态为 idle 时发布 completed 评论；无 sessionId 的事件忽略。
 * stop 置停之后 processPayload 成为空操作。
 * @returns {{ processPayload: (payload: object) => void, stop: () => void }}
 */
export function createLinearSessionStatusRuntime() {
  let stopped = false;

  // 消费单条会话事件并按需异步发布 Linear 评论（已停止或无 sessionId 时忽略）。
  const processPayload = (payload) => {
    if (stopped) return;
    const sessionId = extractSessionId(payload);
    if (!sessionId) return;

    if (isPlainObject(payload) && payload.type === 'session.error') {
      if (extractErrorName(payload) === 'MessageAbortedError') return;
      void postLinearSessionStatus({ kind: 'failure', sessionId }).catch((error) => {
        console.warn('[linear] failed to post session failure comment:', error?.message || error);
      });
      return;
    }

    if (extractStatusType(payload) !== 'idle') return;
    void postLinearSessionStatus({ kind: 'completed', sessionId }).catch((error) => {
      console.warn('[linear] failed to post session completed comment:', error?.message || error);
    });
  };

  // 停止运行时：置停标志，后续事件一律忽略。
  const stop = () => {
    stopped = true;
  };

  return { processPayload, stop };
}
