/**
 * provider-env-aliases 模块的单元测试。
 *
 * 覆盖：GEMINI_API_KEY 被镜像到全部 Google Generative AI 环境变量名；
 * 规范名（GOOGLE_GENERATIVE_AI_API_KEY）已有值时不被别名覆盖；
 * null/undefined 等非法输入返回空对象。
 */
import { describe, expect, test } from 'bun:test';

import { applyProviderEnvAliases } from './provider-env-aliases.js';

// 环境变量别名镜像的优先级与兜底行为
describe('applyProviderEnvAliases', () => {
  test('mirrors GEMINI_API_KEY onto Google Generative AI env names', () => {
    expect(applyProviderEnvAliases({
      GEMINI_API_KEY: 'AIza-demo',
      PATH: '/usr/bin',
    })).toEqual({
      GEMINI_API_KEY: 'AIza-demo',
      GOOGLE_API_KEY: 'AIza-demo',
      GOOGLE_GENERATIVE_AI_API_KEY: 'AIza-demo',
      PATH: '/usr/bin',
    });
  });

  test('does not overwrite an already-set preferred Google key', () => {
    expect(applyProviderEnvAliases({
      GEMINI_API_KEY: 'from-gemini',
      GOOGLE_GENERATIVE_AI_API_KEY: 'from-google',
    })).toEqual({
      GEMINI_API_KEY: 'from-gemini',
      GOOGLE_API_KEY: 'from-google',
      GOOGLE_GENERATIVE_AI_API_KEY: 'from-google',
    });
  });

  test('returns empty object for invalid input', () => {
    expect(applyProviderEnvAliases(null)).toEqual({});
    expect(applyProviderEnvAliases(undefined)).toEqual({});
  });
});
