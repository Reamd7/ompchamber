/**
 * （中文套件说明）TTS 路由测试：say 能力 promise 的等待语义、按文本语言
 * 切换 say voice、摘要兜底不再调用 zen，以及自定义 OpenAI 兼容 baseURL
 * 的本机/远程放行规则与归一化。
 */
import { describe, expect, it, afterEach } from 'vitest';
import express from 'express';
import request from 'supertest';

import { registerTtsRoutes } from './routes.js';
import { normalizeCustomOpenAIBaseURL } from './base-url.js';

/** 构建挂载了 TTS 路由的 express app（可注入假的 say 能力结果）。 */
const createApp = (sayTTSCapability = null) => {
  const app = express();
  app.use(express.json());
  registerTtsRoutes(app, {
    resolveZenModel: async () => 'gpt-5-nano',
    sayTTSCapability,
  });
  return app;
};

// TTS 路由：say 能力等待、语言切换 voice 与本地摘要兜底。
describe('tts routes', () => {
  it('waits for the authoritative macOS say capability', async () => {
    let resolveCapability;
    const capability = new Promise((resolve) => {
      resolveCapability = resolve;
    });
    const pending = request(createApp(capability)).get('/api/tts/say/status');

    resolveCapability({ available: true, voices: [{ name: 'Samantha', locale: 'en_US' }] });

    const response = await pending;
    expect(response.status).toBe(200);
    expect(response.body).toEqual({
      available: true,
      voices: [{ name: 'Samantha', locale: 'en_US' }],
    });
  });

  it('switches the say voice to the language of the text when asked to', async () => {
    const capability = Promise.resolve({
      available: true,
      voices: [
        { name: 'Samantha', locale: 'en_US' },
        { name: 'Lesya', locale: 'uk_UA' },
        { name: 'Lesya (Enhanced)', locale: 'uk_UA' },
      ],
    });
    const app = createApp(capability);
    const response = await request(app)
      .post('/api/tts/say/speak')
      .send({ text: 'Привіт! Це відповідь українською мовою, і вона досить довга.', voice: 'Samantha', language: 'auto' });

    // On macOS the route synthesizes; elsewhere it refuses before running say.
    // Either way the chosen voice must be the Ukrainian one when the platform
    // allows the request to proceed.
    if (process.platform === 'darwin') {
      expect(response.status).toBe(200);
      expect(response.headers['x-speech-voice']).toBe('Lesya (Enhanced)');
      expect(response.headers['x-speech-language']).toBe('uk');
    } else {
      expect(response.status).toBe(503);
    }
  });

  it('returns local note fallback while model summarization is retired', async () => {
    const response = await request(createApp())
      .post('/api/text/summarize')
      .send({
        text: 'First sentence. Second sentence with the useful insight.',
        threshold: 0,
        maxLength: 100,
        mode: 'note',
      });

    expect(response.status).toBe(200);
    expect(response.body).toMatchObject({
      summary: 'First sentence.',
      summarized: false,
      reason: 'Model summarization provider unavailable',
    });
  });

  it('keeps notification fallback behavior without calling zen', async () => {
    const response = await request(createApp())
      .post('/api/text/summarize')
      .send({
        text: 'Notification text that should fall back cleanly.',
        threshold: 0,
        maxLength: 100,
        mode: 'notification',
      });

    expect(response.status).toBe(200);
    expect(response.body).toMatchObject({
      summary: 'Notification text that should fall back cleanly.',
      summarized: false,
      reason: 'Model summarization provider unavailable',
    });
  });
});

// 自定义 OpenAI 兼容 baseURL：运行时环境与本机白名单决定远程放行，归一化去 query/尾斜杠。
describe('normalizeCustomOpenAIBaseURL', () => {
  const originalRuntime = process.env.OMPCHAMBER_RUNTIME;
  const originalAllowRemote = process.env.OMPCHAMBER_ALLOW_REMOTE_OPENAI_COMPAT_URLS;

  afterEach(() => {
    // Restore env vars after each test
    if (originalRuntime === undefined) {
      delete process.env.OMPCHAMBER_RUNTIME;
    } else {
      process.env.OMPCHAMBER_RUNTIME = originalRuntime;
    }
    if (originalAllowRemote === undefined) {
      delete process.env.OMPCHAMBER_ALLOW_REMOTE_OPENAI_COMPAT_URLS;
    } else {
      process.env.OMPCHAMBER_ALLOW_REMOTE_OPENAI_COMPAT_URLS = originalAllowRemote;
    }
  });

  it('rejects remote URLs when OMPCHAMBER_RUNTIME is not set (web)', () => {
    delete process.env.OMPCHAMBER_RUNTIME;
    delete process.env.OMPCHAMBER_ALLOW_REMOTE_OPENAI_COMPAT_URLS;

    const result = normalizeCustomOpenAIBaseURL('https://my-tts-server.example.com/v1');
    expect(result.error).toMatch(/Remote custom server URLs are disabled/);
    expect(result.value).toBeUndefined();
  });

  it('allows remote URLs when OMPCHAMBER_RUNTIME is desktop', () => {
    process.env.OMPCHAMBER_RUNTIME = 'desktop';
    delete process.env.OMPCHAMBER_ALLOW_REMOTE_OPENAI_COMPAT_URLS;

    const result = normalizeCustomOpenAIBaseURL('https://my-tts-server.example.com/v1');
    expect(result.error).toBeUndefined();
    expect(result.value).toBe('https://my-tts-server.example.com/v1');
  });

  it('allows remote URLs when OMPCHAMBER_ALLOW_REMOTE_OPENAI_COMPAT_URLS is true', () => {
    delete process.env.OMPCHAMBER_RUNTIME;
    process.env.OMPCHAMBER_ALLOW_REMOTE_OPENAI_COMPAT_URLS = 'true';

    const result = normalizeCustomOpenAIBaseURL('https://my-tts-server.example.com/v1');
    expect(result.error).toBeUndefined();
    expect(result.value).toBe('https://my-tts-server.example.com/v1');
  });

  it('allows localhost URLs regardless of runtime', () => {
    delete process.env.OMPCHAMBER_RUNTIME;
    delete process.env.OMPCHAMBER_ALLOW_REMOTE_OPENAI_COMPAT_URLS;

    const result = normalizeCustomOpenAIBaseURL('http://localhost:8880/v1');
    expect(result.error).toBeUndefined();
    expect(result.value).toBe('http://localhost:8880/v1');
  });

  it('strips query strings and trailing slashes', () => {
    process.env.OMPCHAMBER_RUNTIME = 'desktop';

    const result = normalizeCustomOpenAIBaseURL('https://my-server.com/v1/?key=123');
    expect(result.value).toBe('https://my-server.com/v1');
  });

  it('denies remote URLs on desktop when env var is explicitly false', () => {
    process.env.OMPCHAMBER_RUNTIME = 'desktop';
    process.env.OMPCHAMBER_ALLOW_REMOTE_OPENAI_COMPAT_URLS = 'false';

    const result = normalizeCustomOpenAIBaseURL('https://my-tts-server.example.com/v1');
    expect(result.error).toMatch(/Remote custom server URLs are disabled/);
    expect(result.value).toBeUndefined();
  });
});
