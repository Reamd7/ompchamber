/**
 * 自定义主题运行时：从 themesDir 读取用户放置的主题 JSON 文件，逐个做
 * 结构校验（metadata 与 colors 的必填字段）并规范化，产出可直接下发给
 * 前端主题选择器与 CSS 变量生成器的主题对象。通过依赖注入 fsPromises、
 * path、themesDir、maxThemeJsonBytes 与 logger，便于单元测试替换实现。
 */

/**
 * 创建主题运行时实例。
 *
 * @param {object} dependencies 注入依赖
 * @param {object} dependencies.fsPromises fs.promises 兼容对象
 * @param {object} dependencies.path node:path 兼容对象
 * @param {string} dependencies.themesDir 自定义主题所在目录
 * @param {number} dependencies.maxThemeJsonBytes 单个主题 JSON 的大小上限（超出则跳过该文件）
 * @param {object} dependencies.logger 日志对象（需提供 warn）
 * @returns {{ normalizeThemeJson: Function, readCustomThemesFromDisk: Function }}
 */
export const createThemeRuntime = (dependencies) => {
  const {
    fsPromises,
    path,
    themesDir,
    maxThemeJsonBytes,
    logger,
  } = dependencies;

  /** 判断值是否为去掉首尾空白后仍非空的字符串。 */
  const isNonEmptyString = (value) => typeof value === 'string' && value.trim().length > 0;
  /** 主题色值的合法性检查：当前仅要求是非空字符串，不校验具体颜色格式。 */
  const isValidThemeColor = (value) => isNonEmptyString(value);

  /**
   * 校验并规范化一份主题 JSON。
   *
   * 逐层检查 metadata（id / name / variant 必填，variant 只允许 light 或
   * dark）与 colors（primary / surface / interactive / status / syntax 的
   * 全部必填色字段，见下方 required 列表），任一字段缺失或为空字符串即视为
   * 非法主题。合法时补齐默认 description、version，过滤空 tags，返回保留
   * 原始其余字段的规范化新对象；非法返回 null，由调用方决定跳过或告警。
   *
   * @param {object} raw JSON.parse 后的原始主题对象
   * @returns {object|null} 规范化后的主题对象；不合法时为 null
   */
  const normalizeThemeJson = (raw) => {
    if (!raw || typeof raw !== 'object') {
      return null;
    }

    const metadata = raw.metadata && typeof raw.metadata === 'object' ? raw.metadata : null;
    const colors = raw.colors && typeof raw.colors === 'object' ? raw.colors : null;
    if (!metadata || !colors) {
      return null;
    }

    const id = metadata.id;
    const name = metadata.name;
    const variant = metadata.variant;
    if (!isNonEmptyString(id) || !isNonEmptyString(name) || (variant !== 'light' && variant !== 'dark')) {
      return null;
    }

    const primary = colors.primary;
    const surface = colors.surface;
    const interactive = colors.interactive;
    const status = colors.status;
    const syntax = colors.syntax;
    const syntaxBase = syntax && typeof syntax === 'object' ? syntax.base : null;
    const syntaxHighlights = syntax && typeof syntax === 'object' ? syntax.highlights : null;

    if (!primary || !surface || !interactive || !status || !syntaxBase || !syntaxHighlights) {
      return null;
    }

    // Minimal fields required by CSSVariableGenerator and diff/syntax rendering.
    const required = [
      primary.base,
      primary.foreground,
      surface.background,
      surface.foreground,
      surface.muted,
      surface.mutedForeground,
      surface.elevated,
      surface.elevatedForeground,
      surface.subtle,
      interactive.border,
      interactive.selection,
      interactive.selectionForeground,
      interactive.focusRing,
      interactive.hover,
      status.error,
      status.errorForeground,
      status.errorBackground,
      status.errorBorder,
      status.warning,
      status.warningForeground,
      status.warningBackground,
      status.warningBorder,
      status.success,
      status.successForeground,
      status.successBackground,
      status.successBorder,
      status.info,
      status.infoForeground,
      status.infoBackground,
      status.infoBorder,
      syntaxBase.background,
      syntaxBase.foreground,
      syntaxBase.keyword,
      syntaxBase.string,
      syntaxBase.number,
      syntaxBase.function,
      syntaxBase.variable,
      syntaxBase.type,
      syntaxBase.comment,
      syntaxBase.operator,
      syntaxHighlights.diffAdded,
      syntaxHighlights.diffRemoved,
      syntaxHighlights.lineNumber,
    ];

    if (!required.every(isValidThemeColor)) {
      return null;
    }

    const tags = Array.isArray(metadata.tags)
      ? metadata.tags.filter((tag) => typeof tag === 'string' && tag.trim().length > 0)
      : [];

    return {
      ...raw,
      metadata: {
        ...metadata,
        id: id.trim(),
        name: name.trim(),
        description: typeof metadata.description === 'string' ? metadata.description : '',
        version: typeof metadata.version === 'string' && metadata.version.trim().length > 0 ? metadata.version : '1.0.0',
        variant,
        tags,
      },
    };
  };

  /**
   * 扫描主题目录并读取所有合法的自定义主题。
   *
   * 只处理普通文件或符号链接、且以 .json 结尾的目录项；逐个 stat 检查
   * 大小上限、解析 JSON、经 normalizeThemeJson 校验，并按 metadata.id
   * 去重（后出现的重复 id 跳过）。单个文件失败只 warn 不中断整体扫描；
   * 目录不存在（ENOENT）或其它目录级错误同样返回空数组。
   *
   * @returns {Promise<object[]>} 规范化后的主题列表（顺序与目录项一致）
   */
  const readCustomThemesFromDisk = async () => {
    try {
      const entries = await fsPromises.readdir(themesDir, { withFileTypes: true });
      const themes = [];
      const seen = new Set();

      for (const entry of entries) {
        if (!entry.isFile() && !entry.isSymbolicLink()) continue;
        if (!entry.name.toLowerCase().endsWith('.json')) continue;

        const filePath = path.join(themesDir, entry.name);
        try {
          const stat = await fsPromises.stat(filePath);
          if (!stat.isFile()) continue;
          if (stat.size > maxThemeJsonBytes) {
            logger.warn(`[themes] Skip ${entry.name}: too large (${stat.size} bytes)`);
            continue;
          }

          const rawText = await fsPromises.readFile(filePath, 'utf8');
          const parsed = JSON.parse(rawText);
          const normalized = normalizeThemeJson(parsed);
          if (!normalized) {
            logger.warn(`[themes] Skip ${entry.name}: invalid theme JSON`);
            continue;
          }

          const id = normalized.metadata.id;
          if (seen.has(id)) {
            logger.warn(`[themes] Skip ${entry.name}: duplicate theme id "${id}"`);
            continue;
          }

          seen.add(id);
          themes.push(normalized);
        } catch (error) {
          logger.warn(`[themes] Failed to read ${entry.name}:`, error);
        }
      }

      return themes;
    } catch (error) {
      // Missing dir is fine.
      if (error && typeof error === 'object' && error.code === 'ENOENT') {
        return [];
      }
      logger.warn('[themes] Failed to list custom themes dir:', error);
      return [];
    }
  };

  return {
    normalizeThemeJson,
    readCustomThemesFromDisk,
  };
};
