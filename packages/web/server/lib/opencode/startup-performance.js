/**
 * 启动性能打点：按白名单输出结构化的启动阶段事件。
 *
 * 仅当 OMPCHAMBER_STARTUP_PERF 为 '1'/'true' 时启用。phase、outcome、
 * routeClass 均有白名单，时长只接受非负有限数字、尝试次数只接受非负整数，
 * 从设计上保证 sessionID、token 等敏感字段不会被打进日志。
 */
/** 视为“启用”的环境变量取值集合（比较前转小写）。 */
const ENABLED_VALUES = new Set(['1', 'true']);
/** 允许上报的启动阶段（phase）白名单。 */
const ALLOWED_PHASES = new Set([
  'web.pipeline.start',
  'web.listener.ready',
  'opencode.bootstrap.start',
  'opencode.bootstrap.ready',
  'opencode.bootstrap.error',
  'opencode.orphan-reap.ready',
  'opencode.attempt.start',
  'opencode.binary.ready',
  'opencode.environment.ready',
  'opencode.process.ready',
  'opencode.health.ready',
  'opencode.attempt.error',
  'proxy.readiness-hold',
]);
/** 允许上报的阶段结果（outcome）白名单。 */
const ALLOWED_OUTCOMES = new Set(['ready', 'timeout', 'aborted', 'error']);
/** 允许上报的路由类别（routeClass）白名单，用于 proxy 就绪保持打点。 */
const ALLOWED_ROUTE_CLASSES = new Set(['session-messages', 'session', 'events', 'other']);

/** 值为非负有限数字时原样返回，否则返回 undefined（对应字段被丢弃）。 */
const finiteNonNegative = (value) => Number.isFinite(value) && value >= 0 ? value : undefined;
/** 值为非负整数时原样返回，否则返回 undefined（对应字段被丢弃）。 */
const nonNegativeInteger = (value) => Number.isInteger(value) && value >= 0 ? value : undefined;

/** 判断启动性能打点是否启用（OMPCHAMBER_STARTUP_PERF=1/true）。 */
const isStartupPerformanceEnabled = () => (
  ENABLED_VALUES.has(String(process.env.OMPCHAMBER_STARTUP_PERF ?? '').toLowerCase())
);

/**
 * 记录一条启动性能事件：phase 必须在白名单内且功能已启用，否则静默返回。
 *
 * 通过 console.info('[startup-performance]', event) 输出；事件包含 phase、
 * 时间戳 at，以及全部通过校验的 durationMs / totalDurationMs / attempt /
 * outcome / routeClass，任何未通过白名单或数值校验的字段都被丢弃。
 * @param {string} phase - 启动阶段标识（须在 ALLOWED_PHASES 内）。
 * @param {object} [details] - 可选元数据（时长、总时长、尝试次数、结果、路由类别）。
 */
export const recordStartupPerformance = (phase, details = {}) => {
  if (!isStartupPerformanceEnabled() || !ALLOWED_PHASES.has(phase)) return;

  const event = {
    phase,
    at: Date.now(),
  };
  const durationMs = finiteNonNegative(details.durationMs);
  const totalDurationMs = finiteNonNegative(details.totalDurationMs);
  const attempt = nonNegativeInteger(details.attempt);
  if (durationMs !== undefined) event.durationMs = durationMs;
  if (totalDurationMs !== undefined) event.totalDurationMs = totalDurationMs;
  if (attempt !== undefined) event.attempt = attempt;
  if (ALLOWED_OUTCOMES.has(details.outcome)) event.outcome = details.outcome;
  if (ALLOWED_ROUTE_CLASSES.has(details.routeClass)) event.routeClass = details.routeClass;

  console.info('[startup-performance]', event);
};
