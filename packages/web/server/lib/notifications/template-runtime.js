/**
 * 通知模板运行时：渲染通知标题/正文模板并组装所需变量。
 *
 * 职责：把用户可配置的通知模板（{project_name}、{branch} 等占位符）渲染成实际文案；
 * 从事件 payload 或 OpenCode API 提取最后一条助手消息文本；维护会话标题/信息缓存；
 * 通过共享 summarization 模块做通知摘要；以及 zen 摘要模型的选择
 * （相关 provider 已下线，现仅保留兼容的空实现）。
 */
import { summarizeText as summarizeSharedText } from '../text/summarization.js';

/**
 * 创建通知模板运行时。
 * deps 注入 readSettingsFromDisk（读取项目等设置）、buildOpenCodeUrl 与
 * getOpenCodeAuthHeaders（访问 OpenCode API）、resolveGitBinaryForSpawn（git 分支查询）。
 */
export const createNotificationTemplateRuntime = (deps) => {
  const {
    readSettingsFromDisk,
    buildOpenCodeUrl,
    getOpenCodeAuthHeaders,
    resolveGitBinaryForSpawn,
  } = deps;

  /** 通知正文的最大字符数，超出截断。 */
  const NOTIFICATION_BODY_MAX_CHARS = 1000;
  /** 会话信息缓存的存活时间（60 秒）。 */
  const SESSION_INFO_CACHE_TTL_MS = 60 * 1000;

  /** 可选 zen（快速摘要）模型列表缓存；provider 下线后恒为空数组，仅为接口兼容保留。 */
  const cachedZenModels = { models: [] };

  /** 会话 ID -> 标题 的内存缓存（由事件顺带写入，避免重复拉取）。 */
  const sessionTitleCache = new Map();
  /** 会话 ID -> { data, at } 的会话信息缓存，带 TTL。 */
  const sessionInfoCache = new Map();

  /**
   * 创建可清理的超时 AbortSignal：timeoutMs 后中止。
   * 返回 { signal, cleanup }，调用方须在结束后调用 cleanup() 取消定时器。
   */
  const createTimeoutSignal = (timeoutMs) => {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), timeoutMs);
    return {
      signal: controller.signal,
      cleanup: () => clearTimeout(timer),
    };
  };

  /** 规整项目标签：仅去首尾空白；文件夹名按磁盘原样展示，不做 title-case。 */
  const formatProjectLabel = (label) => {
    if (!label || typeof label !== 'string') return '';
    // Folder names are shown exactly as they are on disk — no title-casing.
    return label.trim();
  };

  /**
   * 渲染通知模板：把 `{word}` 占位符替换为 variables 中的同名值。
   * 变量缺失或为 null 时替换为空串；模板非字符串返回空串。
   */
  const resolveNotificationTemplate = (template, variables) => {
    if (!template || typeof template !== 'string') return '';
    return template.replace(/\{(\w+)\}/g, (_match, key) => {
      const value = variables[key];
      if (value === undefined || value === null) return '';
      return String(value);
    });
  };

  /**
   * 判断渲染结果是否应作为通知消息使用。
   * resolved 为空一律不采用；模板含 {last_message} 时，仅当变量 last_message
   * 为非空字符串才采用（避免把"无新消息"渲染成空通知）；其余情况采用。
   */
  const shouldApplyResolvedTemplateMessage = (template, resolved, variables) => {
    if (!resolved) {
      return false;
    }

    if (typeof template !== 'string') {
      return true;
    }

    if (template.includes('{last_message}')) {
      return typeof variables?.last_message === 'string' && variables.last_message.trim().length > 0;
    }

    return true;
  };

  /** 可选 zen 模型列表；provider 已下线，恒返回空数组（保留接口形状）。 */
  const fetchFreeZenModels = async () => [];

  /**
   * 解析用于摘要的 zen 模型：显式 override 优先，否则读设置中的 zenModel；
   * 均无则返回空串。设置读取失败按空处理。
   */
  const resolveZenModel = async (override) => {
    const overrideModel = typeof override === 'string' ? override.trim() : '';
    if (overrideModel) return overrideModel;
    const settings = await readSettingsFromDisk().catch(() => ({}));
    return typeof settings?.zenModel === 'string' && settings.zenModel.trim().length > 0
      ? settings.zenModel.trim()
      : '';
  };

  /** 启动时校验 zen 模型；provider 已下线，现为空操作。 */
  const validateZenModelAtStartup = async () => {};

  /**
   * 把长文本摘要为不超过 targetLength 的通知文案（mode: 'notification'）。
   * 空文本、摘要失败或摘要为空时原样返回 text。
   */
  const summarizeText = async (text, targetLength, zenModel) => {
    if (!text || typeof text !== 'string' || text.trim().length === 0) return text;
    const result = await summarizeSharedText({
      text,
      threshold: 0,
      maxLength: targetLength,
        zenModel,
      mode: 'notification',
    });
    return typeof result?.summary === 'string' && result.summary.trim().length > 0
      ? result.summary
      : text;
  };

  /** 判断消息 part 是否为可展示的文本片段：type 为 'text' 且带 text 或 content 字符串。 */
  const isNotificationTextPart = (part) => {
    if (!part || typeof part !== 'object') return false;
    if (part.type !== 'text') return false;
    return typeof part.text === 'string' || typeof part.content === 'string';
  };

  /**
   * 从消息 parts 中提取文本片段并以换行拼接，截断到 maxLength。
   * 无文本 part 时返回空串。
   */
  const extractTextFromParts = (parts, maxLength = NOTIFICATION_BODY_MAX_CHARS) => {
    if (!Array.isArray(parts) || parts.length === 0) return '';

    const textParts = parts
      .filter(isNotificationTextPart)
      .map((part) => part.text || part.content || '')
      .filter(Boolean);

    let text = textParts.length > 0 ? textParts.join('\n').trim() : '';

    if (maxLength > 0 && text.length > maxLength) {
      text = text.slice(0, maxLength);
    }

    return text;
  };

  /**
   * 从事件 payload 提取最后一条消息的展示文本。
   * 优先取 info.parts（或 properties.parts）中的 text part；
   * 其次回退到旧结构的 info.content 数组；均无则返回空串。
   */
  const extractLastMessageText = (payload, maxLength = NOTIFICATION_BODY_MAX_CHARS) => {
    const info = payload?.properties?.info;
    if (!info) return '';

    const parts = info.parts || payload?.properties?.parts;
    const text = extractTextFromParts(parts, maxLength);
    if (text) return text;

    const content = info.content;
    if (Array.isArray(content)) {
      const textContent = content
        .filter(isNotificationTextPart)
        .map((entry) => entry.text || '')
        .filter(Boolean);
      if (textContent.length > 0) {
        let result = textContent.join('\n').trim();
        if (maxLength > 0 && result.length > maxLength) {
          result = result.slice(0, maxLength);
        }
        return result;
      }
    }

    return '';
  };

  /**
   * 从 OpenCode API 拉取会话最近消息并提取目标助手消息文本。
   * 优先匹配 messageId 对应的 assistant 消息，否则取最后一条 finish 为
   * 'stop' 的 assistant 消息。请求 3 秒超时；任何失败（非 2xx、结构异常、
   * 抛错）都返回空串。
   */
  const fetchLastAssistantMessageText = async (sessionId, messageId, maxLength = NOTIFICATION_BODY_MAX_CHARS) => {
    if (!sessionId) return '';

    try {
      const url = buildOpenCodeUrl(`/session/${encodeURIComponent(sessionId)}/message`, '');
      const response = await fetch(`${url}?limit=5`, {
        method: 'GET',
        headers: {
          Accept: 'application/json',
          ...getOpenCodeAuthHeaders(),
        },
        signal: AbortSignal.timeout(3000),
      });

      if (!response.ok) return '';

      const messages = await response.json().catch(() => null);
      if (!Array.isArray(messages)) return '';

      let target = null;
      if (messageId) {
        target = messages.find((message) => message?.info?.id === messageId && message?.info?.role === 'assistant');
      }
      if (!target) {
        for (let i = messages.length - 1; i >= 0; i -= 1) {
          const message = messages[i];
          if (message?.info?.role === 'assistant' && message?.info?.finish === 'stop') {
            target = message;
            break;
          }
        }
      }

      if (!target || !Array.isArray(target.parts)) return '';

      return extractTextFromParts(target.parts, maxLength);
    } catch {
      return '';
    }
  };

  /** 写入会话标题缓存；sessionId 或 title 为空时忽略。 */
  const cacheSessionTitle = (sessionId, title) => {
    if (typeof sessionId === 'string' && sessionId.length > 0 && typeof title === 'string' && title.length > 0) {
      sessionTitleCache.set(sessionId, title);
    }
  };

  /** 读取会话标题缓存，未命中返回 null。 */
  const getCachedSessionTitle = (sessionId) => {
    return sessionTitleCache.get(sessionId) ?? null;
  };

  /**
   * 从 session.created / session.updated 事件中顺带缓存会话标题，
   * 供后续 buildTemplateVariables 免拉取使用；其它事件类型直接忽略。
   */
  const maybeCacheSessionInfoFromEvent = (payload) => {
    if (!payload || typeof payload !== 'object') return;
    const type = payload.type;
    if (type !== 'session.updated' && type !== 'session.created') return;
    const info = payload.properties?.info;
    if (!info || typeof info !== 'object') return;
    cacheSessionTitle(info.id, info.title);
  };

  /**
   * 拉取会话信息（标题等），带 60 秒 TTL 缓存。
   * 命中未过期缓存直接返回；请求 2 秒超时，非 2xx 或异常时告警并返回 null。
   */
  const fetchSessionInfo = async (sessionId) => {
    if (!sessionId) return null;

    const cached = sessionInfoCache.get(sessionId);
    if (cached && Date.now() - cached.at < SESSION_INFO_CACHE_TTL_MS) {
      return cached.data;
    }

    try {
      const url = buildOpenCodeUrl(`/session/${encodeURIComponent(sessionId)}`, '');
      const response = await fetch(url, {
        method: 'GET',
        headers: { Accept: 'application/json' },
        signal: AbortSignal.timeout(2000),
      });
      if (!response.ok) {
        console.warn(`[Notification] fetchSessionInfo: ${response.status} for session ${sessionId}`);
        return null;
      }
      const data = await response.json().catch(() => null);
      if (data && typeof data === 'object') {
        sessionInfoCache.set(sessionId, { data, at: Date.now() });
        return data;
      }
      return null;
    } catch (error) {
      console.warn(`[Notification] fetchSessionInfo failed for ${sessionId}:`, error?.message || error);
      return null;
    }
  };

  /**
   * 组装通知模板可用变量：project_name、worktree、branch、session_name、
   * agent_name、model_name、session_id（last_message 由调用方另行填充）。
   * 会话标题依次取事件自带字段 -> 标题缓存 -> OpenCode 会话信息；
   * 项目名优先取设置中与工作区目录匹配的 label，否则取目录末段；
   * 分支经 simple-git 查询当前 HEAD（3 秒超时，失败留空）。
   */
  const buildTemplateVariables = async (payload, sessionId) => {
    const info = payload?.properties?.info || {};

    let sessionTitle = payload?.properties?.sessionTitle || payload?.properties?.session?.title || (typeof info.sessionTitle === 'string' ? info.sessionTitle : '') || '';

    if (!sessionTitle && sessionId) {
      const cached = getCachedSessionTitle(sessionId);
      if (cached) {
        sessionTitle = cached;
      }
    }

    let sessionInfo = null;
    if (!sessionTitle && sessionId) {
      sessionInfo = await fetchSessionInfo(sessionId);
      if (sessionInfo && typeof sessionInfo.title === 'string') {
        sessionTitle = sessionInfo.title;
        cacheSessionTitle(sessionId, sessionTitle);
      }
    }

    const agentName = (() => {
      const mode = typeof info.agent === 'string' && info.agent.trim().length > 0
        ? info.agent.trim()
        : (typeof info.mode === 'string' ? info.mode.trim() : '');
      if (!mode) return 'Agent';
      return mode.split(/[-_\s]+/).filter(Boolean)
        .map((token) => token.charAt(0).toUpperCase() + token.slice(1)).join(' ');
    })();

    const modelName = (() => {
      const raw = typeof info.modelID === 'string' ? info.modelID.trim()
        : (typeof info.model?.modelID === 'string' ? info.model.modelID.trim() : '');
      if (!raw) return 'Assistant';
      return raw.split(/[-_]+/).filter(Boolean)
        .map((part) => part.charAt(0).toUpperCase() + part.slice(1)).join(' ');
    })();

    let projectName = '';
    let branch = '';
    let worktreeDir = '';

    const infoPath = info.path;
    if (typeof infoPath?.root === 'string' && infoPath.root.length > 0) {
      worktreeDir = infoPath.root;
    } else if (typeof infoPath?.cwd === 'string' && infoPath.cwd.length > 0) {
      worktreeDir = infoPath.cwd;
    }

    try {
      const settings = await readSettingsFromDisk();
      const projects = Array.isArray(settings.projects) ? settings.projects : [];

      if (worktreeDir) {
        const normalizedDir = worktreeDir.replace(/\/+$/, '');
        const matchedProject = projects.find((project) => {
          if (!project || typeof project.path !== 'string') return false;
          return project.path.replace(/\/+$/, '') === normalizedDir;
        });
        if (matchedProject && typeof matchedProject.label === 'string' && matchedProject.label.trim().length > 0) {
          projectName = matchedProject.label.trim();
        } else {
          projectName = normalizedDir.split('/').filter(Boolean).pop() || '';
        }
      } else {
        const activeId = typeof settings.activeProjectId === 'string' ? settings.activeProjectId : '';
        const activeProject = activeId ? projects.find((project) => project && project.id === activeId) : projects[0];
        if (activeProject) {
          projectName = typeof activeProject.label === 'string' && activeProject.label.trim().length > 0
            ? activeProject.label.trim()
            : typeof activeProject.path === 'string'
              ? activeProject.path.split('/').pop() || ''
              : '';
          worktreeDir = typeof activeProject.path === 'string' ? activeProject.path : '';
        }
      }
    } catch {
      if (worktreeDir && !projectName) {
        projectName = worktreeDir.split('/').filter(Boolean).pop() || '';
      }
    }

    if (worktreeDir) {
      try {
        const { simpleGit } = await import('simple-git');
        const git = simpleGit({
          baseDir: worktreeDir,
          spawnOptions: { windowsHide: true },
          binary: resolveGitBinaryForSpawn(),
        });
        branch = await Promise.race([
          git.revparse(['--abbrev-ref', 'HEAD']),
          new Promise((_, reject) => setTimeout(() => reject(new Error('git timeout')), 3000)),
        ]).catch(() => '');
      } catch {
      }
    }

    return {
      project_name: formatProjectLabel(projectName),
      worktree: worktreeDir,
      branch: typeof branch === 'string' ? branch.trim() : '',
      session_name: sessionTitle,
      agent_name: agentName,
      model_name: modelName,
      last_message: '',
      session_id: sessionId || '',
    };
  };

  /** 返回 zen 模型列表缓存（当前恒为空）。 */
  const getCachedZenModels = () => cachedZenModels;

  return {
    createTimeoutSignal,
    formatProjectLabel,
    resolveNotificationTemplate,
    shouldApplyResolvedTemplateMessage,
    fetchFreeZenModels,
    resolveZenModel,
    validateZenModelAtStartup,
    summarizeText,
    extractTextFromParts,
    extractLastMessageText,
    fetchLastAssistantMessageText,
    maybeCacheSessionInfoFromEvent,
    buildTemplateVariables,
    getCachedZenModels,
  };
};
