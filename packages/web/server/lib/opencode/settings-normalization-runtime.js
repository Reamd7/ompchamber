/**
 * settings.json 规范化运行时工厂模块。
 *
 * 通过依赖注入（os/path/processLike/realpathSync 与 tunnel TTL 边界）
 * 构造一组同步清洗函数，在 settings.json 读写前后规范化用户输入：
 * 目录路径（引号剥离、~ 展开、Windows 盘符大小写与反斜杠统一）、
 * 字符串数组、projects 列表、tunnel TTL、托管远端 tunnel 预设与
 * token、typography 尺寸、模型引用、skill 目录及 skill 相对路径安全性。
 * settings 相关路由统一经由这里的返回值落盘，保证跨会话路径形态一致。
 */
/**
 * 创建 settings 规范化运行时。
 *
 * @param {object} dependencies 注入依赖：os/path 为系统与路径工具
 *   （~ 展开与 Windows 判断）；processLike 提供 platform（win32 判断）；
 *   realpathSync 为符号链接解析函数（传 null 可禁用解析）；
 *   tunnelBootstrapTtl* / tunnelSessionTtl* 为两类 tunnel TTL 的
 *   默认值与最小、最大边界（毫秒）。
 * @returns 一组规范化与清洗函数（见各项注释），除 realpathSync 外
 *   不做任何 IO，便于单元测试。
 */
export const createSettingsNormalizationRuntime = (dependencies) => {
  const {
    os,
    path,
    processLike,
    realpathSync,
    tunnelBootstrapTtlDefaultMs,
    tunnelBootstrapTtlMinMs,
    tunnelBootstrapTtlMaxMs,
    tunnelSessionTtlDefaultMs,
    tunnelSessionTtlMinMs,
    tunnelSessionTtlMaxMs,
  } = dependencies;

  // 规范化目录路径输入：非字符串原样返回；剥离首尾引号（Windows
  // “复制文件地址”或 shell 粘贴会带引号，会破坏后续 fs 校验）与
  // 空白；~ 与 ~/ 前缀展开为 homedir；其余原样返回（不做 resolve）。
  const normalizeDirectoryPath = (value) => {
    if (typeof value !== 'string') {
      return value;
    }

    let trimmed = value.trim();
    // Paths pasted from Windows "Copy as path" (or quoted shell snippets)
    // arrive wrapped in quotes — a literal quote character can never be part
    // of a real path, and it breaks every fs.stat/executable check.
    if (trimmed.length >= 2
      && ((trimmed.startsWith('"') && trimmed.endsWith('"'))
        || (trimmed.startsWith("'") && trimmed.endsWith("'")))) {
      trimmed = trimmed.slice(1, -1).trim();
    }
    if (!trimmed) {
      return trimmed;
    }

    if (trimmed === '~') {
      return os.homedir();
    }

    if (trimmed.startsWith('~/') || trimmed.startsWith('~\\')) {
      return path.join(os.homedir(), trimmed.slice(2));
    }

    return trimmed;
  };

  // Resolve symlinks, falling back to the original value on failure.
  /**
   * 安全解析符号链接：realpathSync 不可用、值非字符串或解析抛错
   * （路径不存在等）时原样返回输入，绝不向上抛异常。
   */
  const safeRealpathSync = (value) => {
    if (!realpathSync || typeof value !== 'string' || !value) {
      return value;
    }
    try {
      return realpathSync(value);
    } catch {
      return value;
    }
  };

  /**
   * 把路径规范化为可持久化到 settings.json 的形态。
   *
   * 先走 normalizeDirectoryPath（引号/空白/~ 展开），Windows 上统一
   * 盘符为大写并在最后把正斜杠换成反斜杠；默认再经 safeRealpathSync
   * 解析真实路径（options.resolveRealpath 为 false 时跳过），realpath
   * 之后需再补一次盘符大写（部分 Windows 环境会返回小写盘符）。
   * 空串原样返回；非 Windows 直接返回解析结果。
   */
  const normalizePathForPersistence = (value, options = {}) => {
    if (typeof value !== 'string') {
      return value;
    }

    const normalized = normalizeDirectoryPath(value);
    if (typeof normalized !== 'string') {
      return normalized;
    }

    const trimmed = normalized.trim();
    if (!trimmed) {
      return trimmed;
    }

    // Normalize Windows drive letter to uppercase to ensure consistent
    // case across all path representations on Windows. NTFS is case-insensitive
    // but case-preserving, so a path like "c:\\Users\\..." and "C:\\Users\\..."
    // would be stored differently in settings.json across sessions.
    // 把 Windows 盘符盘号统一为大写（c 盘与 C 盘在 NTFS 上等价但写盘
    // 形态不同），保证跨会话路径一致（背景见上方英文注释）。
    const uppercaseDriveLetter = (p) =>
      p.replace(/^([a-z]):/, (_, letter) => letter.toUpperCase() + ':');

    const isWindows = processLike.platform === 'win32';
    const caseNormalized = isWindows ? uppercaseDriveLetter(trimmed) : trimmed;
    const resolved = options.resolveRealpath === false ? caseNormalized : safeRealpathSync(caseNormalized);

    // Re-normalize after realpath — safeRealpathSync may return a
    // lowercase drive letter on some Windows environments.
    const finalResolved = isWindows && typeof resolved === 'string'
      ? uppercaseDriveLetter(resolved)
      : resolved;

    if (!isWindows) {
      return finalResolved;
    }

    return finalResolved.replace(/\//g, '\\');
  };

  /**
   * 逐元素比较两个字符串数组是否完全相等：任一不是数组、长度不同
   * 或任一元素不同即返回 false。用于判断规范化是否真正改变了字段值，
   * 避免无谓写盘。
   */
  const areStringArraysEqual = (a, b) => {
    if (!Array.isArray(a) || !Array.isArray(b)) {
      return false;
    }
    if (a.length !== b.length) {
      return false;
    }
    for (let i = 0; i < a.length; i += 1) {
      if (a[i] !== b[i]) {
        return false;
      }
    }
    return true;
  };

  /**
   * 把输入清洗为去重后的非空字符串数组：非数组输入返回 []；
   * 过滤非字符串与空串后用 Set 去重，保持首次出现顺序。
   */
  const normalizeStringArray = (input) => {
    if (!Array.isArray(input)) {
      return [];
    }
    return Array.from(
      new Set(
        input.filter((entry) => typeof entry === 'string' && entry.length > 0)
      )
    );
  };

  // 清洗 settings.projects 数组：非数组返回 undefined（字段视为缺省）。
  // 逐项 trim 字符串字段；path 先 resolve + realpath 再做持久化规范化；
  // 缺 id 或规范化 path、重复 id、重复 path 的条目直接丢弃；
  // defaultModel 必须含 “/”（provider/model 形态）才保留，defaultVariant
  // 仅在 defaultModel 有效时才有意义（见行内注释）；iconImage 只保留
  // 合法的 { mime, updatedAt>0, source: custom|auto }，显式 null 原样
  // 保留以支持“清除图标”语义；iconBackground 仅接受 hex 颜色，同样
  // 保留显式 null；addedAt/lastOpenedAt 仅保留非负有限数。返回新数组。
  const sanitizeProjects = (input) => {
    if (!Array.isArray(input)) {
      return undefined;
    }

    const hexColorPattern = /^#(?:[\da-fA-F]{3}|[\da-fA-F]{6})$/;
    // 校验 iconBackground：仅接受 #RGB/#RRGGBB hex 颜色并转小写，
    // 非字符串或非法值返回 null。
    const normalizeIconBackground = (value) => {
      if (typeof value !== 'string') {
        return null;
      }
      const trimmed = value.trim();
      if (!trimmed) {
        return null;
      }
      return hexColorPattern.test(trimmed) ? trimmed.toLowerCase() : null;
    };

    const result = [];
    const seenIds = new Set();
    const seenPaths = new Set();

    for (const entry of input) {
      if (!entry || typeof entry !== 'object') continue;

      const candidate = entry;
      const id = typeof candidate.id === 'string' ? candidate.id.trim() : '';
      const rawPath = typeof candidate.path === 'string' ? candidate.path.trim() : '';
      const resolvedPath = rawPath ? safeRealpathSync(path.resolve(normalizeDirectoryPath(rawPath))) : '';
      const normalizedPath = resolvedPath ? normalizePathForPersistence(resolvedPath, { resolveRealpath: false }) : '';
      const label = typeof candidate.label === 'string' ? candidate.label.trim() : '';
      const icon = typeof candidate.icon === 'string' ? candidate.icon.trim() : '';
      const iconImage = candidate.iconImage && typeof candidate.iconImage === 'object'
        ? candidate.iconImage
        : null;
      const iconBackground = normalizeIconBackground(candidate.iconBackground);
      const color = typeof candidate.color === 'string' ? candidate.color.trim() : '';
      const defaultModel = typeof candidate.defaultModel === 'string' ? candidate.defaultModel.trim() : '';
      const defaultVariant = typeof candidate.defaultVariant === 'string' ? candidate.defaultVariant.trim() : '';
      const addedAt = Number.isFinite(candidate.addedAt) ? Number(candidate.addedAt) : null;
      const lastOpenedAt = Number.isFinite(candidate.lastOpenedAt)
        ? Number(candidate.lastOpenedAt)
        : null;

      if (!id || !normalizedPath) continue;
      if (seenIds.has(id)) continue;
      if (seenPaths.has(normalizedPath)) continue;

      seenIds.add(id);
      seenPaths.add(normalizedPath);

      const project = {
        id,
        path: normalizedPath,
        ...(label ? { label } : {}),
        ...(icon ? { icon } : {}),
        ...(iconBackground ? { iconBackground } : {}),
        ...(color ? { color } : {}),
        ...(defaultModel && defaultModel.includes('/') ? { defaultModel } : {}),
        // A variant is meaningless without the model it belongs to.
        ...(defaultModel && defaultModel.includes('/') && defaultVariant ? { defaultVariant } : {}),
        ...(Number.isFinite(addedAt) && addedAt >= 0 ? { addedAt } : {}),
        ...(Number.isFinite(lastOpenedAt) && lastOpenedAt >= 0 ? { lastOpenedAt } : {}),
      };

      if (candidate.iconImage === null) {
        project.iconImage = null;
      } else if (iconImage) {
        const mime = typeof iconImage.mime === 'string' ? iconImage.mime.trim() : '';
        const updatedAt = typeof iconImage.updatedAt === 'number' && Number.isFinite(iconImage.updatedAt)
          ? Math.max(0, Math.round(iconImage.updatedAt))
          : 0;
        const source = iconImage.source === 'custom' || iconImage.source === 'auto'
          ? iconImage.source
          : null;
        if (mime && updatedAt > 0 && source) {
          project.iconImage = { mime, updatedAt, source };
        }
      }

      if (candidate.iconBackground === null) {
        project.iconBackground = null;
      }

      if (typeof candidate.sidebarCollapsed === 'boolean') {
        project.sidebarCollapsed = candidate.sidebarCollapsed;
      }

      result.push(project);
    }

    return result;
  };

  // 规范化 settings 中的路径字段：lastDirectory/homeDirectory 为单值，
  // pinnedDirectories 为数组（逐项规范化 + 去重），projects 走
  // sanitizeProjects。仅当规范化结果与原值不同才浅拷贝对象并置
  // changed；返回 { settings, changed }，未变化时 settings 仍是原对象。
  const normalizeSettingsPaths = (input) => {
    const settings = input && typeof input === 'object' ? input : {};
    let next = settings;
    let changed = false;

    // 惰性浅拷贝 settings：只在确实有字段变化时才产生新对象。
    const ensureNext = () => {
      if (next === settings) {
        next = { ...settings };
      }
    };

    // 规范化单个字符串路径字段，变化时写入 next 并标记 changed。
    const normalizePathField = (key) => {
      if (typeof settings[key] !== 'string' || settings[key].length === 0) {
        return;
      }
      const normalized = normalizePathForPersistence(settings[key]);
      if (normalized !== settings[key]) {
        ensureNext();
        next[key] = normalized;
        changed = true;
      }
    };

    // 规范化字符串数组字段（逐项规范化后去重），变化时写入 next
    // 并标记 changed。
    const normalizePathArrayField = (key) => {
      if (!Array.isArray(settings[key])) {
        return;
      }

      const normalized = normalizeStringArray(
        settings[key]
          .map((entry) => (typeof entry === 'string' ? normalizePathForPersistence(entry) : entry))
          .filter((entry) => typeof entry === 'string' && entry.length > 0)
      );

      if (!areStringArraysEqual(normalized, settings[key])) {
        ensureNext();
        next[key] = normalized;
        changed = true;
      }
    };

    normalizePathField('lastDirectory');
    normalizePathField('homeDirectory');
    normalizePathArrayField('pinnedDirectories');

    if (Array.isArray(settings.projects)) {
      const normalizedProjects = sanitizeProjects(settings.projects) || [];
      if (JSON.stringify(normalizedProjects) !== JSON.stringify(settings.projects)) {
        ensureNext();
        next.projects = normalizedProjects;
        changed = true;
      }
    }

    return { settings: next, changed };
  };

  // 把 value 收敛到 [min, max] 闭区间。
  const clampNumber = (value, min, max) => Math.max(min, Math.min(max, value));

  // 规范化 tunnel bootstrap TTL：null 显式透传（表示禁用/缺省），
  // 非有限数回退默认值，否则四舍五入后夹到 [min, max]。
  const normalizeTunnelBootstrapTtlMs = (value) => {
    if (value === null) {
      return null;
    }
    if (!Number.isFinite(value)) {
      return tunnelBootstrapTtlDefaultMs;
    }
    return clampNumber(Math.round(value), tunnelBootstrapTtlMinMs, tunnelBootstrapTtlMaxMs);
  };

  // 规范化 tunnel session TTL：非有限数回退默认值，否则四舍五入后
  // 夹到 [min, max]（与 bootstrap 不同，null 不透传）。
  const normalizeTunnelSessionTtlMs = (value) => {
    if (!Number.isFinite(value)) {
      return tunnelSessionTtlDefaultMs;
    }
    return clampNumber(Math.round(value), tunnelSessionTtlMinMs, tunnelSessionTtlMaxMs);
  };

  // 从任意用户输入（裸域名、带端口、完整 URL）提取规范 hostname：
  // 非字符串或空串返回 undefined；无 scheme 时按 https:// 补齐再交给
  // URL 解析，解析失败或取不到 hostname 返回 undefined；成功返回
  // 小写 trim 后的 hostname（端口、路径、协议全部丢弃）。
  const normalizeManagedRemoteTunnelHostname = (value) => {
    if (typeof value !== 'string') {
      return undefined;
    }
    const trimmed = value.trim();
    if (!trimmed) {
      return undefined;
    }

    // 宽容解析：无 :// 前缀时补 https:// 再用 URL 提取 hostname，
    // 任何解析异常返回 null。
    const parsed = (() => {
      try {
        if (trimmed.includes('://')) {
          return new URL(trimmed);
        }
        return new URL(`https://${trimmed}`);
      } catch {
        return null;
      }
    })();

    const hostname = parsed?.hostname?.trim().toLowerCase() || '';
    if (!hostname) {
      return undefined;
    }
    return hostname;
  };

  // 清洗托管远端 tunnel 预设数组：非数组返回 undefined；每项要求
  // id/name 非空且 hostname 可规范化，按 id 与 hostname 双向去重；
  // 返回仅含 { id, name, hostname } 的新数组。
  const normalizeManagedRemoteTunnelPresets = (value) => {
    if (!Array.isArray(value)) {
      return undefined;
    }

    const result = [];
    const seenIds = new Set();
    const seenHostnames = new Set();

    for (const entry of value) {
      if (!entry || typeof entry !== 'object') continue;
      const candidate = entry;
      const id = typeof candidate.id === 'string' ? candidate.id.trim() : '';
      const name = typeof candidate.name === 'string' ? candidate.name.trim() : '';
      const hostname = normalizeManagedRemoteTunnelHostname(candidate.hostname);
      if (!id || !name || !hostname) continue;
      if (seenIds.has(id) || seenHostnames.has(hostname)) continue;
      seenIds.add(id);
      seenHostnames.add(hostname);
      result.push({ id, name, hostname });
    }

    return result;
  };

  // 清洗预设 token 映射：非纯对象（含数组/null）返回 undefined；
  // 键值 trim 后均非空才保留；结果为空时返回 undefined（无 token
  // 需要持久化）。
  const normalizeManagedRemoteTunnelPresetTokens = (value) => {
    if (!value || typeof value !== 'object' || Array.isArray(value)) {
      return undefined;
    }

    const result = {};
    for (const [rawId, rawToken] of Object.entries(value)) {
      const id = typeof rawId === 'string' ? rawId.trim() : '';
      const token = typeof rawToken === 'string' ? rawToken.trim() : '';
      if (!id || !token) {
        continue;
      }
      result[id] = token;
    }

    return Object.keys(result).length > 0 ? result : undefined;
  };

  // 判断 skill 相对路径是否不安全：非字符串或空串视为不安全；把
  // 反斜杠统一为正斜杠后，绝对路径或含 “..” 段视为不安全（防目录
  // 穿越）。返回 true 表示不安全。
  const isUnsafeSkillRelativePath = (value) => {
    if (typeof value !== 'string' || value.length === 0) {
      return true;
    }

    const normalized = value.replace(/\\/g, '/');
    if (path.posix.isAbsolute(normalized)) {
      return true;
    }

    return normalized.split('/').some((segment) => segment === '..');
  };

  // 部分（partial）清洗 typography 尺寸配置：输入为对象时从六个固定
  // 键中拷贝非空字符串；一个有效键都没有则返回 undefined，表示该
  // 字段应整体视为缺省。
  const sanitizeTypographySizesPartial = (input) => {
    if (!input || typeof input !== 'object') {
      return undefined;
    }
    const candidate = input;
    const result = {};
    let populated = false;

    // 拷贝单个尺寸键的非空字符串值并标记已有有效内容。
    const assign = (key) => {
      if (typeof candidate[key] === 'string' && candidate[key].length > 0) {
        result[key] = candidate[key];
        populated = true;
      }
    };

    assign('markdown');
    assign('code');
    assign('uiHeader');
    assign('uiLabel');
    assign('meta');
    assign('micro');

    return populated ? result : undefined;
  };

  // 清洗模型引用数组（如最近使用模型）：非数组返回 undefined；每项
  // 要求 providerID/modelID 非空，按 “providerID/modelID” 去重，最多
  // 保留 limit 条。
  const sanitizeModelRefs = (input, limit) => {
    if (!Array.isArray(input)) {
      return undefined;
    }

    const result = [];
    const seen = new Set();

    for (const entry of input) {
      if (!entry || typeof entry !== 'object') continue;
      const providerID = typeof entry.providerID === 'string' ? entry.providerID.trim() : '';
      const modelID = typeof entry.modelID === 'string' ? entry.modelID.trim() : '';
      if (!providerID || !modelID) continue;
      const key = `${providerID}/${modelID}`;
      if (seen.has(key)) continue;
      seen.add(key);
      result.push({ providerID, modelID });
      if (result.length >= limit) break;
    }

    return result;
  };

  // 清洗 skill 目录数组：非数组返回 undefined；每项要求
  // id/label/source 非空，按 id 去重；subpath/gitIdentityId 为可选
  // 非空字符串；返回仅含有效字段的精简对象数组。
  const sanitizeSkillCatalogs = (input) => {
    if (!Array.isArray(input)) {
      return undefined;
    }

    const result = [];
    const seen = new Set();

    for (const entry of input) {
      if (!entry || typeof entry !== 'object') continue;

      const id = typeof entry.id === 'string' ? entry.id.trim() : '';
      const label = typeof entry.label === 'string' ? entry.label.trim() : '';
      const source = typeof entry.source === 'string' ? entry.source.trim() : '';
      const subpath = typeof entry.subpath === 'string' ? entry.subpath.trim() : '';
      const gitIdentityId = typeof entry.gitIdentityId === 'string' ? entry.gitIdentityId.trim() : '';

      if (!id || !label || !source) continue;
      if (seen.has(id)) continue;
      seen.add(id);

      result.push({
        id,
        label,
        source,
        ...(subpath ? { subpath } : {}),
        ...(gitIdentityId ? { gitIdentityId } : {}),
      });
    }

    return result;
  };

  // 导出规范化 API：供 settings 读写路由复用，全部为同步函数。
  return {
    normalizeDirectoryPath,
    normalizePathForPersistence,
    normalizeSettingsPaths,
    normalizeTunnelBootstrapTtlMs,
    normalizeTunnelSessionTtlMs,
    normalizeManagedRemoteTunnelHostname,
    normalizeManagedRemoteTunnelPresets,
    normalizeManagedRemoteTunnelPresetTokens,
    isUnsafeSkillRelativePath,
    sanitizeTypographySizesPartial,
    normalizeStringArray,
    sanitizeModelRefs,
    sanitizeSkillCatalogs,
    sanitizeProjects,
  };
};
