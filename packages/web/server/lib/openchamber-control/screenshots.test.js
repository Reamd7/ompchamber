/**
 * 截图落盘模块（screenshots.js）的测试套件。
 *
 * 用内存版 fs 桩验证：screenshotSlug 把任意标签压成安全文件名片段（绝
 * 不构成路径），writeScreenshot 按标签 + 时间戳 + MIME 扩展名写入项目
 * 相对目录并回传可移植路径，且拒绝空目录与空图像。
 */
import { describe, expect, it } from 'vitest';
import path from 'node:path';

import { SCREENSHOT_DIRECTORY, screenshotSlug, writeScreenshot } from './screenshots.js';

/**
 * 造一个内存版 fs 桩：mkdir 记录到 made，writeFile 记录到 written，
 * 供断言"写了什么、写到哪里"而不触碰真实磁盘。
 */
const createFs = () => {
  const written = new Map();
  const made = [];
  return {
    written,
    made,
    mkdir: async (target) => { made.push(target); },
    writeFile: async (target, data) => { written.set(target, data); },
  };
};

// 标签 slug 契约：可读名保留、路径片段被压平、退化输入回退 "page"。
describe('screenshot labels', () => {
  it('keeps a readable name', () => {
    expect(screenshotSlug('Before fix')).toBe('before-fix');
  });

  it('never lets a label become a path', () => {
    expect(screenshotSlug('../../etc/passwd')).toBe('etc-passwd');
    expect(screenshotSlug('/absolute')).toBe('absolute');
    expect(screenshotSlug('..')).toBe('page');
    expect(screenshotSlug('.hidden')).toBe('hidden');
  });

  it('falls back to a name rather than an empty one', () => {
    expect(screenshotSlug('')).toBe('page');
    expect(screenshotSlug('!!!')).toBe('page');
    expect(screenshotSlug(undefined)).toBe('page');
  });
});

// 写文件契约：相对路径可移植、扩展名跟随 MIME、缺目录或空图直接拒绝。
describe('writing a screenshot', () => {
  // 真实图像字节的 base64 形式。
  const base64 = Buffer.from('image-bytes').toString('base64');

  it('writes into the project and reports a portable relative path', async () => {
    const fs = createFs();
    const result = await writeScreenshot({
      directory: '/work/project',
      base64,
      mime: 'image/jpeg',
      label: 'After fix',
      now: new Date('2026-08-13T09:37:00.000Z'),
      fs,
    });

    expect(result.path).toBe('.ompchamber/screenshots/after-fix-2026-08-13T09-37-00-000.jpg');
    expect(result.path.includes('\\')).toBe(false);
    expect(result.absolutePath).toBe(path.join('/work/project', SCREENSHOT_DIRECTORY, 'after-fix-2026-08-13T09-37-00-000.jpg'));
    expect(fs.written.get(result.absolutePath).toString()).toBe('image-bytes');
    expect(fs.made[0]).toBe(path.join('/work/project', SCREENSHOT_DIRECTORY));
  });

  it('names the file after the image it actually holds', async () => {
    const fs = createFs();
    const result = await writeScreenshot({ directory: '/work/project', base64, mime: 'image/png', fs });
    expect(result.path.endsWith('.png')).toBe(true);
  });

  it('refuses to write without a project directory', async () => {
    let failed = false;
    try {
      await writeScreenshot({ directory: '', base64, fs: createFs() });
    } catch {
      failed = true;
    }
    expect(failed).toBe(true);
  });

  it('reports an empty capture instead of writing a zero-byte file', async () => {
    const fs = createFs();
    let failed = false;
    try {
      await writeScreenshot({ directory: '/work/project', base64: '', fs });
    } catch {
      failed = true;
    }
    expect(failed).toBe(true);
    expect(fs.written.size).toBe(0);
  });
});
