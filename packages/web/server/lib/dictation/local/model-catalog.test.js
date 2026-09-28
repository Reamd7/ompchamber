/**
 * 本地 TTS 模型目录（model-catalog.js）的单元测试，运行于 vitest。
 *
 * 覆盖：语言到模型的回退选择（保留会说该语言的选中模型、否则挑目录
 * 内首个支持模型、无覆盖时返回 null）、跨语言默认说话人，以及目录
 * 条目自身的完整性（语言非空、下载地址合法、必需文件与词典键可解析）。
 */
import { describe, expect, it } from 'vitest';
import {
  DEFAULT_LOCAL_TTS_MODEL,
  LOCAL_TTS_MODEL_CATALOG,
  getLocalSttModelSpec,
  getLocalTtsDefaultSpeaker,
  resolveLocalTtsModelForLanguage,
} from './model-catalog.js';

/** 本地 TTS 目录的语言选择与条目完整性校验。 */
describe('local TTS catalog', () => {
  it('keeps the selected model when it speaks the language', () => {
    expect(resolveLocalTtsModelForLanguage('en', DEFAULT_LOCAL_TTS_MODEL)).toBe(DEFAULT_LOCAL_TTS_MODEL);
    expect(resolveLocalTtsModelForLanguage('zh', 'kokoro-multi-lang-v1_1')).toBe('kokoro-multi-lang-v1_1');
  });

  it('picks a catalog model for a language the selected model lacks', () => {
    expect(resolveLocalTtsModelForLanguage('uk', DEFAULT_LOCAL_TTS_MODEL)).toBe('piper-uk_UA-lada-x_low');
    expect(resolveLocalTtsModelForLanguage('zh', DEFAULT_LOCAL_TTS_MODEL)).toBe('kokoro-multi-lang-v1_1');
  });

  it('returns null for a language no model covers', () => {
    expect(resolveLocalTtsModelForLanguage('xx', DEFAULT_LOCAL_TTS_MODEL)).toBeNull();
  });

  it('gives Chinese a Chinese speaker on the multi-language Kokoro', () => {
    expect(getLocalTtsDefaultSpeaker('kokoro-multi-lang-v1_1', 'zh')).toBe(3);
    expect(getLocalTtsDefaultSpeaker('kokoro-multi-lang-v1_1', 'en')).toBe(0);
    expect(getLocalTtsDefaultSpeaker('piper-uk_UA-lada-x_low', 'uk')).toBeUndefined();
  });

  it('every TTS entry declares its languages and installable files', () => {
    for (const [id, spec] of Object.entries(LOCAL_TTS_MODEL_CATALOG)) {
      expect(spec.languages.length, id).toBeGreaterThan(0);
      expect(spec.archiveUrl, id).toMatch(/^https:\/\/github\.com\/k2-fsa\/sherpa-onnx\/releases\/download\/tts-models\//);
      const resolved = getLocalSttModelSpec(id);
      expect(resolved.requiredFiles, id).toContain(spec.files.model);
      for (const key of spec.lexicon ?? []) {
        expect(spec.files[key], `${id} lexicon ${key}`).toBeTruthy();
      }
    }
  });
});
