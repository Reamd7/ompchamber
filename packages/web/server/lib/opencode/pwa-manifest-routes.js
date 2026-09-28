/**
 * PWA web app manifest 路由（GET /manifest.webmanifest）。
 *
 * 动态生成 manifest.webmanifest：应用名与屏幕方向优先取查询参数覆盖
 * （pwa_name/app_name/appName 与 orientation），其次取服务端设置
 * （settings.json 的 pwaAppName/pwaOrientation），最后回退默认值；并附带
 * 最近会话快捷方式（最多 3 条，来自 OpenCode /session 接口，按目录过滤、
 * 5 秒缓存）。响应带 no-store，保证设置变更即时生效。
 */
/** 未配置任何应用名时的默认 PWA 应用名。 */
const DEFAULT_PWA_APP_NAME = 'OMPChamber';
/**
 * 将设置中的 PWA 方向值映射为 Web App Manifest 的 orientation 枚举：
 * portrait -> portrait-primary，landscape -> landscape-primary，
 * 其余（system 等）返回 undefined（manifest 中省略该字段，交给系统决定）。
 */
const mapPwaOrientationToManifest = (value) => {
  if (value === 'portrait') {
    return 'portrait-primary';
  }
  if (value === 'landscape') {
    return 'landscape-primary';
  }
  return undefined;
};

/**
 * 在 express app 上注册 /manifest.webmanifest 路由。dependencies 注入
 * 运行时依赖（进程、项目目录解析、OpenCode URL 与鉴权头构造、设置读取、
 * 应用名/方向归一化），便于测试替换。
 */
export const registerPwaManifestRoute = (app, dependencies) => {
  const {
    process,
    resolveProjectDirectory,
    buildOpenCodeUrl,
    getOpenCodeAuthHeaders,
    readSettingsFromDiskMigrated,
    normalizePwaAppName,
    normalizePwaOrientation,
  } = dependencies;

  // 最近会话快捷方式缓存：key 为 "dir:<目录>" 或 "global"，value 含写入时间
  // 与数据，5 秒内直接复用。
  const recentPwaSessionsCache = new Map();

  /**
   * 计算用于 manifest shortcuts 的最近会话列表（最多 3 条）：先解析请求对应的
   * 项目目录，5 秒缓存命中则直接返回；否则查询 OpenCode /session 接口
   * （优先按目录过滤，无结果再全局查询后过滤），去重并按更新时间倒序取前 3，
   * 生成 shortcuts 条目。任何异常都降级为空数组并同样写入缓存。
   */
  const getRecentPwaSessionShortcuts = async (req) => {
    const now = Date.now();

    const resolvedDirectoryResult = await resolveProjectDirectory(req).catch(() => ({ directory: null }));
    const preferredDirectory = typeof resolvedDirectoryResult?.directory === 'string'
      ? resolvedDirectoryResult.directory
      : null;

    const cacheKey = preferredDirectory ? `dir:${preferredDirectory}` : 'global';
    const cached = recentPwaSessionsCache.get(cacheKey);
    if (cached && now - cached.at < 5000) {
      return cached.data;
    }

    /** 将标题归一化为非空字符串并截断到 48 个字符，空值回退 fallback。 */
    const normalizeShortcutTitle = (value, fallback) => {
      const normalized = normalizePwaAppName(value, fallback);
      return normalized.length > 48 ? normalized.slice(0, 48) : normalized;
    };

    /** 把数字或可解析的字符串转换为有限数值，失败返回 null。 */
    const toFiniteNumber = (value) => {
      if (typeof value === 'number' && Number.isFinite(value)) {
        return value;
      }
      if (typeof value === 'string' && value.trim().length > 0) {
        const parsed = Number(value);
        if (Number.isFinite(parsed)) {
          return parsed;
        }
      }
      return null;
    };

    /** 归一化目录字符串：反斜杠统一为正斜杠、去除尾部斜杠（根目录除外），非字符串返回空串。 */
    const normalizeDirectory = (value) => {
      if (typeof value !== 'string') {
        return '';
      }
      const trimmed = value.trim();
      if (!trimmed) {
        return '';
      }
      const normalized = trimmed.replace(/\\/g, '/');
      if (normalized === '/') {
        return '/';
      }
      return normalized.length > 1 ? normalized.replace(/\/+$/, '') : normalized;
    };

    /** 取会话 time.updated，缺失时回退 time.created，再回退 0。 */
    const sessionUpdatedAt = (session) => {
      const time = session && typeof session.time === 'object' ? session.time : null;
      return toFiniteNumber(time?.updated) ?? toFiniteNumber(time?.created) ?? 0;
    };

    /** 按目录过滤会话列表：会话目录等于目标目录或位于其子路径下才保留；目录为空时不过滤。 */
    const filterSessionsByDirectory = (sessions, directory) => {
      const normalizedDirectory = normalizeDirectory(directory);
      if (!normalizedDirectory) {
        return sessions;
      }

      const prefix = normalizedDirectory === '/' ? '/' : `${normalizedDirectory}/`;
      return sessions.filter((session) => {
        const sessionDirectory = normalizeDirectory(session?.directory);
        if (!sessionDirectory) {
          return false;
        }
        return sessionDirectory === normalizedDirectory || sessionDirectory.startsWith(prefix);
      });
    };

    /** 调用 OpenCode GET /session 拉取会话数组（Windows 下把目录中的斜杠转为双反斜杠再编码）；失败或非数组返回空数组。 */
    const listSessions = async (directory) => {
      const query = (() => {
        if (typeof directory !== 'string' || directory.length === 0) {
          return '';
        }
        const preparedDirectory = process.platform === 'win32'
          ? directory.replace(/\//g, '\\\\')
          : directory;
        return `?directory=${encodeURIComponent(preparedDirectory)}`;
      })();

      const response = await fetch(buildOpenCodeUrl(`/session${query}`, ''), {
        method: 'GET',
        headers: {
          Accept: 'application/json',
          ...getOpenCodeAuthHeaders(),
        },
        signal: AbortSignal.timeout(2500),
      });

      if (!response.ok) {
        return [];
      }

      const payload = await response.json().catch(() => null);
      return Array.isArray(payload) ? payload : [];
    };

    try {
      let payload = [];

      if (preferredDirectory) {
        const scopedPayload = await listSessions(preferredDirectory);
        const filteredScopedPayload = filterSessionsByDirectory(scopedPayload, preferredDirectory);

        if (filteredScopedPayload.length > 0) {
          payload = filteredScopedPayload;
        } else {
          const globalPayload = await listSessions(null);
          const filteredGlobalPayload = filterSessionsByDirectory(globalPayload, preferredDirectory);
          payload = filteredGlobalPayload;
        }
      } else {
        payload = await listSessions(null);
      }

      const seen = new Set();
      const rows = [];

      for (const item of payload) {
        if (!item || typeof item !== 'object') {
          continue;
        }

        const id = typeof item.id === 'string' ? item.id.trim().slice(0, 160) : '';
        if (!id || seen.has(id)) {
          continue;
        }

        seen.add(id);
        const title = normalizeShortcutTitle(item.title, `Session ${rows.length + 1}`);
        const updatedAt = sessionUpdatedAt(item);

        rows.push({ id, title, updatedAt });
      }

      rows.sort((a, b) => b.updatedAt - a.updatedAt);

      const shortcuts = rows.slice(0, 3).map((session) => ({
        name: session.title,
        short_name: session.title.length > 32 ? session.title.slice(0, 32) : session.title,
        description: 'Open recent session',
        url: `/?session=${encodeURIComponent(session.id)}`,
        icons: [{ src: '/pwa-192.png', sizes: '192x192', type: 'image/png' }],
      }));

      recentPwaSessionsCache.set(cacheKey, { at: now, data: shortcuts });
      return shortcuts;
    } catch {
      recentPwaSessionsCache.set(cacheKey, { at: now, data: [] });
      return [];
    }
  };

  // manifest 主路由：按「查询参数覆盖 > 服务端设置 > 默认值」的优先级合成
  // 应用名与方向，附加最近会话快捷方式后输出 manifest JSON；
  // Cache-Control: no-store 确保浏览器每次都拉取最新配置。
  app.get('/manifest.webmanifest', async (req, res) => {
    const hasQueryOverride =
      typeof req.query?.pwa_name === 'string'
      || typeof req.query?.app_name === 'string'
      || typeof req.query?.appName === 'string';

    let queryValueRaw = '';
    if (typeof req.query?.pwa_name === 'string') {
      queryValueRaw = req.query.pwa_name;
    } else if (typeof req.query?.app_name === 'string') {
      queryValueRaw = req.query.app_name;
    } else if (typeof req.query?.appName === 'string') {
      queryValueRaw = req.query.appName;
    }

    const queryOverrideName = normalizePwaAppName(queryValueRaw, '');
    const hasOrientationOverride = typeof req.query?.orientation === 'string';
    const queryOverrideOrientation = normalizePwaOrientation(req.query?.orientation, 'system');

    let storedName = '';
    let storedOrientation = 'system';
    try {
      const settings = await readSettingsFromDiskMigrated();
      storedName = normalizePwaAppName(settings?.pwaAppName, '');
      storedOrientation = normalizePwaOrientation(settings?.pwaOrientation, 'system');
    } catch {
      storedName = '';
      storedOrientation = 'system';
    }

    const appName = hasQueryOverride
      ? (queryOverrideName || DEFAULT_PWA_APP_NAME)
      : (storedName || DEFAULT_PWA_APP_NAME);
    const manifestOrientation = mapPwaOrientationToManifest(
      hasOrientationOverride ? queryOverrideOrientation : storedOrientation
    );

    const shortName = appName.length > 30 ? appName.slice(0, 30) : appName;
    const recentSessionShortcuts = await getRecentPwaSessionShortcuts(req);

    const manifest = {
      name: appName,
      short_name: shortName,
      description: 'Web interface for the OMPChamber AI coding assistant',
      id: '/',
      start_url: '/',
      scope: '/',
      display: 'standalone',
      display_override: ['window-controls-overlay'],
      background_color: '#151313',
      theme_color: '#edb449',
      ...(manifestOrientation ? { orientation: manifestOrientation } : {}),
      icons: [
        { src: '/pwa-192.png', sizes: '192x192', type: 'image/png', purpose: 'any' },
        { src: '/pwa-512.png', sizes: '512x512', type: 'image/png', purpose: 'any' },
        { src: '/pwa-maskable-192.png', sizes: '192x192', type: 'image/png', purpose: 'any maskable' },
        { src: '/pwa-maskable-512.png', sizes: '512x512', type: 'image/png', purpose: 'any maskable' },
        { src: '/apple-touch-icon-180x180.png', sizes: '180x180', type: 'image/png', purpose: 'any' },
        { src: '/apple-touch-icon-152x152.png', sizes: '152x152', type: 'image/png', purpose: 'any' },
        { src: '/favicon-32.png', sizes: '32x32', type: 'image/png' },
        { src: '/favicon-16.png', sizes: '16x16', type: 'image/png' },
      ],
      shortcuts: [
        {
          name: 'Appearance Settings',
          short_name: 'Settings',
          description: 'Open appearance settings',
          url: '/?settings=appearance',
          icons: [{ src: '/pwa-192.png', sizes: '192x192', type: 'image/png' }],
        },
        ...recentSessionShortcuts,
      ],
      categories: ['developer', 'tools', 'productivity'],
      lang: 'en',
    };

    res.setHeader('Cache-Control', 'no-store, must-revalidate');
    res.type('application/manifest+json');
    res.send(JSON.stringify(manifest));
  });
};
