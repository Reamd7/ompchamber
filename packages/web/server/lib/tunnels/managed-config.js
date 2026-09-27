/**
 * Cloudflare 托管远端隧道配置（token 预设库）的磁盘持久化模块。
 *
 * 通过工厂函数注入 fs/path 等依赖，负责读写 CLOUDFLARE_MANAGED_REMOTE_TUNNELS_FILE_PATH
 * 指向的 JSON 配置文件：条目清洗（trim、去重、补 updatedAt）、旧版 named-tunnels
 * 文件的一次性迁移、以 Promise 链串行化的“读改写”更新（避免并发写互相覆盖）、
 * 与 UI 预设列表的对齐同步，以及 token 预设的 upsert 与按 id/hostname 查询。
 */

/**
 * 创建托管远端隧道配置运行时。
 * @param {object} deps 注入依赖：fsPromises、path、hostname/预设归一化函数，
 *   以及 constants（新配置文件路径、旧版遗留文件路径、配置结构版本号）。
 * @returns {{readManagedRemoteTunnelConfigFromDisk: Function, syncManagedRemoteTunnelConfigWithPresets: Function, upsertManagedRemoteTunnelToken: Function, resolveManagedRemoteTunnelToken: Function}}
 */
export const createManagedTunnelConfigRuntime = (deps) => {
  const {
    fsPromises,
    path,
    normalizeManagedRemoteTunnelHostname,
    normalizeManagedRemoteTunnelPresets,
    constants,
  } = deps;

  const {
    CLOUDFLARE_MANAGED_REMOTE_TUNNELS_FILE_PATH,
    CLOUDFLARE_LEGACY_NAMED_TUNNELS_FILE_PATH,
    CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION,
  } = constants;

  /** 配置写入串行锁：以 Promise 链保证同一时刻只有一次读改写落盘，防止并发覆盖。 */
  let persistManagedRemoteTunnelConfigLock = Promise.resolve();

  /**
   * 清洗配置条目数组：跳过非对象、任一字段（id/name/hostname/token）缺失或空白、
   * 以及 id 或 hostname 重复的条目；统一 trim 并为缺失 updatedAt 的条目补当前时间戳。
   * 磁盘内容可能被外部修改或来自旧格式，因此输入按不可信数据处理。
   * @param {*} value 原始 tunnels 数组
   * @returns {Array<{id: string, name: string, hostname: string, token: string, updatedAt: number}>}
   */
  const sanitizeManagedRemoteTunnelConfigEntries = (value) => {
    if (!Array.isArray(value)) {
      return [];
    }

    const result = [];
    const seenIds = new Set();
    const seenHostnames = new Set();
    for (const entry of value) {
      if (!entry || typeof entry !== 'object') {
        continue;
      }

      const id = typeof entry.id === 'string' ? entry.id.trim() : '';
      const name = typeof entry.name === 'string' ? entry.name.trim() : '';
      const hostname = normalizeManagedRemoteTunnelHostname(entry.hostname);
      const token = typeof entry.token === 'string' ? entry.token.trim() : '';
      const updatedAt = Number.isFinite(entry.updatedAt) ? entry.updatedAt : Date.now();

      if (!id || !name || !hostname || !token) {
        continue;
      }
      if (seenIds.has(id) || seenHostnames.has(hostname)) {
        continue;
      }

      seenIds.add(id);
      seenHostnames.add(hostname);
      result.push({ id, name, hostname, token, updatedAt });
    }

    return result;
  };

  /**
   * 将配置以两空格缩进的 JSON 写入磁盘：先递归创建父目录，
   * 并以 0o600 权限保存（文件含隧道 token，需限制为属主可读写）。
   * @param {object} data 完整配置对象（version + tunnels）
   */
  const writeManagedRemoteTunnelConfigToDisk = async (data) => {
    await fsPromises.mkdir(path.dirname(CLOUDFLARE_MANAGED_REMOTE_TUNNELS_FILE_PATH), { recursive: true });
    await fsPromises.writeFile(CLOUDFLARE_MANAGED_REMOTE_TUNNELS_FILE_PATH, JSON.stringify(data, null, 2), { encoding: 'utf8', mode: 0o600 });
  };

  /**
   * 从旧版 named-tunnels 配置文件迁移：读取、清洗后写入新文件并返回迁移结果。
   * 旧文件不存在（ENOENT）属正常情况，返回空配置；其余读取/解析失败仅告警并
   * 返回空配置，不向调用方抛错。
   * @returns {Promise<{version: number, tunnels: Array}>}
   */
  const migrateManagedRemoteTunnelConfigFromLegacyFile = async () => {
    try {
      const legacyRaw = await fsPromises.readFile(CLOUDFLARE_LEGACY_NAMED_TUNNELS_FILE_PATH, 'utf8');
      const parsed = JSON.parse(legacyRaw);
      const tunnels = sanitizeManagedRemoteTunnelConfigEntries(parsed?.tunnels);
      const migrated = {
        version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION,
        tunnels,
      };
      await writeManagedRemoteTunnelConfigToDisk(migrated);
      return migrated;
    } catch (error) {
      if (error && typeof error === 'object' && error.code === 'ENOENT') {
        return { version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION, tunnels: [] };
      }
      console.warn('Failed to migrate legacy named tunnel config file:', error);
      return { version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION, tunnels: [] };
    }
  };

  /**
   * 读取托管远端隧道配置，返回结构固定为 { version: 当前版本, tunnels: 清洗后条目 }。
   * 新文件不存在（ENOENT）时触发旧文件迁移；其余读取/解析失败同样仅告警并返回空配置。
   * @returns {Promise<{version: number, tunnels: Array}>}
   */
  const readManagedRemoteTunnelConfigFromDisk = async () => {
    try {
      const raw = await fsPromises.readFile(CLOUDFLARE_MANAGED_REMOTE_TUNNELS_FILE_PATH, 'utf8');
      const parsed = JSON.parse(raw);
      if (!parsed || typeof parsed !== 'object') {
        return { version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION, tunnels: [] };
      }

      return {
        version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION,
        tunnels: sanitizeManagedRemoteTunnelConfigEntries(parsed.tunnels),
      };
    } catch (error) {
      if (error && typeof error === 'object' && error.code === 'ENOENT') {
        return migrateManagedRemoteTunnelConfigFromLegacyFile();
      }
      console.warn('Failed to read managed remote tunnel config file:', error);
      return { version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION, tunnels: [] };
    }
  };

  /**
   * 在串行锁保护下执行“读 → mutate → 写”的配置更新：
   * mutate 收到清洗后的当前配置，其返回值（可空）会再次清洗后落盘。
   * @param {Function} mutate 配置变换函数
   * @returns {Promise<void>} resolve 即表示本次写入已完成
   */
  const updateManagedRemoteTunnelConfig = async (mutate) => {
    persistManagedRemoteTunnelConfigLock = persistManagedRemoteTunnelConfigLock.then(async () => {
      const current = await readManagedRemoteTunnelConfigFromDisk();
      const next = mutate({
        version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION,
        tunnels: sanitizeManagedRemoteTunnelConfigEntries(current.tunnels),
      });

      await writeManagedRemoteTunnelConfigToDisk({
        version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION,
        tunnels: sanitizeManagedRemoteTunnelConfigEntries(next?.tunnels),
      });
    });

    return persistManagedRemoteTunnelConfigLock;
  };

  /**
   * 将配置条目与 UI 侧预设列表对齐：仅保留能按 id 或 hostname 匹配到既有条目的预设
   * （沿用其 token 与 updatedAt，但覆盖 id/name/hostname），并按预设顺序重排；
   * 配置中未被任何预设覆盖的条目会被移除，从而与 UI 展示保持一致。
   * @param {Array} presets UI 侧预设列表（先经 normalizeManagedRemoteTunnelPresets 清洗）
   */
  const syncManagedRemoteTunnelConfigWithPresets = async (presets) => {
    const sanitizedPresets = normalizeManagedRemoteTunnelPresets(presets) || [];

    await updateManagedRemoteTunnelConfig((current) => {
      const byId = new Map(current.tunnels.map((entry) => [entry.id, entry]));
      const byHostname = new Map(current.tunnels.map((entry) => [entry.hostname, entry]));

      const nextTunnels = [];
      for (const preset of sanitizedPresets) {
        const existing = byId.get(preset.id) || byHostname.get(preset.hostname) || null;
        if (!existing) {
          continue;
        }

        nextTunnels.push({
          ...existing,
          id: preset.id,
          name: preset.name,
          hostname: preset.hostname,
        });
      }

      return {
        version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION,
        tunnels: nextTunnels,
      };
    });
  };

  /**
   * 新增或更新一个 token 预设：任一字段类型不对或清洗后为空则静默跳过（不写盘）。
   * 写入前剔除同 id 或同 hostname 的旧条目以避免冲突，updatedAt 刷新为当前时间。
   * @param {object} entry 预设字段（id/name/hostname/token）
   */
  const upsertManagedRemoteTunnelToken = async ({ id, name, hostname, token }) => {
    if (typeof id !== 'string' || typeof name !== 'string' || typeof hostname !== 'string' || typeof token !== 'string') {
      return;
    }
    const normalizedId = id.trim();
    const normalizedName = name.trim();
    const normalizedHostname = normalizeManagedRemoteTunnelHostname(hostname);
    const normalizedToken = token.trim();
    if (!normalizedId || !normalizedName || !normalizedHostname || !normalizedToken) {
      return;
    }

    await updateManagedRemoteTunnelConfig((current) => {
      const withoutConflicts = current.tunnels.filter((entry) => entry.id !== normalizedId && entry.hostname !== normalizedHostname);
      withoutConflicts.push({
        id: normalizedId,
        name: normalizedName,
        hostname: normalizedHostname,
        token: normalizedToken,
        updatedAt: Date.now(),
      });

      return {
        version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION,
        tunnels: withoutConflicts,
      };
    });
  };

  /**
   * 从配置中解析 token：优先按 presetId 精确匹配，其次按 hostname 匹配；
   * 两者均未命中时返回空串（调用方以此判断“无可用 token”）。
   * @param {object} query presetId 与 hostname（均可选）
   * @returns {Promise<string>}
   */
  const resolveManagedRemoteTunnelToken = async ({ presetId, hostname }) => {
    const normalizedPresetId = typeof presetId === 'string' ? presetId.trim() : '';
    const normalizedHostname = normalizeManagedRemoteTunnelHostname(hostname);
    const config = await readManagedRemoteTunnelConfigFromDisk();

    if (normalizedPresetId) {
      const byId = config.tunnels.find((entry) => entry.id === normalizedPresetId);
      if (byId?.token) {
        return byId.token;
      }
    }

    if (normalizedHostname) {
      const byHostname = config.tunnels.find((entry) => entry.hostname === normalizedHostname);
      if (byHostname?.token) {
        return byHostname.token;
      }
    }

    return '';
  };

  // 对外暴露配置读取、预设同步与 token 增改查四个入口。
  return {
    readManagedRemoteTunnelConfigFromDisk,
    syncManagedRemoteTunnelConfigWithPresets,
    upsertManagedRemoteTunnelToken,
    resolveManagedRemoteTunnelToken,
  };
};
