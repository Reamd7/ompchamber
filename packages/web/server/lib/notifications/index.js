/**
 * notifications 模块的公共出口：仅重新导出通知消息纯文本准备能力。
 * push / emitter 等运行时模块由调用方按需直接引入，不经此 barrel 转发。
 */

/** 重新导出消息准备 helper，保持 ./message.js 为唯一实现来源。 */
export { prepareNotificationLastMessage } from './message.js';
