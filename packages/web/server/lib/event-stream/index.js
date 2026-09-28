/**
 * event-stream 模块的公共出口：再导出运行时装配层、全局 hub 构造器
 * 与上游读取器时间常量；更细粒度的模块（protocol、两个 bridge、
 * upstream reader）请从各自文件直接引入。
 */

// 运行时装配与 UI 事件广播器。
export {
  createGlobalUiEventBroadcaster,
  createMessageStreamWsRuntime,
} from './runtime.js';

// 全局消息流 hub（测试与自定义装配场景直接引入）。
export {
  createGlobalMessageStreamHub,
} from './global-hub.js';

// 上游读取器的时间常量（服务装配处引用）。
export {
  DEFAULT_UPSTREAM_STALL_TIMEOUT_MS,
  UPSTREAM_STALL_TIMEOUT_CONCURRENT_MS,
} from './upstream-reader.js';
