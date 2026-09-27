/**
 * 隧道依赖安装指引模块。
 *
 * 维护各提供商（cloudflared / ngrok）在不同平台（darwin / win32 / linux）的
 * 依赖名、安装命令与官方下载页，并生成依赖缺失时展示给用户的提示文案。
 * 被 providers 适配器与 tunnelService 的 missing_dependency 错误路径共用。
 */
import {
  TUNNEL_PROVIDER_CLOUDFLARE,
  TUNNEL_PROVIDER_NGROK,
} from './types.js';

/**
 * 各提供商的安装信息表：dependency 依赖名、installUrl 官方下载页、
 * commands 按平台键（darwin/win32/linux）给出的安装命令。
 */
const PROVIDER_INSTALL_INFO = {
  [TUNNEL_PROVIDER_CLOUDFLARE]: {
    dependency: 'cloudflared',
    installUrl: 'https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/downloads/',
    commands: {
      darwin: 'brew install cloudflared',
      win32: 'winget install --id Cloudflare.cloudflared',
      linux: 'Download cloudflared from https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/downloads/',
    },
  },
  [TUNNEL_PROVIDER_NGROK]: {
    dependency: 'ngrok',
    installUrl: 'https://ngrok.com/download',
    commands: {
      darwin: 'brew install ngrok',
      win32: 'winget install ngrok -s msstore',
      linux: 'Download ngrok from https://ngrok.com/download',
    },
  },
};

/**
 * 平台归一化：仅接受 darwin/win32/linux 三个键，其余值（含未知平台）一律按 linux
 * 处理，保证总能查到安装命令。
 */
const normalizeInstallPlatform = (platform) => {
  if (platform === 'darwin' || platform === 'win32' || platform === 'linux') {
    return platform;
  }
  return 'linux';
};

/**
 * 生成“依赖未安装”的提示消息：命令以 'Download ' 开头（无包管理器可用）时直接
 * 拼接指引，否则格式化为 'Install it with: <command>'。
 */
const createMissingDependencyMessage = ({ dependency, installCommand }) => {
  if (installCommand.startsWith('Download ')) {
    return `${dependency} is not installed. ${installCommand}`;
  }
  return `${dependency} is not installed. Install it with: ${installCommand}`;
};

/**
 * 获取指定提供商在指定平台的安装指引。
 * 未知提供商回退 cloudflare 信息，未知平台回退 linux 命令。
 * @param {string} provider 提供商 id（cloudflare / ngrok）
 * @param {string} platform 平台标识，默认 process.platform
 * @returns {{dependency: string, installCommand: string, installUrl: string, platform: string, message: string}}
 */
export function getTunnelDependencyInstallInfo(provider, platform = process.platform) {
  const providerInfo = PROVIDER_INSTALL_INFO[provider] || PROVIDER_INSTALL_INFO[TUNNEL_PROVIDER_CLOUDFLARE];
  const normalizedPlatform = normalizeInstallPlatform(platform);
  const installCommand = providerInfo.commands[normalizedPlatform] || providerInfo.commands.linux;

  return {
    dependency: providerInfo.dependency,
    installCommand,
    installUrl: providerInfo.installUrl,
    platform: normalizedPlatform,
    message: createMissingDependencyMessage({
      dependency: providerInfo.dependency,
      installCommand,
    }),
  };
}
