/**
 * types 模块中隧道配置路径归一化的测试套件。
 *
 * 聚焦 Windows 平台的路径安全与语义：home 目录判定的大小写不敏感、
 * 同级目录（仅前缀相似）拒绝、~ 前缀展开到指定 home，以及逸出 home
 * 目录的路径必须抛出 validation_error（防止任意路径读写）。
 */
import { describe, expect, it } from 'bun:test';

import {
  isPathWithinDirectory,
  resolveTunnelConfigPath,
} from './types.js';

/** 配置路径校验：Windows 大小写、目录边界与 ~ 展开。 */
describe('tunnel config path normalization', () => {
  it('allows Windows home paths with different drive casing', () => {
    expect(isPathWithinDirectory(
      'c:\\Users\\Bohdan\\.cloudflared\\config.yml',
      'C:\\Users\\Bohdan',
      'win32'
    )).toBe(true);
  });

  it('does not allow Windows sibling home directories', () => {
    expect(isPathWithinDirectory(
      'C:\\Users\\Bohdan2\\.cloudflared\\config.yml',
      'C:\\Users\\Bohdan',
      'win32'
    )).toBe(false);
  });

  it('resolves Windows tilde paths inside the provided home directory', () => {
    expect(resolveTunnelConfigPath('~\\.cloudflared\\config.yml', 'C:\\Users\\Bohdan', 'win32'))
      .toBe('C:\\Users\\Bohdan\\.cloudflared\\config.yml');
  });

  it('rejects Windows paths outside the provided home directory', () => {
    expect(() => resolveTunnelConfigPath('C:\\Temp\\config.yml', 'C:\\Users\\Bohdan', 'win32'))
      .toThrow(/Config path must be within the home directory/);
  });
});
