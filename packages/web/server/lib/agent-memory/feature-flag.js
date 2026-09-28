/**
 * Whether agent memory exists at all in this build.
 *
 * The feature is complete but not released: it ships dark so it can be tested
 * against real work without appearing to users who have not asked for it. With
 * the flag unset there is no tool, no routes, no session index and no settings
 * row — not a switch left in the off position, which would invite someone to
 * turn on something unannounced.
 *
 * Read per call rather than captured at import, so a process started with the
 * variable set is the only thing that decides — no build step bakes it in.
 */
/**
 * agent 记忆（agent memory）功能的发布开关（中文说明）。
 *
 * 功能已开发完成但尚未正式发布：通过环境变量灰度放开，只对主动设置的
 * 进程生效。开关关闭时不注册工具、不注册路由、没有会话索引也没有设置
 * 项——而不是留一个"关着"的开关等人误开未发布的东西。
 *
 * 每次调用都重新读取环境变量而不是在 import 时固化，因此只有进程启动
 * 时的环境说了算，构建过程不会把开关烘焙进产物。
 */

/** 被视为"开启"的环境变量取值集合；比对前会 trim 并转小写。 */
const TRUTHY = new Set(['1', 'true', 'yes', 'on']);

/**
 * 判断 agent memory 功能在当前进程中是否可用。
 *
 * 读取 OMPCHAMBER_MEMORY_ENABLE，trim 并转小写后与 TRUTHY 集合比对。
 * 未设置或取其它值（包括 'false'、'0'、空串）一律视为关闭。
 * 返回 boolean；每次调用即时求值，不缓存结果。
 */
export const isAgentMemoryFeatureAvailable = () => {
  const raw = process.env.OMPCHAMBER_MEMORY_ENABLE;
  return typeof raw === 'string' && TRUTHY.has(raw.trim().toLowerCase());
};
