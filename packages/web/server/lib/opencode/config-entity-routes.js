/**
 * OpenCode 配置实体（agent / command / MCP / snippet）的 CRUD 路由模块。
 *
 * 统一挂载四类配置资产的 REST 端点：agent 走 /api/config/agents、
 * command 走 /api/config/commands、MCP server 走 /api/config/mcp、
 * snippet 走 /api/config/snippets。所有写操作只落盘，OpenCode 引擎
 * 重启被推迟到用户显式的 Apply & Restart（响应统一用
 * buildDeferredRestartResponse 标注 restartDeferred），避免打断在跑
 * 会话。各实体的实际存取实现由 dependencies 注入（agents.js /
 * commands.js / mcp.js / snippets.js 等），本模块只负责参数解包、
 * 目录解析、错误码映射与响应包装。
 */
import { buildDeferredRestartResponse } from './config-mutation-response.js';

/**
 * 在 express app 上注册 agent / command / MCP / snippet 配置路由。
 *
 * @param app express 应用实例
 * @param dependencies 注入依赖：resolveProjectDirectory /
 *   resolveOptionalProjectDirectory 解析请求对应的项目目录（后者允许
 *   无目录的全局配置请求）；getAgentSources / getAgentConfig /
 *   createAgent / updateAgent / deleteAgent、getCommandSources /
 *   createCommand / updateCommand / deleteCommand、listMcpConfigs /
 *   getMcpConfig / createMcpConfig / updateMcpConfig / deleteMcpConfig、
 *   listSnippets / getSnippet / createSnippet / updateSnippet /
 *   deleteSnippet / expandSnippets 为各实体的具体存取函数。
 */
export const registerConfigEntityRoutes = (app, dependencies) => {
  const {
    resolveProjectDirectory,
    resolveOptionalProjectDirectory,
    getAgentSources,
    getAgentConfig,
    createAgent,
    updateAgent,
    deleteAgent,
    getCommandSources,
    createCommand,
    updateCommand,
    deleteCommand,
    listMcpConfigs,
    getMcpConfig,
    createMcpConfig,
    updateMcpConfig,
    deleteMcpConfig,
    listSnippets,
    getSnippet,
    createSnippet,
    updateSnippet,
    deleteSnippet,
    expandSnippets,
  } = dependencies;

  // Persist to disk immediately; OpenCode restart is deferred to an explicit
  // Apply & Restart so settings edits do not interrupt live sessions.
  /**
   * MCP 变更收尾：先同步执行 applyChange 落盘，再按动作词拼出过去式
   * 消息并返回 deferred-restart 响应（不立即重启引擎，见上方英文注释）。
   */
  const completeMcpMutation = async (res, action, name, applyChange) => {
    applyChange();
    const past = action === 'delete' ? 'deleted' : `${action}d`;
    return res.json(buildDeferredRestartResponse(
      `MCP server "${name}" ${past}. Restart the engine to apply.`,
    ));
  };

  // GET /api/config/agents/:name — 返回指定 agent 的来源信息（md/json
  // 双源）、生效 scope 与 isBuiltIn；项目目录解析失败 400，内部异常 500。
  app.get('/api/config/agents/:name', async (req, res) => {
    try {
      const agentName = req.params.name;
      const { directory, error } = await resolveProjectDirectory(req);
      if (!directory) {
        return res.status(400).json({ error });
      }
      const sources = getAgentSources(agentName, directory);

      const scope = sources.md.exists
        ? sources.md.scope
        : (sources.json.exists ? sources.json.scope : null);

      res.json({
        name: agentName,
        sources: sources,
        scope,
        isBuiltIn: !sources.md.exists && !sources.json.exists
      });
    } catch (error) {
      console.error('Failed to get agent sources:', error);
      res.status(500).json({ error: 'Failed to get agent configuration metadata' });
    }
  });

  // GET /api/config/agents/:name/config — 返回该 agent 的完整配置
  // （getAgentConfig 的合并结果）；目录解析失败 400，读取异常 500。
  app.get('/api/config/agents/:name/config', async (req, res) => {
    try {
      const agentName = req.params.name;
      const { directory, error } = await resolveProjectDirectory(req);
      if (!directory) {
        return res.status(400).json({ error });
      }

      const configInfo = getAgentConfig(agentName, directory);
      res.json(configInfo);
    } catch (error) {
      console.error('Failed to get agent config:', error);
      res.status(500).json({ error: 'Failed to get agent configuration' });
    }
  });

  // POST /api/config/agents/:name — 创建 agent：body 中的 scope 单独
  // 取出，其余作为配置写入；成功返回 deferred-restart 响应，目录解析
  // 失败 400，创建异常 500（透出 error.message）。
  app.post('/api/config/agents/:name', async (req, res) => {
    try {
      const agentName = req.params.name;
      const { scope, ...config } = req.body;
      const { directory, error } = await resolveProjectDirectory(req);
      if (!directory) {
        return res.status(400).json({ error });
      }

      console.log('[Server] Creating agent:', agentName);
      console.log('[Server] Config received:', JSON.stringify(config, null, 2));
      console.log('[Server] Scope:', scope, 'Working directory:', directory);

      createAgent(agentName, config, directory, scope);
      res.json(buildDeferredRestartResponse(
        `Agent ${agentName} created successfully. Restart the engine to apply.`,
      ));
    } catch (error) {
      console.error('Failed to create agent:', error);
      res.status(500).json({ error: error.message || 'Failed to create agent' });
    }
  });

  // PATCH /api/config/agents/:name — 用 body 作为增量更新 agent，
  // 成功返回 deferred-restart 响应；目录解析失败 400，更新异常 500。
  app.patch('/api/config/agents/:name', async (req, res) => {
    try {
      const agentName = req.params.name;
      const updates = req.body;
      const { directory, error } = await resolveProjectDirectory(req);
      if (!directory) {
        return res.status(400).json({ error });
      }

      console.log(`[Server] Updating agent: ${agentName}`);
      console.log('[Server] Updates:', JSON.stringify(updates, null, 2));
      console.log('[Server] Working directory:', directory);

      updateAgent(agentName, updates, directory);

      console.log(`[Server] Agent ${agentName} updated successfully`);

      res.json(buildDeferredRestartResponse(
        `Agent ${agentName} updated successfully. Restart the engine to apply.`,
      ));
    } catch (error) {
      console.error('[Server] Failed to update agent:', error);
      console.error('[Server] Error stack:', error.stack);
      res.status(500).json({ error: error.message || 'Failed to update agent' });
    }
  });

  // DELETE /api/config/agents/:name — 删除 agent（body.scope 可指定
  // 删除范围）；成功返回 deferred-restart 响应；错误路径同上。
  app.delete('/api/config/agents/:name', async (req, res) => {
    try {
      const agentName = req.params.name;
      const { directory, error } = await resolveProjectDirectory(req);
      if (!directory) {
        return res.status(400).json({ error });
      }

      const scope = req.body?.scope;
      deleteAgent(agentName, directory, scope);
      res.json(buildDeferredRestartResponse(
        `Agent ${agentName} deleted successfully. Restart the engine to apply.`,
      ));
    } catch (error) {
      console.error('Failed to delete agent:', error);
      res.status(500).json({ error: error.message || 'Failed to delete agent' });
    }
  });

  // GET /api/config/mcp — 列出全部 MCP server 配置（用 optional 目录
  // 解析器，允许无项目目录的全局请求）；解析失败 400，读取异常 500。
  app.get('/api/config/mcp', async (req, res) => {
    try {
      const { directory, error } = await resolveOptionalProjectDirectory(req);
      if (error) {
        return res.status(400).json({ error });
      }
      const configs = listMcpConfigs(directory);
      res.json(configs);
    } catch (error) {
      console.error('[API:GET /api/config/mcp] Failed:', error);
      res.status(500).json({ error: error.message || 'Failed to list MCP configs' });
    }
  });

  // GET /api/config/mcp/:name — 返回单个 MCP server 配置；不存在 404，
  // 目录解析失败 400，其余异常 500。
  app.get('/api/config/mcp/:name', async (req, res) => {
    try {
      const name = req.params.name;
      const { directory, error } = await resolveOptionalProjectDirectory(req);
      if (error) {
        return res.status(400).json({ error });
      }
      const config = getMcpConfig(name, directory);
      if (!config) {
        return res.status(404).json({ error: `MCP server "${name}" not found` });
      }
      res.json(config);
    } catch (error) {
      console.error('[API:GET /api/config/mcp/:name] Failed:', error);
      res.status(500).json({ error: error.message || 'Failed to get MCP config' });
    }
  });

  // POST /api/config/mcp/:name — 创建 MCP server：body 拆出 scope 后
  // 其余作为配置；落盘后经 completeMcpMutation 返回 deferred-restart
  // 响应；异常 500。
  app.post('/api/config/mcp/:name', async (req, res) => {
    try {
      const name = req.params.name;
      const { scope, ...config } = req.body || {};
      const { directory, error } = await resolveOptionalProjectDirectory(req);
      if (error) {
        return res.status(400).json({ error });
      }
      console.log(`[API:POST /api/config/mcp] Creating MCP server: ${name}`);

      await completeMcpMutation(res, 'create', name, () => {
        createMcpConfig(name, config, directory, scope);
      });
    } catch (error) {
      console.error('[API:POST /api/config/mcp/:name] Failed:', error);
      res.status(500).json({ error: error.message || 'Failed to create MCP server' });
    }
  });

  // PATCH /api/config/mcp/:name — 增量更新 MCP server；目标不存在的
  // 错误消息被识别为 404，其余异常 500。
  app.patch('/api/config/mcp/:name', async (req, res) => {
    try {
      const name = req.params.name;
      const updates = req.body;
      const { directory, error } = await resolveOptionalProjectDirectory(req);
      if (error) {
        return res.status(400).json({ error });
      }
      console.log(`[API:PATCH /api/config/mcp] Updating MCP server: ${name}`);

      await completeMcpMutation(res, 'update', name, () => {
        updateMcpConfig(name, updates, directory);
      });
    } catch (error) {
      console.error('[API:PATCH /api/config/mcp/:name] Failed:', error);
      if (error?.message === `MCP server "${req.params.name}" not found`) {
        return res.status(404).json({ error: error.message });
      }
      res.status(500).json({ error: error.message || 'Failed to update MCP server' });
    }
  });

  // DELETE /api/config/mcp/:name — 删除 MCP server 并返回
  // deferred-restart 响应；异常 500。
  app.delete('/api/config/mcp/:name', async (req, res) => {
    try {
      const name = req.params.name;
      const { directory, error } = await resolveOptionalProjectDirectory(req);
      if (error) {
        return res.status(400).json({ error });
      }
      console.log(`[API:DELETE /api/config/mcp] Deleting MCP server: ${name}`);

      await completeMcpMutation(res, 'delete', name, () => {
        deleteMcpConfig(name, directory);
      });
    } catch (error) {
      console.error('[API:DELETE /api/config/mcp/:name] Failed:', error);
      res.status(500).json({ error: error.message || 'Failed to delete MCP server' });
    }
  });

  // GET /api/config/commands/:name — 返回指定 command 的来源信息
  // （md/json 双源）、生效 scope 与 isBuiltIn；目录解析失败 400，
  // 内部异常 500。
  app.get('/api/config/commands/:name', async (req, res) => {
    try {
      const commandName = req.params.name;
      const { directory, error } = await resolveProjectDirectory(req);
      if (!directory) {
        return res.status(400).json({ error });
      }
      const sources = getCommandSources(commandName, directory);

      const scope = sources.md.exists
        ? sources.md.scope
        : (sources.json.exists ? sources.json.scope : null);

      res.json({
        name: commandName,
        sources: sources,
        scope,
        isBuiltIn: !sources.md.exists && !sources.json.exists
      });
    } catch (error) {
      console.error('Failed to get command sources:', error);
      res.status(500).json({ error: 'Failed to get command configuration metadata' });
    }
  });

  // POST /api/config/commands/:name — 创建 command：body 拆出 scope 后
  // 其余作为配置，成功返回 deferred-restart 响应。
  app.post('/api/config/commands/:name', async (req, res) => {
    try {
      const commandName = req.params.name;
      const { scope, ...config } = req.body;
      const { directory, error } = await resolveProjectDirectory(req);
      if (!directory) {
        return res.status(400).json({ error });
      }

      console.log('[Server] Creating command:', commandName);
      console.log('[Server] Config received:', JSON.stringify(config, null, 2));
      console.log('[Server] Scope:', scope, 'Working directory:', directory);

      createCommand(commandName, config, directory, scope);
      res.json(buildDeferredRestartResponse(
        `Command ${commandName} created successfully. Restart the engine to apply.`,
      ));
    } catch (error) {
      console.error('Failed to create command:', error);
      res.status(500).json({ error: error.message || 'Failed to create command' });
    }
  });

  // PATCH /api/config/commands/:name — 增量更新 command，成功返回
  // deferred-restart 响应；目录解析失败 400，异常 500。
  app.patch('/api/config/commands/:name', async (req, res) => {
    try {
      const commandName = req.params.name;
      const updates = req.body;
      const { directory, error } = await resolveProjectDirectory(req);
      if (!directory) {
        return res.status(400).json({ error });
      }

      console.log(`[Server] Updating command: ${commandName}`);
      console.log('[Server] Updates:', JSON.stringify(updates, null, 2));
      console.log('[Server] Working directory:', directory);

      updateCommand(commandName, updates, directory);

      console.log(`[Server] Command ${commandName} updated successfully`);

      res.json(buildDeferredRestartResponse(
        `Command ${commandName} updated successfully. Restart the engine to apply.`,
      ));
    } catch (error) {
      console.error('[Server] Failed to update command:', error);
      console.error('[Server] Error stack:', error.stack);
      res.status(500).json({ error: error.message || 'Failed to update command' });
    }
  });

  // DELETE /api/config/commands/:name — 删除 command，成功返回
  // deferred-restart 响应；目录解析失败 400，异常 500。
  app.delete('/api/config/commands/:name', async (req, res) => {
    try {
      const commandName = req.params.name;
      const { directory, error } = await resolveProjectDirectory(req);
      if (!directory) {
        return res.status(400).json({ error });
      }

      deleteCommand(commandName, directory);
      res.json(buildDeferredRestartResponse(
        `Command ${commandName} deleted successfully. Restart the engine to apply.`,
      ));
    } catch (error) {
      console.error('Failed to delete command:', error);
      res.status(500).json({ error: error.message || 'Failed to delete command' });
    }
  });

  // GET /api/config/snippets — 列出全部 snippet；目录解析失败 400，
  // 读取异常 500。
  app.get('/api/config/snippets', async (req, res) => {
    try {
      const { directory, error } = await resolveOptionalProjectDirectory(req);
      if (error) {
        return res.status(400).json({ error });
      }
      res.json(listSnippets(directory));
    } catch (error) {
      console.error('[API:GET /api/config/snippets] Failed:', error);
      res.status(500).json({ error: error.message || 'Failed to list snippets' });
    }
  });

  // POST /api/config/snippets/expand — 对 body.text 做 snippet 展开
  // （{snippet:name} 引用替换），返回 { text }；异常 500。
  app.post('/api/config/snippets/expand', async (req, res) => {
    try {
      const { directory, error } = await resolveOptionalProjectDirectory(req);
      if (error) {
        return res.status(400).json({ error });
      }
      res.json({ text: expandSnippets(req.body?.text ?? '', directory) });
    } catch (error) {
      console.error('[API:POST /api/config/snippets/expand] Failed:', error);
      res.status(500).json({ error: error.message || 'Failed to expand snippets' });
    }
  });

  // GET /api/config/snippets/:name — 返回单个 snippet；不存在 404，
  // 名称非法（错误消息含 “Snippet name”）400，其余 500。
  app.get('/api/config/snippets/:name', async (req, res) => {
    try {
      const name = req.params.name;
      const { directory, error } = await resolveOptionalProjectDirectory(req);
      if (error) {
        return res.status(400).json({ error });
      }
      const snippet = getSnippet(name, directory);
      if (!snippet) {
        return res.status(404).json({ error: `Snippet "${name}" not found` });
      }
      res.json(snippet);
    } catch (error) {
      console.error('[API:GET /api/config/snippets/:name] Failed:', error);
      if (error.message?.includes('Snippet name')) {
        return res.status(400).json({ error: error.message });
      }
      res.status(500).json({ error: error.message || 'Failed to get snippet' });
    }
  });

  // POST /api/config/snippets/:name — 创建 snippet（scope 缺省
  // global）；已存在 409，名称/目录非法 400，成功返回
  // { success, snippet }。
  app.post('/api/config/snippets/:name', async (req, res) => {
    try {
      const name = req.params.name;
      const { directory, error } = await resolveOptionalProjectDirectory(req);
      if (error) {
        return res.status(400).json({ error });
      }
      const snippet = createSnippet(name, req.body || {}, directory, req.body?.scope || 'global');
      res.json({ success: true, snippet });
    } catch (error) {
      console.error('[API:POST /api/config/snippets/:name] Failed:', error);
      if (error.message?.includes('already exists')) {
        return res.status(409).json({ error: error.message });
      }
      if (error.message?.includes('Snippet name') || error.message?.includes('Project directory')) {
        return res.status(400).json({ error: error.message });
      }
      res.status(500).json({ error: error.message || 'Failed to create snippet' });
    }
  });

  // PATCH /api/config/snippets/:name — 更新 snippet；不存在 404，
  // 名称非法 400，成功返回 { success, snippet }。
  app.patch('/api/config/snippets/:name', async (req, res) => {
    try {
      const name = req.params.name;
      const { directory, error } = await resolveOptionalProjectDirectory(req);
      if (error) {
        return res.status(400).json({ error });
      }
      res.json({ success: true, snippet: updateSnippet(name, req.body || {}, directory) });
    } catch (error) {
      console.error('[API:PATCH /api/config/snippets/:name] Failed:', error);
      if (error.message?.includes('not found')) {
        return res.status(404).json({ error: error.message });
      }
      if (error.message?.includes('Snippet name')) {
        return res.status(400).json({ error: error.message });
      }
      res.status(500).json({ error: error.message || 'Failed to update snippet' });
    }
  });

  // DELETE /api/config/snippets/:name — 删除 snippet；不存在 404，
  // 名称非法 400，成功返回 { success: true }。
  app.delete('/api/config/snippets/:name', async (req, res) => {
    try {
      const name = req.params.name;
      const { directory, error } = await resolveOptionalProjectDirectory(req);
      if (error) {
        return res.status(400).json({ error });
      }
      deleteSnippet(name, directory);
      res.json({ success: true });
    } catch (error) {
      console.error('[API:DELETE /api/config/snippets/:name] Failed:', error);
      if (error.message?.includes('not found')) {
        return res.status(404).json({ error: error.message });
      }
      if (error.message?.includes('Snippet name')) {
        return res.status(400).json({ error: error.message });
      }
      res.status(500).json({ error: error.message || 'Failed to delete snippet' });
    }
  });
};
