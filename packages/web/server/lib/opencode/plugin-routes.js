/**
 * 插件（plugin）配置的 CRUD 路由：管理 opencode.json 中的插件条目（entry）与
 * plugin 目录下的独立文件（file），另提供 npm registry 规格批量校验端点。
 * 通过 registerPluginRoutes 依赖注入挂载；具体存储与编解码逻辑由 plugins.js、
 * plugin-spec.js 提供，npm 查询默认走 npm-registry.js 的缓存实现。
 */
import fs from 'fs';
import os from 'os';

import { getNpmInfo as defaultGetNpmInfo } from './npm-registry.js';
import { isExactSemver as defaultIsExactSemver, isPathSpec as defaultIsPathSpec, parseNpmSpec as defaultParseNpmSpec, parsePathSpec as defaultParsePathSpec } from './plugin-spec.js';
import { buildDeferredRestartResponse } from './config-mutation-response.js';

/** 视为"插件条目已存在"冲突（HTTP 409）的错误码集合。 */
const ENTRY_EXISTS_CODES = new Set(['ENTRY_EXISTS', 'EEXIST']);
/** 视为"插件文件已存在"冲突（HTTP 409）的错误码集合。 */
const FILE_EXISTS_CODES = new Set(['FILE_EXISTS', 'EEXIST']);
/** 视为"未找到"（HTTP 404）的错误码集合。 */
const NOT_FOUND_CODES = new Set(['NOT_FOUND', 'ENOENT']);
/** 视为"请求参数非法"（HTTP 400）的错误码集合。 */
const BAD_REQUEST_CODES = new Set(['INVALID_FILENAME', 'INVALID_SCOPE', 'INVALID_SPEC', 'EINVAL']);

/**
 * 在 Express app 上注册 /api/config/plugins 系列路由：条目与文件的增删改查、
 * 以及 registry 规格校验。所有插件读写、ID 编解码与 npm 查询能力经
 * dependencies 注入（未提供时回退到 plugin-spec.js / npm-registry.js 默认实现）。
 * @param {import('express').Express} app Express 应用实例
 * @param {object} dependencies 插件存储操作（listPluginEntries 等）、encodePluginId /
 *   decodePluginId、resolveOptionalProjectDirectory 及可覆盖的 npm 校验函数集合
 * @returns {void}
 */
export const registerPluginRoutes = (app, dependencies) => {
  const {
    resolveOptionalProjectDirectory,
    listPluginEntries,
    getPluginEntry,
    createPluginEntry,
    updatePluginEntry,
    deletePluginEntry,
    listPluginDirFiles,
    readPluginDirFile,
    writePluginDirFile,
    deletePluginDirFile,
    encodePluginId,
    decodePluginId,
    getNpmInfo = defaultGetNpmInfo,
    parseNpmSpec = defaultParseNpmSpec,
    parsePathSpec = defaultParsePathSpec,
    isExactSemver = defaultIsExactSemver,
    isPathSpec = defaultIsPathSpec,
  } = dependencies;

  // 依据语法判断 spec 类型：路径规格为 'path'，否则视为 npm 规格。
  const parsedKindForSpec = (spec) => (isPathSpec(spec) ? 'path' : 'npm');

  // 解析可选项目目录；query 无效时直接回 400。返回 null 表示"无目录"或"已写响应"，
  // 调用方需结合 res.headersSent 判断是否中止。
  const resolveDirectory = async (req, res) => {
    const { directory, error } = await resolveOptionalProjectDirectory(req);
    if (error) {
      res.status(400).json({ error });
      return null;
    }
    return directory || null;
  };

  // 执行插件变更并统一返回"延迟重启生效"响应；operation 文案转为过去式嵌入提示。
  const completePluginMutation = async (res, operation, _noun, applyChange) => {
    applyChange();

    const pastTense = operation.replace(/ion$/, 'ed').replace(/update$/, 'updated');
    return res.json(buildDeferredRestartResponse(
      `Plugin ${pastTense}. Restart the engine to apply.`,
    ));
  };

  // 校验 id 解码后前缀为 config（插件条目），否则抛出 NOT_FOUND 错误。
  const validateEntryId = (id) => {
    const decoded = decodePluginId(id);
    if (decoded.prefix !== 'config') {
      const error = new Error('Plugin entry not found');
      error.code = 'NOT_FOUND';
      throw error;
    }
  };

  // 校验 id 解码后前缀为 file（插件文件），否则抛出 NOT_FOUND 错误。
  const validateFileId = (id) => {
    const decoded = decodePluginId(id);
    if (decoded.prefix !== 'file') {
      const error = new Error('Plugin file not found');
      error.code = 'NOT_FOUND';
      throw error;
    }
  };

  // 将底层错误码映射为 HTTP 响应：存在冲突（按 existsKind 区分条目/文件）→409、
  // 未找到→404、非法参数→400；其余记录日志后返回 500 与兜底文案。
  const handlePluginError = (res, error, fallbackMessage, context, existsKind = null) => {
    const code = error?.code;
    if ((existsKind === 'entry' && ENTRY_EXISTS_CODES.has(code)) || (existsKind === 'file' && FILE_EXISTS_CODES.has(code))) {
      return res.status(409).json({ error: error.message });
    }
    if (NOT_FOUND_CODES.has(code)) {
      return res.status(404).json({ error: error.message });
    }
    if (BAD_REQUEST_CODES.has(code)) {
      return res.status(400).json({ error: error.message });
    }

    console.error(context, error);
    return res.status(500).json({ error: fallbackMessage });
  };

  // GET /api/config/plugins：列出当前项目目录下的插件条目与 plugin 目录文件；
  // 目录参数非法时 400，读取失败 500。
  app.get('/api/config/plugins', async (req, res) => {
    try {
      const directory = await resolveDirectory(req, res);
      if (directory === null && res.headersSent) return;

      res.json({
        entries: listPluginEntries(directory),
        files: listPluginDirFiles(directory),
      });
    } catch (error) {
      console.error('[API:GET /api/config/plugins] Failed:', error);
      res.status(500).json({ error: 'Failed to list plugins' });
    }
  });

  // GET /api/config/plugins/registry?specs=a,b&refresh=true：批量校验规格。
  // specs 逗号分隔（逐个 decodeURIComponent、去空、去重，上限 100 个）；路径规格
  // 用 fs 检查存在性/可读性，npm 规格按包名合并查询 registry（refresh=true 强制
  // 绕过缓存），逐条返回 kind：npm-ok / npm-missing-package / npm-missing-version /
  // npm-network / npm-malformed / path-ok / path-missing / path-unreadable。
  app.get('/api/config/plugins/registry', async (req, res) => {
    try {
      const { directory, error: directoryError } = await resolveOptionalProjectDirectory(req);
      if (directoryError) {
        return res.status(400).json({ error: directoryError });
      }
      const rawSpecs = (req.query.specs || '').toString();
      const specs = rawSpecs
        ? rawSpecs.split(',').map((spec) => {
          try {
            return decodeURIComponent(spec);
          } catch {
            return spec;
          }
        }).filter((spec) => spec.length > 0)
        : [];
      const uniqueSpecs = Array.from(new Set(specs));
      if (uniqueSpecs.length > 100) {
        return res.status(400).json({ error: 'too many specs' });
      }

      const refresh = req.query.refresh === 'true';
      const npmJobs = new Map();
      const malformedSpecs = new Set();

      for (const spec of uniqueSpecs) {
        if (parsedKindForSpec(spec) !== 'npm') continue;

        const parsed = parseNpmSpec(spec);
        if (parsed.malformed) {
          malformedSpecs.add(spec);
          continue;
        }

        const job = npmJobs.get(parsed.name) || { specs: [], parsedBySpec: new Map() };
        job.specs.push(spec);
        job.parsedBySpec.set(spec, parsed);
        npmJobs.set(parsed.name, job);
      }

      const npmInfoByName = new Map();
      await Promise.all(Array.from(npmJobs.keys()).map(async (name) => {
        npmInfoByName.set(name, await getNpmInfo(name, { forceRefresh: refresh }));
      }));

      const results = [];
      for (const spec of uniqueSpecs) {
        if (malformedSpecs.has(spec)) {
          results.push({ kind: 'npm-malformed', spec, error: 'Spec syntax is malformed' });
          continue;
        }

        if (parsedKindForSpec(spec) === 'path') {
          const { absolutePath } = parsePathSpec(spec, { homedir: os.homedir(), cwd: directory || os.homedir() });
          try {
            fs.statSync(absolutePath);
          } catch {
            results.push({ kind: 'path-missing', spec, absolutePath });
            continue;
          }

          try {
            fs.accessSync(absolutePath, fs.constants.R_OK);
            results.push({ kind: 'path-ok', spec, absolutePath });
          } catch {
            results.push({ kind: 'path-unreadable', spec, absolutePath });
          }
          continue;
        }

        const parsed = parseNpmSpec(spec);
        const info = npmInfoByName.get(parsed.name);
        if (!info.ok) {
          if (info.status === 404) {
            results.push({ kind: 'npm-missing-package', spec, name: parsed.name, error: info.error });
            continue;
          }

          results.push({ kind: 'npm-network', spec, error: info.status === 'network' ? info.error : `Registry returned ${info.status}` });
          continue;
        }

        const currentVersion = parsed.version;
        if (currentVersion !== null && isExactSemver(currentVersion) && !info.versions.includes(currentVersion)) {
          results.push({
            kind: 'npm-missing-version',
            spec,
            name: parsed.name,
            currentVersion,
            latestVersion: info.latest,
            versions: info.versions,
          });
          continue;
        }

        results.push({
          kind: 'npm-ok',
          spec,
          name: parsed.name,
          currentVersion,
          latestVersion: info.latest,
          versions: info.versions,
          hasUpdate: currentVersion !== null && isExactSemver(currentVersion) && currentVersion !== info.latest,
        });
      }

      return res.json({ results });
    } catch (error) {
      console.error('[API:GET /api/config/plugins/registry]', error);
      return res.status(500).json({ error: 'Failed to query npm registry' });
    }
  });

  // GET /api/config/plugins/entry/:id：读取单个插件条目；id 前缀不符或条目缺失返回 404。
  app.get('/api/config/plugins/entry/:id', async (req, res) => {
    try {
      const directory = await resolveDirectory(req, res);
      if (directory === null && res.headersSent) return;
      validateEntryId(req.params.id);

      const entry = getPluginEntry(req.params.id, directory);
      if (!entry) {
        return res.status(404).json({ error: 'Plugin entry not found' });
      }
      return res.json(entry);
    } catch (error) {
      return handlePluginError(res, error, 'Failed to get plugin entry', '[API:GET /api/config/plugins/entry/:id] Failed:');
    }
  });

  // POST /api/config/plugins/entry：新建插件条目（spec / options / scope）；
  // 成功返回延迟重启响应，同名条目已存在返回 409。
  app.post('/api/config/plugins/entry', async (req, res) => {
    try {
      const directory = await resolveDirectory(req, res);
      if (directory === null && res.headersSent) return;

      await completePluginMutation(res, 'entry creation', 'entry', () => {
        createPluginEntry({
          spec: req.body?.spec,
          options: req.body?.options,
          scope: req.body?.scope,
        }, directory);
      });
    } catch (error) {
      return handlePluginError(res, error, 'Failed to create plugin entry', '[API:POST /api/config/plugins/entry] Failed:', 'entry');
    }
  });

  // PATCH /api/config/plugins/entry/:id：更新插件条目的 spec / options；成功返回延迟重启响应。
  app.patch('/api/config/plugins/entry/:id', async (req, res) => {
    try {
      const directory = await resolveDirectory(req, res);
      if (directory === null && res.headersSent) return;
      validateEntryId(req.params.id);

      await completePluginMutation(res, 'entry update', 'entry', () => {
        updatePluginEntry(req.params.id, {
          spec: req.body?.spec,
          options: req.body?.options,
        }, directory);
      });
    } catch (error) {
      return handlePluginError(res, error, 'Failed to update plugin entry', '[API:PATCH /api/config/plugins/entry/:id] Failed:', 'entry');
    }
  });

  // DELETE /api/config/plugins/entry/:id：删除插件条目；成功返回延迟重启响应。
  app.delete('/api/config/plugins/entry/:id', async (req, res) => {
    try {
      const directory = await resolveDirectory(req, res);
      if (directory === null && res.headersSent) return;
      validateEntryId(req.params.id);

      await completePluginMutation(res, 'entry deletion', 'entry', () => {
        deletePluginEntry(req.params.id, directory);
      });
    } catch (error) {
      return handlePluginError(res, error, 'Failed to delete plugin entry', '[API:DELETE /api/config/plugins/entry/:id] Failed:', 'entry');
    }
  });

  // GET /api/config/plugins/file/:id：读取单个插件文件内容；id 前缀不符或文件缺失返回 404。
  app.get('/api/config/plugins/file/:id', async (req, res) => {
    try {
      const directory = await resolveDirectory(req, res);
      if (directory === null && res.headersSent) return;
      validateFileId(req.params.id);

      const file = readPluginDirFile(req.params.id, directory);
      if (!file) {
        return res.status(404).json({ error: 'Plugin file not found' });
      }
      return res.json(file);
    } catch (error) {
      return handlePluginError(res, error, 'Failed to read plugin file', '[API:GET /api/config/plugins/file/:id] Failed:');
    }
  });

  // POST /api/config/plugins/file：新建插件文件。以 scope:fileName 命名空间编码出
  // file id 并校验前缀，写入内容；同名文件已存在返回 409。
  app.post('/api/config/plugins/file', async (req, res) => {
    try {
      const directory = await resolveDirectory(req, res);
      if (directory === null && res.headersSent) return;
      const id = encodePluginId('file', `${req.body?.scope || 'user'}:${req.body?.fileName || ''}`);

      await completePluginMutation(res, 'file creation', 'file', () => {
        validateFileId(id);
        writePluginDirFile({
          fileName: req.body?.fileName,
          content: req.body?.content,
          scope: req.body?.scope,
        }, directory);
      });
    } catch (error) {
      return handlePluginError(res, error, 'Failed to create plugin file', '[API:POST /api/config/plugins/file] Failed:', 'file');
    }
  });

  // PUT /api/config/plugins/file/:id：覆盖更新已存在插件文件的内容（沿用原文件名
  // 与 scope，overwrite 强制写入）；目标不存在返回 404。
  app.put('/api/config/plugins/file/:id', async (req, res) => {
    try {
      const directory = await resolveDirectory(req, res);
      if (directory === null && res.headersSent) return;
      validateFileId(req.params.id);

      const existing = readPluginDirFile(req.params.id, directory);
      if (!existing) {
        return res.status(404).json({ error: 'Plugin file not found' });
      }

      await completePluginMutation(res, 'file update', 'file', () => {
        writePluginDirFile({
          fileName: existing.fileName,
          content: req.body?.content,
          scope: existing.scope,
        }, directory, { overwrite: true });
      });
    } catch (error) {
      return handlePluginError(res, error, 'Failed to update plugin file', '[API:PUT /api/config/plugins/file/:id] Failed:', 'file');
    }
  });

  // DELETE /api/config/plugins/file/:id：删除插件文件；成功返回延迟重启响应。
  app.delete('/api/config/plugins/file/:id', async (req, res) => {
    try {
      const directory = await resolveDirectory(req, res);
      if (directory === null && res.headersSent) return;
      validateFileId(req.params.id);

      await completePluginMutation(res, 'file deletion', 'file', () => {
        deletePluginDirFile(req.params.id, directory);
      });
    } catch (error) {
      return handlePluginError(res, error, 'Failed to delete plugin file', '[API:DELETE /api/config/plugins/file/:id] Failed:', 'file');
    }
  });
};
