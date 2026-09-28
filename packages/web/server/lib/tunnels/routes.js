/**
 * 隧道 HTTP 路由模块。
 *
 * 通过工厂函数 createTunnelRoutesRuntime 创建并注册一组 Express 路由，对外暴露
 * 隧道相关 REST API：提供商依赖可用性检查、doctor 诊断、提供商能力列表、
 * 隧道状态查询、托管远端隧道 token 预设管理、隧道启动/停止等。
 * 全部依赖（crypto、URL、tunnelService、tunnelProviderRegistry、tunnelAuthController、
 * 各 normalize/校验函数与运行时状态存取器）均由调用方注入，便于测试替换。
 */

/**
 * 创建隧道路由运行时。
 * @param {object} dependencies 注入依赖：crypto/URL 全局对象、tunnelService 隧道服务、
 *   tunnelProviderRegistry 提供商注册表、tunnelAuthController 隧道鉴权控制器、
 *   readSettingsFromDiskMigrated/readManagedRemoteTunnelConfigFromDisk 磁盘读取函数、
 *   各类 normalize/isSupported 校验函数、托管远端隧道 token 的读写函数、
 *   模式与提供商常量、TunnelServiceError、getActivePort 及活动隧道控制器的存取器。
 * @returns {{registerRoutes: Function, startTunnelWithNormalizedRequest: Function}}
 *   返回路由注册函数与归一化启动函数，供 server 装配层使用。
 */
export const createTunnelRoutesRuntime = (dependencies) => {
  const {
    crypto,
    URL,
    tunnelService,
    tunnelProviderRegistry,
    tunnelAuthController,
    readSettingsFromDiskMigrated,
    readManagedRemoteTunnelConfigFromDisk,
    normalizeTunnelProvider,
    normalizeTunnelMode,
    normalizeOptionalPath,
    normalizeManagedRemoteTunnelHostname,
    normalizeTunnelBootstrapTtlMs,
    normalizeTunnelSessionTtlMs,
    isSupportedTunnelMode,
    upsertManagedRemoteTunnelToken,
    resolveManagedRemoteTunnelToken,
    TUNNEL_MODE_QUICK,
    TUNNEL_MODE_MANAGED_LOCAL,
    TUNNEL_MODE_MANAGED_REMOTE,
    TUNNEL_PROVIDER_CLOUDFLARE,
    TunnelServiceError,
    getActivePort,
    getRuntimeManagedRemoteTunnelHostname,
    setRuntimeManagedRemoteTunnelHostname,
    getRuntimeManagedRemoteTunnelToken,
    setRuntimeManagedRemoteTunnelToken,
    getActiveTunnelController,
    setActiveTunnelController,
  } = dependencies;

  /**
   * 读取当前活动隧道的模式并归一化为三种合法模式之一；
   * 未知或缺失时回退为 TUNNEL_MODE_QUICK。
   * @returns {string} 归一化后的活动模式
   */
  const resolveActiveNormalizedTunnelMode = () => {
    const mode = tunnelService.resolveActiveMode();
    if (mode === TUNNEL_MODE_MANAGED_LOCAL) {
      return TUNNEL_MODE_MANAGED_LOCAL;
    }
    if (mode === TUNNEL_MODE_MANAGED_REMOTE) {
      return TUNNEL_MODE_MANAGED_REMOTE;
    }
    return TUNNEL_MODE_QUICK;
  };

  /**
   * 从隧道公网 URL 中解析 hostname（转小写）。
   * @param {string} publicUrl 隧道公开地址
   * @returns {string|null} 入参非字符串、为空或 URL 解析失败时返回 null
   */
  const resolveNormalizedTunnelHost = (publicUrl) => {
    if (typeof publicUrl !== 'string' || publicUrl.trim().length === 0) {
      return null;
    }
    try {
      return new URL(publicUrl).hostname.toLowerCase();
    } catch {
      return null;
    }
  };

  /**
   * 解析首选隧道提供商，优先级：请求体显式 provider > 当前活动隧道的 provider >
   * 磁盘设置中的 tunnelProvider（normalizeTunnelProvider 保证最终有合法默认值）。
   * @param {object|null} reqBody 请求体（可为 null）
   * @returns {Promise<string>} 归一化后的提供商 id
   */
  const resolvePreferredTunnelProvider = async (reqBody = null) => {
    if (typeof reqBody?.provider === 'string' && reqBody.provider.trim().length > 0) {
      return normalizeTunnelProvider(reqBody.provider);
    }
    const activeProvider = tunnelService.resolveActiveProvider();
    if (activeProvider) {
      return normalizeTunnelProvider(activeProvider);
    }
    const settings = await readSettingsFromDiskMigrated();
    return normalizeTunnelProvider(settings?.tunnelProvider);
  };

  /**
   * 使用已完成归一化的参数启动隧道。
   * cloudflare + managed-remote 组合会先把 hostname/token 写入进程运行时状态，
   * 并在 token 与 hostname 同时存在时将其作为预设持久化到托管远端隧道配置。
   * @param {object} params 归一化后的启动参数（provider/mode/intent/hostname/token/
   *   configPath/selectedPresetId/selectedPresetName）
   * @returns {Promise<{publicUrl: string, mode: string, provider: string, providerMetadata: object|null}>}
   * @throws {TunnelServiceError} 由 tunnelService.start 透传（依赖缺失、校验失败、启动失败等）
   */
  const startTunnelWithNormalizedRequest = async ({
    provider,
    mode,
    intent,
    hostname,
    token,
    configPath,
    selectedPresetId,
    selectedPresetName,
  }) => {
    if (provider === TUNNEL_PROVIDER_CLOUDFLARE && mode === TUNNEL_MODE_MANAGED_REMOTE) {
      setRuntimeManagedRemoteTunnelHostname(hostname);
      setRuntimeManagedRemoteTunnelToken(token);

      if (token && hostname) {
        await upsertManagedRemoteTunnelToken({
          id: selectedPresetId || hostname,
          name: selectedPresetName || hostname,
          hostname,
          token,
        });
      }
    }

    const result = await tunnelService.start({
      provider,
      mode,
      intent,
      configPath,
      token,
      hostname,
    });

    console.log(`Tunnel active (${result.provider}): ${result.publicUrl}`);
    return {
      publicUrl: result.publicUrl,
      mode: result.activeMode,
      provider: result.provider,
      providerMetadata: result.providerMetadata,
    };
  };

  /**
   * 为未实现 diagnose 的提供商生成通用的“按模式”检查结果：
   * 先构造 startup_readiness 检查项，再逐一核对 requiredFields 中的字段在
   * doctorRequest 里是否已提供（字符串需非空白，其余按真值判断）。
   * @param {object} params modeKey 模式 key；requiredFields 该模式必填字段名列表；
   *   doctorRequest 汇总后的诊断输入；startupReady 提供商依赖检查是否通过
   * @returns {{mode: string, checks: Array, summary: {ready: boolean, failures: number, warnings: number}, ready: boolean, blockers: Array}}
   *   blockers 剔除 startup_readiness，仅保留用户可处理的阻塞项
   */
  const createGenericModeChecks = ({ modeKey, requiredFields, doctorRequest, startupReady }) => {
    const checks = [
      {
        id: 'startup_readiness',
        label: 'Provider startup readiness',
        status: startupReady ? 'pass' : 'fail',
        detail: startupReady
          ? 'Provider dependency checks passed.'
          : 'Resolve provider checks before starting tunnels.',
      },
    ];

    for (const field of requiredFields) {
      const value = doctorRequest?.[field];
      const present = typeof value === 'string' ? value.trim().length > 0 : Boolean(value);
      checks.push({
        id: `requirement_${field}`,
        label: `Required: ${field}`,
        status: present ? 'pass' : 'fail',
        detail: present
          ? `${field} is configured.`
          : `${field} is required for ${modeKey}.`,
      });
    }

    const failures = checks.filter((entry) => entry.status === 'fail').length;
    const warnings = checks.filter((entry) => entry.status === 'warn').length;
    return {
      mode: modeKey,
      checks,
      summary: {
        ready: failures === 0,
        failures,
        warnings,
      },
      ready: failures === 0,
      blockers: checks
        .filter((entry) => entry.status === 'fail' && entry.id !== 'startup_readiness')
        .map((entry) => entry.detail || entry.label || entry.id),
    };
  };

  /**
   * 执行隧道 doctor 诊断。
   * 提供商自身实现了 diagnose 时直接委托（并按 modeFilter 过滤其返回的模式列表）；
   * 否则走通用路径：检查依赖可用性得到 providerChecks，再按 capabilities.modes
   * 逐模式调用 createGenericModeChecks 生成检查结果。
   * @param {object} params providerId 提供商 id；modeFilter 可选的模式过滤；
   *   doctorRequest 汇总后的诊断输入
   * @returns {Promise<{ok: boolean, provider: string, providerChecks: Array, modes: Array}>}
   * @throws {TunnelServiceError} provider_unsupported（注册表中无此提供商）或
   *   mode_unsupported（提供商能力未声明该模式）
   */
  const runTunnelDoctor = async ({ providerId, modeFilter, doctorRequest }) => {
    const provider = tunnelProviderRegistry.get(providerId);
    if (!provider) {
      throw new TunnelServiceError('provider_unsupported', `Unsupported tunnel provider: ${providerId}`);
    }

    const capabilities = provider.capabilities || {};
    const modeKeys = Array.isArray(capabilities.modes)
      ? capabilities.modes.map((entry) => entry?.key).filter((key) => typeof key === 'string' && key.length > 0)
      : [];

    if (modeFilter && !modeKeys.includes(modeFilter)) {
      throw new TunnelServiceError('mode_unsupported', `Provider '${providerId}' does not support mode '${modeFilter}'`);
    }

    if (typeof provider.diagnose === 'function') {
      const diagnosed = await provider.diagnose({
        ...doctorRequest,
        mode: modeFilter || doctorRequest?.mode,
      }, {
        capabilities,
      });
      const providerChecks = Array.isArray(diagnosed?.providerChecks) ? diagnosed.providerChecks : [];
      const allModes = Array.isArray(diagnosed?.modes) ? diagnosed.modes : [];
      const modes = modeFilter ? allModes.filter((entry) => entry?.mode === modeFilter) : allModes;
      return {
        ok: true,
        provider: providerId,
        providerChecks,
        modes,
      };
    }

    const availability = await tunnelService.checkAvailability(providerId);
    const dependencyAvailable = Boolean(availability?.available);
    const providerChecks = [{
      id: 'dependency',
      label: 'Provider dependency',
      status: dependencyAvailable ? 'pass' : 'fail',
      detail: dependencyAvailable
        ? (availability?.version || 'available')
        : (availability?.message || 'Required provider dependency is unavailable.'),
    }];

    const targetModes = (Array.isArray(capabilities.modes) ? capabilities.modes : [])
      .filter((entry) => !modeFilter || entry?.key === modeFilter);
    const modes = targetModes.map((entry) => createGenericModeChecks({
      modeKey: entry.key,
      requiredFields: Array.isArray(entry?.requires) ? entry.requires : [],
      doctorRequest,
      startupReady: dependencyAvailable,
    }));

    return {
      ok: true,
      provider: providerId,
      providerChecks,
      modes,
    };
  };

  /**
   * 在 Express app 上注册全部隧道 REST 路由（/api/ompchamber/tunnel/*）。
   * @param {object} app Express 应用实例
   */
  const registerRoutes = (app) => {
    // GET /tunnel/check：检查（指定或首选）提供商 CLI 依赖可用性并附带安装指引；
    // 任何异常都兜底为 available:false 的 JSON，避免诊断接口本身 5xx。
    app.get('/api/ompchamber/tunnel/check', async (req, res) => {
      try {
        const requestedProvider = typeof req?.query?.provider === 'string' && req.query.provider.trim().length > 0
          ? normalizeTunnelProvider(req.query.provider)
          : await resolvePreferredTunnelProvider();
        const result = await tunnelService.checkAvailability(requestedProvider);
        res.json({
          available: result.available,
          provider: requestedProvider,
          version: result.version || null,
          dependency: result.dependency || null,
          installCommand: result.installCommand || null,
          installUrl: result.installUrl || null,
          platform: result.platform || process.platform,
          message: result.message || null,
        });
      } catch (error) {
        console.warn('Tunnel dependency check failed:', error);
        res.json({ available: false, provider: null, version: null, dependency: null, installCommand: null, installUrl: null, platform: process.platform, message: null });
      }
    });

    /**
     * GET/POST /api/ompchamber/tunnel/doctor 的共享处理器。
     * 汇总 query/body 与磁盘设置，按“请求显式值 > 进程运行时状态 > 托管配置文件 >
     * 设置存储”的优先级解析出 provider、mode 过滤、hostname 与 token，再交由
     * runTunnelDoctor 产出诊断结果。
     * @returns {Promise<void>} 成功返回诊断 JSON；TunnelServiceError 映射 400，
     *   其余异常记日志后映射 500
     */
    const handleTunnelDoctor = async (req, res) => {
      try {
        const params = req.query || {};
        const body = req.body || {};

        const providerId = typeof params.provider === 'string' && params.provider.trim().length > 0
          ? normalizeTunnelProvider(params.provider)
          : await resolvePreferredTunnelProvider();
        const modeFilter = typeof params.mode === 'string' && params.mode.trim().length > 0
          ? params.mode.trim().toLowerCase()
          : null;

        const settings = await readSettingsFromDiskMigrated();
        const selectedPresetId = typeof params.managedRemoteTunnelPresetId === 'string'
          ? params.managedRemoteTunnelPresetId.trim()
          : '';
        const requestConfigPath = normalizeOptionalPath(params.configPath)
          ?? normalizeOptionalPath(settings?.managedLocalTunnelConfigPath);
        const requestManagedRemoteHostname = normalizeManagedRemoteTunnelHostname(params.managedRemoteTunnelHostname);
        const requestTunnelHostname = normalizeManagedRemoteTunnelHostname(params.tunnelHostname);
        const requestHostname = normalizeManagedRemoteTunnelHostname(params.hostname);
        const hostnameFromSettings = normalizeManagedRemoteTunnelHostname(settings?.managedRemoteTunnelHostname);
        const hostname = requestHostname || requestTunnelHostname || requestManagedRemoteHostname || hostnameFromSettings;

        const requestManagedRemoteToken = typeof body.managedRemoteTunnelToken === 'string'
          ? body.managedRemoteTunnelToken.trim()
          : '';
        const requestTunnelToken = typeof body.tunnelToken === 'string'
          ? body.tunnelToken.trim()
          : '';
        const requestToken = typeof body.token === 'string'
          ? body.token.trim()
          : '';
        const requestTokenProvided = body.managedRemoteTunnelTokenProvided === true
          || body.tunnelTokenProvided === true
          || body.tokenProvided === true;
        const requestHostnameProvided = body.managedRemoteTunnelHostnameProvided === true
          || body.tunnelHostnameProvided === true
          || body.hostnameProvided === true;
        const storedManagedRemoteToken = typeof settings?.managedRemoteTunnelToken === 'string'
          ? settings.managedRemoteTunnelToken.trim()
          : '';
        const managedRemoteTunnelConfig = await readManagedRemoteTunnelConfigFromDisk();
        const serverHasSavedManagedRemoteProfile = managedRemoteTunnelConfig.tunnels.some((entry) => {
          const savedHostname = normalizeManagedRemoteTunnelHostname(entry?.hostname);
          const savedToken = typeof entry?.token === 'string' ? entry.token.trim() : '';
          return Boolean(savedHostname && savedToken);
        });
        const cliHasSavedManagedRemoteProfile = params.hasSavedManagedRemoteProfile === '1';
        const hasSavedManagedRemoteProfile = serverHasSavedManagedRemoteProfile || cliHasSavedManagedRemoteProfile;
        const configManagedRemoteToken = providerId === TUNNEL_PROVIDER_CLOUDFLARE
          ? await resolveManagedRemoteTunnelToken({ presetId: selectedPresetId, hostname })
          : '';
        const runtimeHostname = getRuntimeManagedRemoteTunnelHostname();
        const runtimeToken = getRuntimeManagedRemoteTunnelToken();
        const token = requestToken
          || requestTunnelToken
          || requestManagedRemoteToken
          || ((runtimeHostname && hostname && runtimeHostname === hostname) ? runtimeToken : '')
          || configManagedRemoteToken
          || storedManagedRemoteToken;

        const doctorRequest = {
          mode: modeFilter,
          hostname,
          token,
          tokenProvided: requestTokenProvided,
          hostnameProvided: requestHostnameProvided,
          configPath: requestConfigPath,
          hasSavedManagedRemoteProfile,
        };

        const result = await runTunnelDoctor({
          providerId,
          modeFilter,
          doctorRequest,
        });
        return res.json(result);
      } catch (error) {
        if (error instanceof TunnelServiceError) {
          return res.status(400).json({ ok: false, error: error.message, code: error.code });
        }
        console.warn('Tunnel doctor failed:', error);
        return res.status(500).json({ ok: false, error: 'Failed to run tunnel doctor' });
      }
    };
    // doctor 同时接受 GET（链接直达/浏览器）与 POST（可携带 token 请求体）两种调用方式。
    app.post('/api/ompchamber/tunnel/doctor', handleTunnelDoctor);
    app.get('/api/ompchamber/tunnel/doctor', handleTunnelDoctor);

    // GET /tunnel/providers：列出全部已注册提供商的能力声明（capabilities 浅拷贝）。
    app.get('/api/ompchamber/tunnel/providers', (_req, res) => {
      const providers = tunnelProviderRegistry.listCapabilities();
      return res.json({ providers });
    });

    // GET /tunnel/status：汇总隧道与鉴权状态（活动地址、模式、预设、TTL 配置、会话列表）。
    // 无公网地址时返回 inactive 形态；检测到活动隧道与 auth 控制器状态不一致时自动补同步。
    app.get('/api/ompchamber/tunnel/status', async (_req, res) => {
      try {
        const settings = await readSettingsFromDiskMigrated();
        const normalizedMode = normalizeTunnelMode(settings?.tunnelMode);
        const managedRemoteHostname = normalizeManagedRemoteTunnelHostname(settings?.managedRemoteTunnelHostname);
        const managedRemoteTunnelConfig = await readManagedRemoteTunnelConfigFromDisk();
        const managedRemoteTunnelPresetSummaries = managedRemoteTunnelConfig.tunnels.map((entry) => ({
          id: entry.id,
          name: entry.name,
          hostname: entry.hostname,
        }));
        const hasStoredManagedRemoteToken = typeof settings?.managedRemoteTunnelToken === 'string' && settings.managedRemoteTunnelToken.trim().length > 0;
        const hasManagedRemoteTunnelToken = getRuntimeManagedRemoteTunnelToken().length > 0 || managedRemoteTunnelConfig.tunnels.length > 0 || hasStoredManagedRemoteToken;
        const bootstrapTtlMs = settings?.tunnelBootstrapTtlMs === null
          ? null
          : normalizeTunnelBootstrapTtlMs(settings?.tunnelBootstrapTtlMs);
        const sessionTtlMs = normalizeTunnelSessionTtlMs(settings?.tunnelSessionTtlMs);
        const activeSessions = tunnelAuthController.listTunnelSessions();
        const activeProvider = tunnelService.resolveActiveProvider();
        const provider = activeProvider || normalizeTunnelProvider(settings?.tunnelProvider);

        const publicUrl = tunnelService.getPublicUrl();
        if (!publicUrl) {
          return res.json({
            active: false,
            url: null,
            mode: normalizedMode,
            provider,
            providerMetadata: null,
            hasManagedRemoteTunnelToken,
            managedRemoteTunnelHostname: managedRemoteHostname || null,
            managedRemoteTunnelPresets: managedRemoteTunnelPresetSummaries,
            managedRemoteTunnelTokenPresetIds: managedRemoteTunnelConfig.tunnels.map((entry) => entry.id),
            hasBootstrapToken: false,
            bootstrapExpiresAt: null,
            policy: 'tunnel-gated',
            activeTunnelMode: tunnelAuthController.getActiveTunnelMode() || null,
            activeSessions,
            localPort: getActivePort(),
            ttlConfig: {
              bootstrapTtlMs,
              sessionTtlMs,
            },
          });
        }

        const activeNormalizedMode = resolveActiveNormalizedTunnelMode();
        const activeTunnelId = tunnelAuthController.getActiveTunnelId();
        const activeTunnelHost = tunnelAuthController.getActiveTunnelHost();
        const resolvedTunnelHost = resolveNormalizedTunnelHost(publicUrl);
        const activeTunnelMode = tunnelAuthController.getActiveTunnelMode();
        const needsActiveTunnelSync = !activeTunnelId
          || !activeTunnelHost
          || !resolvedTunnelHost
          || activeTunnelHost !== resolvedTunnelHost
          || activeTunnelMode !== activeNormalizedMode;
        if (needsActiveTunnelSync) {
          tunnelAuthController.setActiveTunnel({
            tunnelId: activeTunnelId || crypto.randomUUID(),
            publicUrl,
            mode: activeNormalizedMode,
          });
        }

        const bootstrapStatus = tunnelAuthController.getBootstrapStatus();
        const providerMetadata = tunnelService.getProviderMetadata();

        return res.json({
          active: true,
          url: publicUrl,
          mode: activeNormalizedMode,
          provider,
          providerMetadata,
          hasManagedRemoteTunnelToken,
          managedRemoteTunnelHostname: managedRemoteHostname || null,
          managedRemoteTunnelPresets: managedRemoteTunnelPresetSummaries,
          managedRemoteTunnelTokenPresetIds: managedRemoteTunnelConfig.tunnels.map((entry) => entry.id),
          hasBootstrapToken: bootstrapStatus.hasBootstrapToken,
          bootstrapExpiresAt: bootstrapStatus.bootstrapExpiresAt,
          policy: 'tunnel-gated',
          activeTunnelMode: activeNormalizedMode,
          activeSessions: tunnelAuthController.listTunnelSessions(),
          localPort: getActivePort(),
          ttlConfig: {
            bootstrapTtlMs,
            sessionTtlMs,
          },
        });
      } catch (error) {
        return res.status(500).json({ error: 'Failed to get tunnel status' });
      }
    });

    // PUT /tunnel/managed-remote-token：保存/更新托管远端隧道 token 预设；
    // presetId/presetName/hostname/token 四字段必填，缺一即 400，落盘失败 500。
    app.put('/api/ompchamber/tunnel/managed-remote-token', async (req, res) => {
      try {
        const presetId = typeof req?.body?.presetId === 'string' ? req.body.presetId.trim() : '';
        const presetName = typeof req?.body?.presetName === 'string' ? req.body.presetName.trim() : '';
        const managedRemoteTunnelHostname = normalizeManagedRemoteTunnelHostname(req?.body?.managedRemoteTunnelHostname);
        const managedRemoteTunnelToken = typeof req?.body?.managedRemoteTunnelToken === 'string' ? req.body.managedRemoteTunnelToken.trim() : '';

        if (!presetId || !presetName || !managedRemoteTunnelHostname || !managedRemoteTunnelToken) {
          return res.status(400).json({ ok: false, error: 'presetId, presetName, managedRemoteTunnelHostname and managedRemoteTunnelToken are required' });
        }

        await upsertManagedRemoteTunnelToken({
          id: presetId,
          name: presetName,
          hostname: managedRemoteTunnelHostname,
          token: managedRemoteTunnelToken,
        });

        const managedRemoteTunnelConfig = await readManagedRemoteTunnelConfigFromDisk();
        return res.json({ ok: true, managedRemoteTunnelTokenPresetIds: managedRemoteTunnelConfig.tunnels.map((entry) => entry.id) });
      } catch (error) {
        return res.status(500).json({ ok: false, error: 'Failed to save managed remote tunnel token' });
      }
    });

    // POST /tunnel/start：启动隧道。校验 provider/mode 合法性，按优先级汇聚 token/hostname
    // 与 TTL 配置；若替换了已有隧道（模式/提供商/地址变化）先吊销其 bootstrap token 与会话；
    // 成功后签发新的 bootstrap token 并返回 connect URL。TunnelServiceError 按 code 映射
    // 400/422/500，任何失败都会清空活动隧道状态。
    app.post('/api/ompchamber/tunnel/start', async (_req, res) => {
      try {
        const settings = await readSettingsFromDiskMigrated();
        if (typeof _req?.body?.provider === 'string' && _req.body.provider.trim().length > 0) {
          const rawProvider = _req.body.provider.trim().toLowerCase();
          if (!tunnelProviderRegistry.get(rawProvider)) {
            return res.status(422).json({ ok: false, error: `Unsupported tunnel provider: ${rawProvider}`, code: 'provider_unsupported' });
          }
        }
        const provider = normalizeTunnelProvider(_req?.body?.provider ?? settings?.tunnelProvider);
        const modeInput = _req?.body?.mode ?? settings?.tunnelMode;
        const intent = typeof _req?.body?.intent === 'string' ? _req.body.intent.trim().toLowerCase() : undefined;
        const mode = typeof modeInput === 'string'
          ? modeInput.trim().toLowerCase()
          : normalizeTunnelMode(modeInput);
        if (typeof _req?.body?.mode === 'string' && _req.body.mode.trim().length > 0 && !isSupportedTunnelMode(mode)) {
          return res.status(422).json({ ok: false, error: `Unsupported tunnel mode: ${mode}`, code: 'mode_unsupported' });
        }
        const selectedPresetId = typeof _req?.body?.managedRemoteTunnelPresetId === 'string' ? _req.body.managedRemoteTunnelPresetId.trim() : '';
        const selectedPresetName = typeof _req?.body?.managedRemoteTunnelPresetName === 'string' ? _req.body.managedRemoteTunnelPresetName.trim() : '';
        const requestConfigPath = normalizeOptionalPath(_req?.body?.configPath)
          ?? normalizeOptionalPath(settings?.managedLocalTunnelConfigPath);
        const requestManagedRemoteHostname = normalizeManagedRemoteTunnelHostname(_req?.body?.managedRemoteTunnelHostname);
        const requestTunnelHostname = normalizeManagedRemoteTunnelHostname(_req?.body?.tunnelHostname);
        const requestHostname = normalizeManagedRemoteTunnelHostname(_req?.body?.hostname);
        const hostnameFromSettings = normalizeManagedRemoteTunnelHostname(settings?.managedRemoteTunnelHostname);
        const hostname = requestHostname || requestTunnelHostname || requestManagedRemoteHostname || hostnameFromSettings;
        const requestManagedRemoteToken = typeof _req?.body?.managedRemoteTunnelToken === 'string' ? _req.body.managedRemoteTunnelToken.trim() : '';
        const requestTunnelToken = typeof _req?.body?.tunnelToken === 'string' ? _req.body.tunnelToken.trim() : '';
        const requestToken = typeof _req?.body?.token === 'string' ? _req.body.token.trim() : '';
        const storedManagedRemoteToken = typeof settings?.managedRemoteTunnelToken === 'string' ? settings.managedRemoteTunnelToken.trim() : '';
        const configManagedRemoteToken = provider === TUNNEL_PROVIDER_CLOUDFLARE
          ? await resolveManagedRemoteTunnelToken({ presetId: selectedPresetId, hostname })
          : '';
        const runtimeHostname = getRuntimeManagedRemoteTunnelHostname();
        const runtimeToken = getRuntimeManagedRemoteTunnelToken();
        const token = requestToken
          || requestTunnelToken
          || requestManagedRemoteToken
          || ((runtimeHostname && hostname && runtimeHostname === hostname) ? runtimeToken : '')
          || configManagedRemoteToken
          || storedManagedRemoteToken;
        const requestConnectTtlMs = typeof _req?.body?.connectTtlMs === 'number' && Number.isFinite(_req.body.connectTtlMs)
          ? normalizeTunnelBootstrapTtlMs(_req.body.connectTtlMs)
          : undefined;
        const requestSessionTtlMs = typeof _req?.body?.sessionTtlMs === 'number' && Number.isFinite(_req.body.sessionTtlMs)
          ? normalizeTunnelSessionTtlMs(_req.body.sessionTtlMs)
          : undefined;
        const bootstrapTtlMs = requestConnectTtlMs ?? (settings?.tunnelBootstrapTtlMs === null
          ? null
          : normalizeTunnelBootstrapTtlMs(settings?.tunnelBootstrapTtlMs));
        const sessionTtlMs = requestSessionTtlMs ?? normalizeTunnelSessionTtlMs(settings?.tunnelSessionTtlMs);

        const previousTunnelId = tunnelAuthController.getActiveTunnelId();
        const previousMode = tunnelAuthController.getActiveTunnelMode();
        const previousProvider = tunnelService.resolveActiveProvider();
        const previousUrl = tunnelService.getPublicUrl();

        const { publicUrl, provider: activeProvider, providerMetadata } = await startTunnelWithNormalizedRequest({
          provider,
          mode,
          intent,
          hostname,
          token,
          configPath: requestConfigPath,
          selectedPresetId,
          selectedPresetName,
        });

        const replacedTunnel = Boolean(previousTunnelId) && (
          previousMode !== mode
          || previousProvider !== activeProvider
          || previousUrl !== publicUrl
        );
        let revokedBootstrapCount = 0;
        let invalidatedSessionCount = 0;
        if (replacedTunnel && previousTunnelId) {
          const revoked = tunnelAuthController.revokeTunnelArtifacts(previousTunnelId);
          revokedBootstrapCount = revoked.revokedBootstrapCount;
          invalidatedSessionCount = revoked.invalidatedSessionCount;
        }

        tunnelAuthController.setActiveTunnel({
          tunnelId: replacedTunnel || !previousTunnelId ? crypto.randomUUID() : previousTunnelId,
          publicUrl,
          mode,
        });

        const bootstrapToken = tunnelAuthController.issueBootstrapToken({ ttlMs: bootstrapTtlMs });
        const connectUrl = `${publicUrl.replace(/\/$/, '')}/connect?t=${encodeURIComponent(bootstrapToken.token)}`;
        const managedRemoteTunnelConfig = await readManagedRemoteTunnelConfigFromDisk();
        const isCloudflareProvider = activeProvider === TUNNEL_PROVIDER_CLOUDFLARE;

        return res.json({
          ok: true,
          url: publicUrl,
          mode,
          provider: activeProvider,
          providerMetadata,
          managedRemoteTunnelHostname: isCloudflareProvider ? (hostname || null) : null,
          managedRemoteTunnelTokenPresetIds: isCloudflareProvider ? managedRemoteTunnelConfig.tunnels.map((entry) => entry.id) : [],
          connectUrl,
          bootstrapExpiresAt: bootstrapToken.expiresAt,
          replacedTunnel,
          replaced: replacedTunnel
            ? {
              mode: previousMode,
              provider: previousProvider,
              url: previousUrl,
            }
            : null,
          revokedBootstrapCount,
          invalidatedSessionCount,
          policy: 'tunnel-gated',
          activeTunnelMode: mode,
          activeSessions: tunnelAuthController.listTunnelSessions(),
          localPort: getActivePort(),
          ttlConfig: {
            bootstrapTtlMs,
            sessionTtlMs,
          },
        });
      } catch (error) {
        console.error('Failed to start tunnel:', error);
        setActiveTunnelController(null);
        tunnelAuthController.clearActiveTunnel();
        if (error instanceof TunnelServiceError) {
          const status = error.code === 'missing_dependency'
            ? 400
            : (error.code === 'validation_error' || error.code === 'provider_unsupported' || error.code === 'mode_unsupported'
              ? 422
              : 500);
          return res.status(status).json({ ok: false, error: error.message, code: error.code });
        }
        return res.status(500).json({ ok: false, error: 'Failed to start tunnel', code: 'startup_failed' });
      }
    });

    // POST /tunnel/stop：停止活动隧道并吊销其全部鉴权产物（bootstrap token 与会话），
    // 返回被吊销的数量统计。
    app.post('/api/ompchamber/tunnel/stop', (_req, res) => {
      let revokedBootstrapCount = 0;
      let invalidatedSessionCount = 0;
      const activeTunnelId = tunnelAuthController.getActiveTunnelId();

      if (activeTunnelId) {
        const revoked = tunnelAuthController.revokeTunnelArtifacts(activeTunnelId);
        revokedBootstrapCount = revoked.revokedBootstrapCount;
        invalidatedSessionCount = revoked.invalidatedSessionCount;
      }

      if (getActiveTunnelController()) {
        console.log('Stopping active tunnel (user requested)...');
        tunnelService.stop();
      }

      tunnelAuthController.clearActiveTunnel();
      res.json({ ok: true, revokedBootstrapCount, invalidatedSessionCount });
    });
  };

  // 对外暴露路由注册入口与归一化启动函数两个能力。
  return {
    registerRoutes,
    startTunnelWithNormalizedRequest,
  };
};
