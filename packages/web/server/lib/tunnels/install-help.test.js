/**
 * install-help 模块测试套件。
 *
 * 验证各提供商在各平台返回的依赖名、安装命令、下载页与缺失提示文案
 * （Windows winget、macOS Homebrew、Linux 下载页指引），确保 UI 展示的
 * 安装指引与当前推荐一致。
 */
import { describe, expect, it } from 'bun:test';

import { getTunnelDependencyInstallInfo } from './install-help.js';
import {
  TUNNEL_PROVIDER_CLOUDFLARE,
  TUNNEL_PROVIDER_NGROK,
} from './types.js';

/** 按提供商与平台校验安装指引的内容与提示文案。 */
describe('getTunnelDependencyInstallInfo', () => {
  it('returns Windows cloudflared winget guidance', () => {
    const info = getTunnelDependencyInstallInfo(TUNNEL_PROVIDER_CLOUDFLARE, 'win32');

    expect(info.dependency).toBe('cloudflared');
    expect(info.installCommand).toBe('winget install --id Cloudflare.cloudflared');
    expect(info.message).toContain('Cloudflare.cloudflared');
  });

  it('returns Windows ngrok winget guidance', () => {
    const info = getTunnelDependencyInstallInfo(TUNNEL_PROVIDER_NGROK, 'win32');

    expect(info.dependency).toBe('ngrok');
    expect(info.installCommand).toBe('winget install ngrok -s msstore');
    expect(info.message).toContain('ngrok -s msstore');
  });

  it('keeps macOS Homebrew guidance', () => {
    const info = getTunnelDependencyInstallInfo(TUNNEL_PROVIDER_CLOUDFLARE, 'darwin');

    expect(info.installCommand).toBe('brew install cloudflared');
  });

  it('returns the current Linux cloudflared download guidance', () => {
    const info = getTunnelDependencyInstallInfo(TUNNEL_PROVIDER_CLOUDFLARE, 'linux');
    const downloadUrl = 'https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/downloads/';

    expect(info.installUrl).toBe(downloadUrl);
    expect(info.installCommand).toBe(`Download cloudflared from ${downloadUrl}`);
    expect(info.message).toContain(downloadUrl);
  });
});
