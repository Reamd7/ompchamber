import fs from 'fs';
import os from 'os';
import path from 'path';
import { afterAll, beforeEach, describe, expect, it } from 'vitest';

/**
 * readWalkthroughModelOverride 的测试套件：验证覆盖项读取、空值回落
 * null、缺失或损坏文件不抛错，以及与 small-model 覆盖项互不影响。
 * 用独立临时数据目录隔离真实配置。
 */
// 被测模块在 import 时求值 SETTINGS_FILE 路径，环境变量必须先于动态 import 设置。
const TEMP_DATA_DIR = fs.mkdtempSync(path.join(os.tmpdir(), 'walkthrough-model-settings-'));
process.env.OMPCHAMBER_DATA_DIR = TEMP_DATA_DIR;

// 环境变量就绪后再动态引入被测模块，确保它读到的是临时目录。
const { readWalkthroughModelOverride } = await import('./model-settings.js');

// 临时目录内 settings.json 的预期路径。
const SETTINGS_FILE = path.join(TEMP_DATA_DIR, 'settings.json');

/** 将设置对象写入临时 settings.json（整体覆盖）。 */
const write = (value) => fs.writeFileSync(SETTINGS_FILE, JSON.stringify(value), 'utf8');

// 每个用例先删掉 settings.json，从"文件不存在"的干净状态起步。
describe('readWalkthroughModelOverride', () => {
  beforeEach(() => {
    fs.rmSync(SETTINGS_FILE, { force: true });
  });

  it('returns the chosen model', () => {
    write({ walkthroughModelOverride: 'anthropic/claude-haiku-4-5' });
    expect(readWalkthroughModelOverride()).toBe('anthropic/claude-haiku-4-5');
  });

  it('defers to the small model when nothing is chosen', () => {
    write({});
    expect(readWalkthroughModelOverride()).toBeNull();

    // Clearing the picker writes an empty string; that must read as "use the
    // small model", not as an override of ''.
    write({ walkthroughModelOverride: '' });
    expect(readWalkthroughModelOverride()).toBeNull();

    write({ walkthroughModelOverride: '   ' });
    expect(readWalkthroughModelOverride()).toBeNull();
  });

  it('never throws on a missing or corrupt settings file', () => {
    expect(readWalkthroughModelOverride()).toBeNull();

    fs.writeFileSync(SETTINGS_FILE, '{ not json', 'utf8');
    expect(readWalkthroughModelOverride()).toBeNull();
  });

  it('is independent of the small model override', () => {
    write({
      smallModelUseDefault: false,
      smallModelOverride: 'google/gemini-2.5-flash',
      walkthroughModelOverride: 'anthropic/claude-haiku-4-5',
    });

    expect(readWalkthroughModelOverride()).toBe('anthropic/claude-haiku-4-5');
  });
});

// 全部用例结束后清理临时数据目录。
afterAll(() => {
  fs.rmSync(TEMP_DATA_DIR, { recursive: true, force: true });
});
