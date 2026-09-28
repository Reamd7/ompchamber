/**
 * MiniMax Coding Plan 国际站（minimax.io）配额 provider。
 *
 * 站点薄封装：全部取数与解析逻辑在 minimax-shared.js 的共享工厂中实现，
 * 这里只传入国际站的 provider 标识与 API 域名，然后转发导出各成员。
 */
import { createMiniMaxCodingPlanProvider } from './minimax-shared.js';

/** 由共享工厂生成的国际站 provider 实例。 */
const provider = createMiniMaxCodingPlanProvider({
  providerId: 'minimax-coding-plan',
  providerName: 'MiniMax Coding Plan (minimax.io)',
  aliases: ['minimax-coding-plan'],
  tokenPlanUrl: 'https://api.minimax.io/v1/token_plan/remains',
  codingPlanUrl: 'https://api.minimax.io/v1/api/openplatform/coding_plan/remains',
});

/** 对外 provider 标识（转发自工厂实例）。 */
export const providerId = provider.providerId;
/** 展示名，含国际站域名（转发自工厂实例）。 */
export const providerName = provider.providerName;
/** auth 文件匹配别名（转发自工厂实例）。 */
const aliases = provider.aliases;
/** 是否已配置（转发自工厂实例）。 */
export const isConfigured = provider.isConfigured;
/** 配额查询函数（转发自工厂实例）。 */
export const fetchQuota = provider.fetchQuota;
