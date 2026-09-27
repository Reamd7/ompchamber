/**
 * ompchamber serve 命令的 CLI 选项解析：先把环境变量（端口、UI 密码、
 * 隧道相关、API-only 等）读入默认值，再逐个扫描 argv，按 --key=value、
 * --key value 与布尔旗标三种形态覆盖默认值。--tunnel 是组合简写，同时
 * 设定隧道 provider（cloudflare）与托管本地模式。
 */

/**
 * 解析 serve 命令的 CLI 选项。
 *
 * @param {object} input
 * @param {string[]} [input.argv] 命令行参数（不含 node 与脚本路径）
 * @param {object} [input.env] 环境变量对象（默认空）
 * @param {number} input.defaultPort 未指定端口时的默认端口
 * @param {string} input.cloudflareProvider --tunnel 简写使用的 Cloudflare provider 名
 * @param {string} input.managedLocalMode --tunnel 简写使用的托管本地隧道模式名
 * @returns {{ port: number, host: string|undefined, uiPassword: string|null, tryCfTunnel: boolean,
 *   tunnelProvider: string|undefined, tunnelMode: string|undefined, tunnelConfigPath: string|null|undefined,
 *   tunnelToken: string|undefined, tunnelHostname: string|undefined, apiOnly: boolean }}
 */
export const parseServeCliOptions = ({
  argv = [],
  env = {},
  defaultPort,
  cloudflareProvider,
  managedLocalMode,
}) => {
  const args = Array.isArray(argv) ? [...argv] : [];
  const envPassword =
    env.OMPCHAMBER_UI_PASSWORD ||
    env.OPENCODE_UI_PASSWORD ||
    null;
  const envCfTunnel = env.OMPCHAMBER_TRY_CF_TUNNEL === 'true';
  const envTunnelProvider = env.OMPCHAMBER_TUNNEL_PROVIDER || undefined;
  const envTunnelMode = env.OMPCHAMBER_TUNNEL_MODE || undefined;
  const envTunnelConfigRaw = env.OMPCHAMBER_TUNNEL_CONFIG;
  const envTunnelConfig = typeof envTunnelConfigRaw === 'string'
    ? (envTunnelConfigRaw.trim().length > 0 ? envTunnelConfigRaw.trim() : null)
    : undefined;
  const envTunnelToken = env.OMPCHAMBER_TUNNEL_TOKEN || undefined;
  const envTunnelHostname = env.OMPCHAMBER_TUNNEL_HOSTNAME || undefined;
  const envApiOnly = env.OMPCHAMBER_API_ONLY === '1' || env.OMPCHAMBER_API_ONLY === 'true';

  const envPortRaw = typeof env.OMPCHAMBER_PORT === 'string' ? env.OMPCHAMBER_PORT.trim() : '';
  const envPort = Number.isFinite(parseInt(envPortRaw, 10)) ? parseInt(envPortRaw, 10) : undefined;
  const options = {
    port: envPort ?? defaultPort,
    host: undefined,
    uiPassword: envPassword,
    tryCfTunnel: envCfTunnel,
    tunnelProvider: envTunnelProvider,
    tunnelMode: envTunnelMode,
    tunnelConfigPath: envTunnelConfig,
    tunnelToken: envTunnelToken,
    tunnelHostname: envTunnelHostname,
    apiOnly: envApiOnly,
  };

  /**
   * 读取当前选项的值：优先 `--key=value` 的内联值；否则看下一个参数，
   * 只要它存在且不以 `--` 开头就当作值并把游标前移一位；都不满足则值为
   * undefined（布尔旗标语义）。
   *
   * @param {number} currentIndex 当前参数下标
   * @param {string|undefined} inlineValue `--key=value` 形式的内联值
   * @returns {{ value: string|undefined, nextIndex: number }} 解析出的值与新的游标
   */
  const consumeValue = (currentIndex, inlineValue) => {
    if (typeof inlineValue === 'string') {
      return { value: inlineValue, nextIndex: currentIndex };
    }
    const nextArg = args[currentIndex + 1];
    if (typeof nextArg === 'string' && !nextArg.startsWith('--')) {
      return { value: nextArg, nextIndex: currentIndex + 1 };
    }
    return { value: undefined, nextIndex: currentIndex };
  };

  // 逐个扫描以 -- 开头的参数：拆出选项名与可选内联值，未知选项与非选项参数直接跳过。
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (!arg.startsWith('--')) {
      continue;
    }

    const eqIndex = arg.indexOf('=');
    const optionName = eqIndex >= 0 ? arg.slice(2, eqIndex) : arg.slice(2);
    const inlineValue = eqIndex >= 0 ? arg.slice(eqIndex + 1) : undefined;

    if (optionName === 'port' || optionName === 'p') {
      const { value, nextIndex } = consumeValue(i, inlineValue);
      i = nextIndex;
      const parsedPort = parseInt(value ?? '', 10);
      options.port = Number.isFinite(parsedPort) ? parsedPort : defaultPort;
      continue;
    }

    if (optionName === 'host') {
      const { value, nextIndex } = consumeValue(i, inlineValue);
      i = nextIndex;
      options.host = typeof value === 'string' && value.trim().length > 0 ? value.trim() : undefined;
      continue;
    }

    if (optionName === 'ui-password') {
      const { value, nextIndex } = consumeValue(i, inlineValue);
      i = nextIndex;
      options.uiPassword = typeof value === 'string' ? value : '';
      continue;
    }

    if (optionName === 'api-only') {
      options.apiOnly = true;
      continue;
    }

    if (optionName === 'try-cf-tunnel') {
      options.tryCfTunnel = true;
      continue;
    }

    if (optionName === 'tunnel-provider') {
      const { value, nextIndex } = consumeValue(i, inlineValue);
      i = nextIndex;
      options.tunnelProvider = typeof value === 'string' ? value : options.tunnelProvider;
      continue;
    }

    if (optionName === 'tunnel-mode') {
      const { value, nextIndex } = consumeValue(i, inlineValue);
      i = nextIndex;
      options.tunnelMode = typeof value === 'string' ? value : options.tunnelMode;
      continue;
    }

    if (optionName === 'tunnel-config') {
      const { value, nextIndex } = consumeValue(i, inlineValue);
      i = nextIndex;
      options.tunnelConfigPath = typeof value === 'string' ? value : null;
      continue;
    }

    if (optionName === 'tunnel-token') {
      const { value, nextIndex } = consumeValue(i, inlineValue);
      i = nextIndex;
      options.tunnelToken = typeof value === 'string' ? value : options.tunnelToken;
      continue;
    }

    if (optionName === 'tunnel-hostname') {
      const { value, nextIndex } = consumeValue(i, inlineValue);
      i = nextIndex;
      options.tunnelHostname = typeof value === 'string' ? value : options.tunnelHostname;
      continue;
    }

    if (optionName === 'tunnel') {
      const { value, nextIndex } = consumeValue(i, inlineValue);
      i = nextIndex;
      options.tunnelProvider = cloudflareProvider;
      options.tunnelMode = managedLocalMode;
      options.tunnelConfigPath = typeof value === 'string' ? value : null;
    }
  }

  return options;
};
