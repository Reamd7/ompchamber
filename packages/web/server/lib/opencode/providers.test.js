/**
 * providers.js 自定义 provider 配置持久化层的单元测试（bun:test）。
 *
 * 覆盖 validateCustomProviderConfig 的校验规则（ID/名称/baseURL/模型/凭据/
 * npm 适配包白名单）、upsertProviderConfig 在 user/project/custom 三种 scope
 * 下的写入与更新、removeProviderConfig 的删除语义，以及「校验失败不落盘」
 * 「hasStoredAuth 免 env」等边界。每个用例使用独立的临时项目目录，不污染真实配置。
 */
import { afterEach, beforeEach, describe, expect, test } from 'bun:test';
import fs from 'fs';
import os from 'os';
import path from 'path';

import {
  upsertProviderConfig,
  validateCustomProviderConfig,
  getProviderSources,
  removeProviderConfig,
} from './providers.js';

// 当前用例使用的临时项目目录（beforeEach 中重建、afterEach 中删除）。
let projectDir;

/** 将 value 以两空格缩进的 JSON 写入 filePath（自动创建父目录），用于铺设配置文件。 */
function writeJson(filePath, value) {
  fs.mkdirSync(path.dirname(filePath), { recursive: true });
  fs.writeFileSync(filePath, JSON.stringify(value, null, 2), 'utf8');
}

/** 读取并解析 filePath 的 JSON 内容，用于断言落盘后的配置形状。 */
function readJson(filePath) {
  return JSON.parse(fs.readFileSync(filePath, 'utf8'));
}

// 套件：自定义 provider 的校验规则与跨 user/project/custom 配置层的持久化读写。
describe('custom provider config persistence', () => {
  beforeEach(() => {
    projectDir = fs.mkdtempSync(path.join(os.tmpdir(), 'ompchamber-provider-'));
  });

  afterEach(() => {
    fs.rmSync(projectDir, { recursive: true, force: true });
  });

  test('validateCustomProviderConfig rejects invalid endpoint and credentials shape', () => {
    expect(validateCustomProviderConfig('Bad Id', {
      name: 'X',
      options: { baseURL: 'https://api.example.com' },
      models: { m: { name: 'M' } },
    }).ok).toBe(false);

    expect(validateCustomProviderConfig('ok', {
      name: 'X',
      options: { baseURL: 'ftp://api.example.com' },
      models: { m: { name: 'M' } },
    }).error).toContain('http://');

    expect(validateCustomProviderConfig('ok', {
      name: 'X',
      options: { baseURL: 'https://api.example.com' },
      models: {},
    }).ok).toBe(false);
  });

  test('validateCustomProviderConfig rejects missing credentials', () => {
    expect(validateCustomProviderConfig('ok', {
      name: 'X',
      options: { baseURL: 'https://api.example.com' },
      models: { m: { name: 'M' } },
    }).ok).toBe(false);

    expect(validateCustomProviderConfig('ok', {
      name: 'X',
      options: { baseURL: 'https://api.example.com' },
      models: { m: { name: 'M' } },
    }, { hasStoredAuth: true }).ok).toBe(true);

    expect(validateCustomProviderConfig('ok', {
      name: 'X',
      env: ['MY_KEY'],
      options: { baseURL: 'https://api.example.com' },
      models: { m: { name: 'M' } },
    }).ok).toBe(true);
  });

  test('accepts the OpenCode Responses and Anthropic adapter packages', () => {
    for (const npm of ['@ai-sdk/openai', '@ai-sdk/anthropic']) {
      const result = validateCustomProviderConfig('ok', {
        name: 'X',
        npm,
        env: ['MY_KEY'],
        options: { baseURL: 'https://api.example.com/v1' },
        models: { m: { name: 'M' } },
      });
      expect(result.ok).toBe(true);
      expect(result.value.config.npm).toBe(npm);
    }
  });

  test('rejects unsupported adapter packages', () => {
    const result = validateCustomProviderConfig('ok', {
      name: 'X',
      npm: '@example/unsupported',
      env: ['MY_KEY'],
      options: { baseURL: 'https://api.example.com/v1' },
      models: { m: { name: 'M' } },
    });

    expect(result.ok).toBe(false);
    expect(result.error).toContain('@ai-sdk/openai');
  });

  test('upsertProviderConfig writes and round-trips project config', () => {
    const result = upsertProviderConfig('campus-llm', {
      name: 'Campus LLM',
      npm: '@ai-sdk/openai-compatible',
      options: {
        baseURL: 'https://llm.example.edu/v1',
        headers: { 'X-Campus': '1' },
      },
      models: {
        'fast-model': { name: 'Fast' },
      },
      env: ['CAMPUS_KEY'],
    }, projectDir, 'project');

    expect(result.providerId).toBe('campus-llm');
    expect(fs.existsSync(result.path)).toBe(true);
    expect(result.path.startsWith(projectDir)).toBe(true);

    const written = readJson(result.path);
    expect(written.provider['campus-llm']).toEqual({
      npm: '@ai-sdk/openai-compatible',
      name: 'Campus LLM',
      env: ['CAMPUS_KEY'],
      options: {
        baseURL: 'https://llm.example.edu/v1',
        headers: { 'X-Campus': '1' },
      },
      models: {
        'fast-model': { name: 'Fast' },
      },
    });

    const sources = getProviderSources('campus-llm', projectDir);
    expect(sources.sources.project.exists).toBe(true);
    expect(sources.sources.project.path).toBe(result.path);
  });

  test('upsertProviderConfig updates existing entry and clears disabled_providers', () => {
    const configPath = path.join(projectDir, 'opencode.json');
    writeJson(configPath, {
      provider: {
        'campus-llm': {
          npm: '@ai-sdk/openai-compatible',
          name: 'Old',
          options: { baseURL: 'https://old.example.edu/v1' },
          models: { a: { name: 'A' } },
        },
      },
      disabled_providers: ['campus-llm', 'other'],
    });

    upsertProviderConfig('campus-llm', {
      name: 'Campus LLM',
      options: { baseURL: 'https://llm.example.edu/v1' },
      models: { b: { name: 'B' } },
      env: ['CAMPUS_KEY'],
    }, projectDir, 'project');

    const written = readJson(configPath);
    expect(written.provider['campus-llm'].name).toBe('Campus LLM');
    expect(written.provider['campus-llm'].models).toEqual({ b: { name: 'B' } });
    expect(written.disabled_providers).toEqual(['other']);
  });

  test('upsert then remove restores absence', () => {
    upsertProviderConfig('temp-provider', {
      name: 'Temp',
      options: { baseURL: 'https://api.example.com/v1' },
      models: { m: { name: 'M' } },
      env: ['TEMP_KEY'],
    }, projectDir, 'project');

    expect(getProviderSources('temp-provider', projectDir).sources.project.exists).toBe(true);
    expect(removeProviderConfig('temp-provider', projectDir, 'project')).toBe(true);
    expect(getProviderSources('temp-provider', projectDir).sources.project.exists).toBe(false);
  });

  test('failed validation does not write config', () => {
    const configPath = path.join(projectDir, 'opencode.json');
    expect(() => upsertProviderConfig('ok', {
      name: 'X',
      options: { baseURL: 'not-a-url' },
      models: { m: { name: 'M' } },
      env: ['X'],
    }, projectDir, 'project')).toThrow(/Base URL/);
    expect(fs.existsSync(configPath)).toBe(false);
  });

  test('upsert with hasStoredAuth allows config without env', () => {
    const result = upsertProviderConfig('keyed-provider', {
      name: 'Keyed',
      options: { baseURL: 'https://api.example.com/v1' },
      models: { m: { name: 'M' } },
    }, projectDir, 'project', { hasStoredAuth: true });

    expect(result.providerId).toBe('keyed-provider');
    expect(result.config.env).toEqual(undefined);
  });

  test('project-scope edit updates project layer without creating a user entry', () => {
    const providerId = `proj-scope-${Date.now()}`;
    const configPath = path.join(projectDir, 'opencode.json');

    upsertProviderConfig(providerId, {
      name: 'Project Scoped',
      options: { baseURL: 'https://project.example.com/v1' },
      models: { m: { name: 'M' } },
    }, projectDir, 'project', { hasStoredAuth: true });

    upsertProviderConfig(providerId, {
      name: 'Project Scoped Updated',
      options: { baseURL: 'https://project.example.com/v2', headers: { 'X-Project': '1' } },
      models: { m: { name: 'M2' } },
    }, projectDir, 'project', { hasStoredAuth: true });

    const written = readJson(configPath);
    expect(written.provider[providerId]).toEqual({
      npm: '@ai-sdk/openai-compatible',
      name: 'Project Scoped Updated',
      options: {
        baseURL: 'https://project.example.com/v2',
        headers: { 'X-Project': '1' },
      },
      models: { m: { name: 'M2' } },
    });

    const sources = getProviderSources(providerId, projectDir);
    expect(sources.sources.project.exists).toBe(true);
    expect(sources.sources.user.exists).toBe(false);
    expect(sources.sources.custom.exists).toBe(false);

    for (const userPath of [
      path.join(os.homedir(), '.config', 'opencode', 'opencode.json'),
      path.join(os.homedir(), '.config', 'opencode', 'config.json'),
    ]) {
      if (!fs.existsSync(userPath)) continue;
      const userConfig = readJson(userPath);
      expect(userConfig.provider?.[providerId]).toBeUndefined();
      expect(userConfig.providers?.[providerId]).toBeUndefined();
    }
  });

  test('custom-scope edit updates custom layer without creating a user entry', () => {
    const providerId = `custom-scope-${Date.now()}`;
    const customPath = path.join(projectDir, 'custom-opencode.json');
    const previousEnv = process.env.OPENCODE_CONFIG;
    process.env.OPENCODE_CONFIG = customPath;

    try {
      upsertProviderConfig(providerId, {
        name: 'Custom Scoped',
        options: { baseURL: 'https://custom.example.com/v1' },
        models: { m: { name: 'M' } },
      }, projectDir, 'custom', { hasStoredAuth: true });

      upsertProviderConfig(providerId, {
        name: 'Custom Scoped Updated',
        options: { baseURL: 'https://custom.example.com/v2' },
        models: { n: { name: 'N' } },
      }, projectDir, 'custom', { hasStoredAuth: true });

      const written = readJson(customPath);
      expect(written.provider[providerId].name).toBe('Custom Scoped Updated');
      expect(written.provider[providerId].options.baseURL).toBe('https://custom.example.com/v2');

      const sources = getProviderSources(providerId, projectDir);
      expect(sources.sources.custom.exists).toBe(true);
      expect(sources.sources.user.exists).toBe(false);
      expect(sources.sources.project.exists).toBe(false);

      for (const userPath of [
        path.join(os.homedir(), '.config', 'opencode', 'opencode.json'),
        path.join(os.homedir(), '.config', 'opencode', 'config.json'),
      ]) {
        if (!fs.existsSync(userPath)) continue;
        const userConfig = readJson(userPath);
        expect(userConfig.provider?.[providerId]).toBeUndefined();
        expect(userConfig.providers?.[providerId]).toBeUndefined();
      }
    } finally {
      if (previousEnv === undefined) {
        delete process.env.OPENCODE_CONFIG;
      } else {
        process.env.OPENCODE_CONFIG = previousEnv;
      }
    }
  });
});
