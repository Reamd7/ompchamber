# 01 · 域 A:模型选择与 model roles(取代默认模型链)

状态:设计稿(v3,修订轮:落地 00-MASTER D6 裁决 R1/R2/R3/R4/R6/R12;修订轮 2 落地 R2 评审 H3——project-scope 角色的会话级生效依赖 06 §5.1「每目录 keyed Settings 实例注入」)
基线:2026-08-19;omp SDK = `@oh-my-pi/pi-coding-agent`(安装副本,下文 `<s>` = `C:/Users/reamd/.bun/install/global/node_modules/@oh-my-pi/pi-coding-agent/src`)
裁决依据:00-MASTER D1(双轨契约)、D2(事件显式处置)、D3 概念映射第 2 行「默认模型(defaultModel 设置项)→ `modelRoles.default`,UI 不再每 prompt 强制显式 model」、D4 P0 第 2/3 条;修订轮追加 D6 冻结契约——R1(事件单通道)、R2(capabilities 门控)、R3(路径复数)、R4(进程归属与代理)、R6(settings 多目录例外;R2 修订:会话消费改每目录 keyed 实例注入,06 §5.1/总纲回写挂 06 OQ-F1)、R12(defaultModel 迁移、resync)。

---

## 1. 域概述与边界

**本域管:**

- 「这场会话用哪个模型」的**解析权威**:omp settings 的 `modelRoles`(全局 `~/.omp/agent/config.yml` + 项目 `<cwd>/.omp/config.yml` 的 modelRoles 子树)取代 OpenChamber 三层平行默认模型级联(OC host settings `defaultModel` → OpenCode agent-pinned → OpenCode `config.model` → 硬编码 → first)。两层均经 omp-host **每目录 keyed Settings 实例**被会话真实消费(`createAgentSession` 的 `options.settings` 注入,sdk.ts:1273-1275;06 §5.1 REVISED R2)——project-scope 对该目录会话是**实际生效的权威层,非仅 UI 展示**(修订轮 2,R2 评审 H3)。
- **prompt 的 model 参数语义**:从「每次发送都携带 providerID/modelID/variant」改为「prompt 省略 model,会话保持自身模型;换模型 = 显式 setModel 动作」。
- **model roles 体系的产品面**:10 个内置角色(default/smol/slow/vision/plan/designer/commit/tiny/task/advisor)+ 自定义角色,角色赋值的持久化(`modelRoleStorage` global|project)、`cycleOrder` 快速轮换语义。
- **thinking levels**(`defaultThinkingLevel`、per-session thinking、auto 分辨)从 OpenCode 的 `variant` 概念迁移到 omp `thinkingLevel`。
- **retry fallback 链**(`retry.fallbackChains`)的事件透出与模型徽标真相(model badge 必须显示实际运行模型)。
- RuntimeAPIs 新端点组 `/api/omp/models`、`/api/omp/model-roles`、`/api/omp/sessions/{id}/model`(D1:omp 原生概念走自有面,不进 wire gen;D6-R3 复数路径)。

**本域不管:**

- build/plan agent 二分的删除与 custom agents 体系(→ 02 章;本域只接管其中「模型」维度,`meta.agent` 的处置在 02 章)。
- 事件通道本体(SSE 连接、envelope、事件 ID、durable/volatile、`Last-Event-ID` 重放、resync 矩阵与 reducer 分发框架)→ 05 章唯一 `OmpEventBus → /api/omp/events`(D1/D6-R1);本域只**按名声明** model 域 4 个公开事件(`omp.model.changed`/`omp.thinking.changed`/`omp.fallback.applied`/`omp.fallback.succeeded`)的 payload 语义,登记于 05 章事件注册表。
- 设置面板的 schema 代理与 DefaultsSettings 页面整体改造 → 06 章;本域定义其中 defaultModel/defaultVariant 字段的**语义处置**(代理写还是弃用)。
- OpenCode 残留(`permission`/`share`/`shell` 等)→ 07 章。
- multirun/AgentManager、scheduled tasks、GitHub issue picker 等原创面的模型输入迁移 → 08 章(本域给出它们应调用的 API 契约)。

**与其他章的接口:**

- 02 章删除 build/plan 后,`engine.js` 的 agent 重建逻辑(engine.js:639-649)与本域的 setModel 端点共用 `#materialize` 重建路径。
- 05 章:唯一事件通道承载本域 4 个事件(注册表列:SDK source → 公开名 → payload → producer → durable/volatile → 作用域 → 快照端点 → reducer,D6-R1);本域 GAP-07/08 对应的 SDK 事件是 05 章「12 类 drop 事件」清单的子集——处置与 payload 语义以本章声明为准,通道与注册表以 05 章为唯一权威。
- 06 章的设置页承载 model tab(role 编辑器、enabledModels、modelRoleStorage、defaultThinkingLevel 的 UI);本域定义数据契约。

---

## 2. 现状分析(OpenChamber 侧)

### 2.1 默认模型级联(UI 侧,五层)

`packages/ui/src/stores/useConfigStore.ts:282-293` 的注释是这条级联的自白:

```
Model: project.defaultModel → settings.defaultModel → resolved agent's pinned model+variant
       → opencode config.model → opencode/big-pickle → first
```

实现 `resolveDefaultAgentModelSelection`(useConfigStore.ts:294-399):

1. `projectDefaultModel || settingsDefaultModel`(项目字段优先)parse 后须在 providers 列表内(useConfigStore.ts:355-364);
2. 否则用解析出的 agent 的 pinned `model`+`variant`(useConfigStore.ts:366-373);
3. 否则 OpenCode 全局 `config.model`(useConfigStore.ts:376-382);
4. 否则硬编码 `opencode/big-pickle`(FALLBACK,useConfigStore.ts:384-387);
5. 否则第一个 provider 的第一个模型(useConfigStore.ts:389-395)。

这条级联的**输入面**:

- **全局设置页**:`packages/ui/src/components/sections/openchamber/DefaultsSettings.tsx:50-51` 维护 `defaultModel`/`defaultVariant` state,保存时 `PUT /api/config/settings`(DefaultsSettings.tsx:162-167)+ `updateDesktopSettings`(桌面侧 `packages/ui/src/lib/desktop.ts:124-129` 的 `defaultModel`/`defaultVariant`/`defaultAgent` 字段)。
- **项目设置页**:项目条目自带 `defaultModel` 字段(`packages/ui/src/components/sections/projects/ProjectIdentityFields.tsx:77-82`「Default model for new chats」;类型 `packages/ui/src/lib/api/types.ts:628`;服务端规范化 `packages/web/server/lib/opencode/settings-normalization-runtime.js:157,177`)。
- **OpenCode config 指针**:omp-host `GET /config` 返回的 `model` 字段,经 `applyOpenCodeConfigDefaults`(useConfigStore.ts:2023-2065,scout 证据)灌入 `opencodeDefaultModel`。

**服务端还有第二条平行级联**:`packages/web/server/lib/openchamber-sessions/routes.js:121-174` 的 `resolveDefaultSelection`(为 GitHub issue picker、worktree 等服务端发起的会话解析 agent+model+variant):`settings.defaultModel` → resolved agent pinned → `opencodeDefaultModel` → `FALLBACK_PROVIDER_ID/MODEL_ID` → first。与 UI 级联同构但各自维护。控制面 `openchamber-control/service.js:190-193` 又把 `defaultModel`/`defaultVariant` 原样透出。

**关键事实:这整条级联与 omp 的解析链是两套宇宙。** omp 没有 `defaultProvider`/`defaultModel` 键(`<s>`/config/settings-schema.ts 全文无此键;默认模型 = `modelRoles` record,settings-schema.ts:561)。当用户在 omp 侧配置了 `modelRoles.default`,OpenChamber 只会碰巧通过第 3 层(`config.model` 指针)读到它——而该指针本身是错的(见 2.3)。

### 2.2 每 prompt 强制显式 model

UI 发送路径全程携带模型三元组:

- `packages/ui/src/lib/opencode/client.ts:779-805`:`sendMessage` 参数签名把 `providerID`/`modelID` 声明为**必填**;
- `client.ts:896-908`:`session.promptAsync` 请求体**无条件**携带 `model: {providerID, modelID}`、`agent`、`variant`;
- `packages/ui/src/sync/session-ui-store.ts:124-222`:`routeMessage` 把 `providerID`/`modelID`/`variant` 穿透进 shell(146)、slash command(179-191)、普通 prompt(206-220)三条路径;
- 乐观消息也带模型:optimistic user message 携带 `model: providerID/modelID`(sync/session-actions.ts:1357-1369,scout 证据)。

接收侧 omp-host 的处置:

- `packages/web/server/lib/omp-host/endpoints.js:238-287`:三条 prompt 路由(`/message` 同步、`/prompt_async`、`/command`)把 `body.model`/`body.agent` 传给 `engine.prompt`;**`body.variant` 被静默丢弃**(grep 全 omp-host 无 variant 消费点)——即 UI 的 thinking variant 选择对 omp 引擎是 no-op。
- `packages/web/server/lib/omp-host/engine.js:619-704` `prompt()`:`model` 存在且与 `session.model` 不同时调用 `session.setModel(target)` 切换(628-636),并把选择器写入 sidecar registry(634);随后 `session.prompt(textOnly, {images, streamingBehavior})`(702)。引擎侧其实已经是「session 持有模型、prompt 可不带」的形态——**是 UI 每次强制重申制造了「每次 prompt 都是一次潜在 setModel」的语义**。

选择记忆(localStorage,`packages/ui/src/stores/contextStore.ts:40-49`):`saveSessionModelSelection` / `saveAgentModelForSession` / `saveAgentModelVariantForSession` 按 (sessionId)、(sessionId,agentName) 维度缓存模型与 variant。会话头的当前模型显示、ChatMessage 徽标(`packages/ui/src/components/chat/ChatMessage.tsx:281-363`)从 `message.info` → 前一条 user 元数据 → 本地 selection 兜底解析。

### 2.3 `/config` model 指针错误

`packages/web/server/lib/omp-host/endpoints.js:135-151` `configPayload()`:

```js
...(engine.availableModels()[0]
  ? { model: `${engine.availableModels()[0].provider}/${engine.availableModels()[0].id}` }
  : {}),
```

`availableModels()` = `modelRegistry.getAvailable()`(engine.js:823-825),`[0]` 即排序碰巧第一的模型——与用户配置无关。注意 engine 的**会话物化路径已经修对**(engine.js:443-450:无持久选择器时 `createAgentSession` 按 settings 默认解析,注释明言「Pinning getAvailable()[0] here used to override the user's configured default」);`/config` 指针是同一 bug 的残留面。UI 侧该指针仅作级联第 3 层输入,且 `PATCH /config` 只存 custom agents(endpoints.js:146-149)。(修订轮 2:该 settings 默认按 06 §5.1 R2 升级为**本目录 keyed 实例**——`#materialize` 注入 `#settingsFor(directoryKey)`,多目录下各目录会话解析各自的全局+项目层。)

### 2.4 会话模型持久化:sidecar registry

`packages/web/server/lib/omp-host/registry.js`(每项目一 JSON,`openchamber-session-meta.json`,registry.js:22)为每个会话存 `model`/`agent`/title/timeCreated/revert/parentID/metadata/timeArchived。写入点:prompt 时显式切换(engine.js:634)、fork 复制(engine.js:731-739)。读取点:`#materialize` 冷启动把 `meta.model` 作为显式 model 传给 `createAgentSession`(engine.js:448-450)、`#wireSession` 把它投影成 wire `Session.model`(engine.js:157,166)。

**与 SDK 的重复**:omp SDK 自己把模型切换持久化进会话 transcript(`appendModelChange`,<s>/session/model-controls.ts:224),恢复时 `getRestorableSessionModels(existingSession.models, …)` 按 fallback 顺序还原(<s>/sdk.ts:1428-1462)。sidecar 的 `meta.model` 是这套机制的平行副本——单选择器、无 fallback 顺序、只在 OpenChamber 写入时同步。若 setModel 从新通路发生(见 4.4)而不同步 registry,两者会发散。

### 2.5 被引擎丢弃的模型域事件

`engine.js:611-613` 的 `default: return` 清零掉本域 4 个 SDK 事件(`model_changed`、`thinking_level_changed`、`retry_fallback_applied`、`retry_fallback_succeeded`,payload 见 <s>/session/agent-session-events.ts:48-63)。后果:

- **中途换模型/fallback 后 UI 徽标撒谎**:fallback 发生时引擎已切到备用模型(event-controller 侧 omp TUI 会 showWarning,<s>/modes/controllers/event-controller.ts:2036-2046),OpenChamber 的 transcript 徽标仍显示用户原选模型——「OpenChamber can silently run a different model than selected」(WireResidue scout B3/B4 结论)。
- wire `SessionStatus {type:'retry'}` 机器(event-reducer.ts:132-138)是 OpenCode 形状,omp-host 从不产出(死残留,07 章处置)。

### 2.6 UI 模型选择器与 thinking variant

`packages/ui/src/components/chat/ModelControls.tsx`:

- `renderModelSelector`(2151 起):providers 全列表 + favorites/recent(useModelLists);每个模型行内 ArrowLeft/Right 调整 thinking `variant`(2152-2189),行内显示 `Thinking: {label}`(2245-2262);选中后 `handleSharedModelSelect` 把 pending variant 作为 `variant` 参数发送(2202-2211)。
- variant 是 OpenCode 的 reasoning 编码(模型 `variants` record);omp 侧对应概念是 `thinkingLevel`(off/minimal/low/medium/high/xhigh/max + auto)。
- 硬编码 `build` agent 兜底(523-525/971/1291-1295)归 02 章,此处不展开。

---

## 3. 目标语义(omp/TUI 侧)

### 3.1 model roles:角色而非 agent 二分

- 10 个内置角色(<s>/config/model-roles.ts:42-53):`default`(DEFAULT)、`smol`(Fast)、`slow`(Thinking)、`vision`、`plan`(Architect)、`designer`、`commit`、`tiny`、`task`(Subtask)、`advisor`;别名 `@role`、legacy `pi/role`、`*`=default(model-roles.ts:8-15);`getKnownRoleIds` 还会并入 cycleOrder/modelRoles/modelTags 引入的自定义角色(model-roles.ts:77-91)。
- 存储:`modelRoles` record(settings-schema.ts:561);`modelRoleStorage` enum `global|project` 默认 `global`(settings-schema.ts:537-559)——global 写 `~/.omp/agent/config.yml`,project 写 `<cwd>/.omp/config.yml`(仅其 modelRoles 子树是权威项目层,<s>/config/settings.ts:1218-1242、2046-2082,scout 证据)。
- settings API:<s>/config/settings.ts — `getModelRole`(948)、`setModelRole`(892,global 层)、`setProjectModelRole`(931)、`clearProjectModelRole`(939)、`getModelRoleProvenance`(980,runtime|overlay|project|global|default)、`getModelRoleSource`(991,project|global|default)。

### 3.2 解析链(TUI 启动无显式 --model 时)

<s>/sdk.ts:1409-1473(`createAgentSession` 内):

1. 显式 `options.model` / `--model`(main.ts:967-971,scout 证据)最优先;
2. 否则**会话 transcript 里持久化的模型链**(`getRestorableSessionModels(existingSession.models, getLastModelChangeRole())`,sdk.ts:1428-1462)——含 thinking 级别还原(sdk.ts:1453 `parsedModel.thinkingLevel`);
3. 否则 settings 默认角色:`resolveModelRoleValue(settings.getModelRole("default"), allowedModels, …)`(sdk.ts:1414-1418, 1464-1472);
4. 角色未配置时走 priority 链(rolePriorityDefaults;<`advisor`→`slow`、`tiny`→`smol` 别名;`smol/slow/designer` 先继承已配置的 default;<s>/config/model-resolver.ts:961-1000,scout 证据),最终落到 `pickDefaultAvailableModel`/每 provider 目录默认(model-resolver.ts:62-76,scout 证据);
5. 全程过 `enabledModels` 白名单过滤(`resolveAllowedModels`,model-resolver.ts:1628-1637,scout 证据;空匹配即报错 sdk.ts:2493,scout 证据)。

### 3.3 setModel 的三种语义(TUI 产品行为为规格)

- **持久角色赋值**(TUI `/model` → Model Hub):`session.setModel(model, role, {selector, thinkingLevel, persist: true})`(<s>/session/agent-session.ts:6898-6908)→ model-controls.ts:205-245:校验 auth(215-217)→ provider session reset → transcript `appendModelChange("provider/id", role)`(224)→ `persist` 时 `settings.setModelRole(role, formatRoleModelValue(...))`(225-237)→ 记录 model usage → 按模型 defaultLevel 重应用 thinking(242)。Model Hub 的赋值 UI:角色条 + project/global scope 选择(`modelRoleStorage==='project'` 时出现 scope strip,<s>/modes/components/model-hub.ts:785-788, 820-851),思考级保留逻辑 model-hub.ts:790-798。
- **会话内临时切换**(TUI `alt+p` / `/switch`,session-only picker):`session.setModelTemporary(model, roleThinkingLevel)`(selector-controller.ts:742-751;model-controls.ts:254-283——**不写 settings**,transcript 记 `temporary`/ephemeral role);thinking 继承自「任何绑定到同一模型的角色显式思考级」(`resolveTemporaryModelThinkingLevel`,model-controls.ts:185-203);状态提示「Session-only model: … Use Alt+M or /model for roles.」(selector-controller.ts:750)。
- **角色轮换**(TUI `alt+m`):`session.cycleRoleModels(settings.get("cycleOrder"), direction)`(input-controller.ts:1878-1906);`getRoleModelCycle` 跳过未配置/不可用角色,`default` 角色未配置时回退当前模型(model-controls.ts:308-356,319-321),带 stale-role 防走空保护(340-353);`applyRoleModel` = setModel 不写 settings(model-controls.ts:362-367);TUI 渲染 segment track 高亮当前角色(input-controller.ts:1898-1902)。`cycleOrder` 默认 `["smol","default","slow"]`(settings-schema.ts:321, 567)。

### 3.4 thinking levels

- 设置键 `defaultThinkingLevel` enum(`THINKING_EFFORTS` + auto)默认 `"high"`(settings-schema.ts:1085-1088);`default` 角色未带显式思考级时的兜底显示也读它(`<s>`/modes/components/model-browser.ts:78-82);`thinkingBudgets` 每级 token 预算(schema:5170-5182,scout 证据)。
- `setThinkingLevel(level, persist)`(model-controls.ts:495-543):`auto` → 立即解析临时级并可能持久 `defaultThinkingLevel=auto`(496-513);具体级 → transcript `appendThinkingLevelChange`(539)+ 可选持久(540-542);发 `thinking_level_changed {thinkingLevel, configured?, resolved?}`(543;agent-session-events.ts:56-63,`resolved` 是 auto 本回合实际分辨出的 Effort)。
- 会话恢复:transcript `thinking_level_change` 条目 → `restoreThinkingLevel`(agent-session.ts:7913-7919;model-controls.ts:146-149);`configuredThinkingLevel()`(用户选择器,含 auto)vs `thinkingLevel`(生效级)分离(agent-session.ts:4375-4380)。
- 状态行模型段:模型名 + thinking 尾标(`◉ xhigh` / `⟳ auto` 待分辨→已分辨 / `off`),segments.ts:96-171(107-125)。

### 3.5 retry fallback

- 设置 `retry.fallbackChains` record:键 = 角色名 | `"provider/model"` | `"provider/*"` 通配;值 = 有序 selector 数组(<s>/session/retry-fallback-chains.ts:100-118);`default` 链自动套用到所有未单独配置的角色(137-148);`getRetryFallbackChains`(151-155)。
- 事件:`retry_fallback_applied {from, to, role}`、`retry_fallback_succeeded {model, role}`(agent-session-events.ts:48-49);TUI showWarning/showStatus(event-controller.ts:2036-2046);`model_changed`(无 payload,agent-session.ts:7272)在每次实际切换时同步发出(7257-7273 注释:setModel/retry-fallback/cycle 都汇聚于 `#setModelWithProviderSessionReset`)。
- 失败恢复:`setModel` 显式切换会 `clearActiveRetryFallback()`(model-controls.ts:222)——用户手动换模型即放弃 fallback 状态。

### 3.6 辅助模型面(裁定项)

- `enabledModels` array(默认 []):全模型白名单,支持 path-scoped 条目(settings.ts:277-330,scout 证据)。
- `modelTags`:角色名/颜色/隐藏自定义(model-roles.ts:97-113)。
- `providers.tinyModel`(标题生成等辅助任务;默认 ONLINE = tiny 角色否则 @smol;本地下载态另说):消费点 title-generator.ts:105,148(scout 证据)。

---

## 4. 差距清单

| # | 差距 | 分类 | 优先级 | 风险 |
|---|---|---|---|---|
| GAP-01 | UI 与服务端两份平行默认模型级联(useConfigStore.ts:282-399;openchamber-sessions/routes.js:121-174)取代为 omp `modelRoles.default` 解析链 | 改(删平行宇宙) | **P0** | 中:多入口必须一次切净,漏一处即「同会话不同默认」;providers 未加载时序敏感 |
| GAP-02 | 每 prompt 强制显式 model+variant(client.ts:779-805, 896-908;session-ui-store.ts:124-222)→ prompt 省略 model,会话保持自身模型;换模型走显式 setModel | 改 | **P0** | 高:乐观消息徽标、queue/multirun/git-generation 等隐性依赖 modelID 的调用方都要迁移;分阶段节奏(master 开放问题 3)已由 D6-R2 改为 capabilities 门控(`modelRoles.v1`),不再使用本地 flag(6.4) |
| GAP-03 | `GET /config` model 指针 = `availableModels()[0]`(endpoints.js:141-143)→ settings 解析的 default 角色(过 enabledModels 过滤) | 改 | **P0** | 低:单点修复,现有读方(UI 级联第 3 层)随 GAP-01 一并退役 |
| GAP-04 | 缺 `/api/omp` 模型面 RuntimeAPIs:models+roles 读、角色赋值/取消(global\|project)、会话 setModel(临时/持久)、cycleOrder 轮换;`capabilities.modelRoles.v1` 门控(D6-R2) | 建 | **P0** | 中:多目录 settings 消费已由 06 §5.1 每目录 keyed 实例注入解决(`options.settings` 注入,sdk.ts:1273-1275;R2 修订,8.3 已闭);残留上游多实例确认(06 OQ-F2) |
| GAP-05 | UI 模型选择器(ModeControls renderModelSelector + variant 槽)改造为「快速角色 + 全模型列表」双面,session-only 与角色赋值两种语义显式化;localStorage selection store 角色降级 | 改+建 | **P0** | 中:交互重设计;mobileControlsUtils、AgentManager、issue picker 等旁路调用方(08 章) |
| GAP-06 | thinking:variant(omp-host 已静默丢弃)→ `thinkingLevel`(configured 含 auto);`thinking_level_changed` 桥接与 per-turn auto 分辨显示 | 建 | **P0** | 中:auto 的 `resolved` 需事件到达后二次刷新;跨模型切换时思考级钳制语义需与 SDK 一致 |
| GAP-07 | `model_changed` 被丢弃(engine.js:611-613)→ 显式处置:registry 同步 + wire `session.updated` 重发 + `omp.model.changed` 事件 | 建(D2 强制) | **P0** | 低:engine 内已有 `session.model` 权威值 |
| GAP-08 | `retry_fallback_applied/succeeded` 被丢弃 → 事件透出 + 徽标真相(fallback 后 badge 显示实际模型);fallbackChains 只读展示 | 建 | **P1** | 低:纯增量;badge 真相依赖 GAP-07 |
| GAP-09 | sidecar `registry.meta.model` 与 SDK transcript 模型持久化并存:明确 meta.model 降级为「wire 投影缓存 + 冷启动选择器」,新 setModel 通路与 `model_changed` 均同步之;存量数据不迁移 | 改 | **P0** | 中:分歧窗口(事件丢失时投影过期)需 wire Session 刷新兜底 |
| GAP-10 | `enabledModels`/`modelRoleStorage`/`providers.tinyModel` 本地模型的范围裁定:enabledModels=P1(读+过滤),modelRoleStorage=P0(决定 PUT 落盘层),tinyModel local=P3 出域 | 决 | P1(裁定本身 P0) | 低 |
| GAP-11 | `DefaultsSettings` defaultModel/defaultVariant 字段与 `project.defaultModel` 字段的命运(D6-R12 裁决:弃用代理写 + 只读检测 + 用户确认导入,永不覆盖;见 5.8) | 删或改 | **P1** | 中:存量桌面设置(desktop.ts:124-129)与项目数据的读取兼容;涉及 i18n 六语言文案;导入护栏与审计须验证(7.1-7) |

---

## 5. 设计方案

### 5.0 总原则

- 引擎侧模型权威 = **AgentSession 实例**(session.model / thinkingLevel),持久层 = omp settings(角色)与 SDK transcript(会话内切换)。OpenChamber 不再自造任何默认模型状态。
- UI 侧模型权威 = **per-session runtime model**(来自 omp 事件 + GET 端点),localStorage selection 只作乐观缓存。
- 遵守 D1/D6:全部新端点实现于 omp-host 进程内(自有 Bun.serve 路由表,host.js:33-56;Basic auth = OPENCODE_SERVER_PASSWORD,host.js:8/36-39;子进程托管形态 omp-host-launch.js:1-7),web server 仅做既有 `/api` 透传代理(D6-R4;master §7.1 D04-1/D04-2 裁决);路径集合一律复数(D6-R3:`/api/omp/models`、`/api/omp/model-roles`、`/api/omp/sessions/{id}/model`),UI 经 RuntimeAPIs `runtimeFetch` 调用,禁止组件自行拼 URL。事件命名 `omp.<域>.<事件>`,一律经 05 章唯一 `OmpEventBus → /api/omp/events` 下发(D6-R1)——本章不定义任何通道。wire gen 不动。
- **能力门控(D6-R2)**:模型面(端点组 + 新 UI 行为)由 `GET /api/omp/capabilities` 的 feature key `modelRoles.v1` 门控,服务端裁决;UI 不用本地 feature flag(原 `ompModelRoles` 方案作废,见 6.4)。

### 5.1 GAP-01+03:默认解析归一

**engine.js 新增解析入口**(唯一真源):

```js
// engine.js
resolveDefaultModel(directory) {    // /config 指针用 boot 目录;/api/omp/models 用 ?directory=
  const settings = this.#settingsFor(directory ?? this.bootDirectory);
                                    // 06 §5.1 R2:每目录 keyed 实例(与 #materialize 注入 createAgentSession 的实例同源)
  const allowed = resolveAllowedModels(this.modelRegistry, settings, getModelMatchPreferences(settings));
  const resolved = resolveModelRoleValue(settings.getModelRole('default'), allowed, { settings });
  return resolved?.model ?? null; // { provider, id, ... }
}
```

- `endpoints.js:135-151` `configPayload` 的 `model` 字段改为 `engine.resolveDefaultModel()` 的 `provider/id`(无 directory 参数 → boot 目录上下文;未解析则省略——保持今日「无默认则无字段」的 wire 形状);`providersPayload` 一并过 `resolveAllowedModels`(enabledModels 过滤,GAP-10)。
- **删除** `resolveDefaultAgentModelSelection` 的模型半边(useConfigStore.ts:350-398)与 `resolveProviderModelSelection` 的 FALLBACK 段(useConfigStore.ts:262-270):`useConfigStore` 的 `currentProviderId/currentModelId` 初始值改为「该会话的 runtime model」(新 session 为 role default,由 GAP-04 GET 提供);`opencodeDefaultModel` 字段(useConfigStore.ts:1020-1029)与 `applyOpenCodeConfigDefaults` 中 model 灌入点删除(agent 半边归 02 章)。
- **删除** `openchamber-sessions/routes.js` 的 `resolveDefaultSelection` 模型段(routes.js:138-167):服务端发起会话只传 `agent`,**不传 model**——`engine.prompt` 无 model 即不切换(engine.js:628-636 的既有语义),新会话自然落 settings default。`fetchSelectionInputs` 中 `configUrl`/`opencodeDefaultModel`(routes.js:103-104, 117)删除。
- **备选方案对比**:(a) 保留 OC settings.defaultModel 但级联末尾并入——**否**,三处入口仍可能各自解析,违反归一;(b) OC settings.defaultModel 代理写 config.yml(见 GAP-11,仅作为迁移期兼容,不参与运行时解析);(c) 本方案(读取归一 omp)——推荐,唯一真源且对存量会话零影响(meta.model 仍生效)。

### 5.2 GAP-02:prompt 省略 model

**发送侧**:

- `client.ts:779-805` `sendMessage` 签名:`providerID`/`modelID`/`variant` 改为可选 `model?: {providerID; modelID}`(仅服务端发起的 git-generation 等仍需);`promptAsync` 请求体仅在 `model` 显式传入时携带(client.ts:896-908);`variant` 参数与穿线(session-ui-store.ts:133, 146, 187, 214)整体删除——thinking 走 5.4 的新通路。
- `routeMessage`(session-ui-store.ts:124-222)去掉 model 穿线;`sendCommand`/`shellSession` 同步(client.ts:957-998;shell 归 07 章删除,这里只清参数)。
- 乐观消息:optimistic user message 的 `model` 取自 per-session runtime model store(5.5),不再要求调用方传入(session-actions.ts:1357-1369 改读 store)。

**接收侧(engine.js prompt,619-704)**:`model` 参数保留(wire 兼容:gitApi 同步路径 endpoints.js:238-251 与外部 API 消费者),语义维持「显式切换请求」;UI 主通路不再发送。**engine 不做任何默认值兜底**——无 model 且会话无历史时,`createAgentSession` 已按 settings 解析(engine.js:443-450 已修)。

**关键正确性论据**:prompt 不带 model 后,「会话记住自己的模型」由两层保证——(1) 活跃会话:AgentSession 实例持有 model/thinkingLevel,`session.prompt` 直接用;(2) 冷启动:`#materialize` 传 `meta.model`(engine.js:448-450)或 SDK transcript 还原链(sdk.ts:1428-1462)。两条路今日均已存在,删除 UI 强制 model 只是停止干扰。

**备选方案对比**:(a) 保留每 prompt 显式 model 但 UI 保证与 session.model 一致——**否**,要求 UI 持有权威副本,恰是要消灭的分歧源;(b) prompt 带「期望模型」仅在不同时切换——即现状(engine.js:628-636),保留为 API 兼容层,UI 不再使用;(c) 本方案——推荐。

### 5.3 GAP-04:/api/omp 模型面(RuntimeAPIs)

路由注册于 omp-host 进程内新模块 `packages/web/server/lib/omp-host/omp-routes.js`,挂到 omp-host 自有 Bun.serve 路由表(`registerEndpoints(route, engine, …)` 同机制,host.js:15/33-56;认证 = omp-host Basic auth,OPENCODE_SERVER_PASSWORD,host.js:8/36-39)——一切触碰 settings/engine 的 omp-parity 路由只注册在 omp-host,web server 仅做既有 `/api` 透传代理(D6-R4;master §7.1 D04-1/D04-2)。作用域沿用 `?directory=` 查询参数。UI 一律经 RuntimeAPIs `runtimeFetch` 调用,禁止组件自行拼 URL(D6-R3)。**门控**:本端点组随 `GET /api/omp/capabilities` 暴露 feature key `modelRoles.v1`(D6-R2),key 缺失/false 时 UI 回退旧模型通路(6.4 三矩阵)。

**(1) GET /api/omp/models?directory=...** — 模型 + 角色快照:

```jsonc
{
  "models": [{
    "provider": "anthropic", "id": "claude-sonnet-4-6", "name": "Claude Sonnet 4.6",
    "reasoning": true, "contextWindow": 200000, "maxTokens": 32000,
    "thinking": { "supported": ["off","low","medium","high","xhigh","max"], "defaultLevel": "high" }
  }],
  "roles": {
    "default": {
      "configured": "anthropic/claude-sonnet-4-6",        // modelRoles 原值,null=未配置
      "resolved":  { "provider": "anthropic", "id": "claude-sonnet-4-6", "thinkingLevel": "high" },
      "explicitThinking": true,                            // 角色值里带显式思考级
      "source": "global",                                  // project|global|default|priority(解析链命中层)
      "autoSelected": false                                // priority/catalog 兜底时 true
    }
    // smol/slow/... 同构;自定义角色(getKnownRoleIds)一并返回
  },
  "roleMeta": { "default": { "tag": "DEFAULT", "name": "Default" }, "smol": { "tag": "SMOL", "name": "Fast" } },
  "cycleOrder": ["smol", "default", "slow"],
  "modelRoleStorage": "global",
  "enabledModels": [],
  "defaultThinkingLevel": "high"
}
```

实现:engine 暴露 `roleSnapshot(directory)`——在**该目录的 keyed Settings 实例**上求值(`this.#settingsFor(directory)`,06 §5.1 R2:与该目录会话注入 `createAgentSession` 的实例同源,project 层视图 = 会话实际消费值,非旁路快照);直接复用 SDK 导出的 `resolveRoleAssignments(settings, allModels, autoCandidates)`(<s>/modes/components/model-browser.ts:66-119,角色→模型+thinking+autoSelected 的权威实现)与 `getKnownRoleIds`/`getRoleInfo`(model-roles.ts:77-113);`source` 用实例的 `getModelRoleProvenance`(settings.ts:980)。

**(2) PUT /api/omp/model-roles/:role?directory=...** — 赋值/改值:

```jsonc
// 请求
{ "value": "anthropic/claude-sonnet-4-6", "scope": "global", "thinkingLevel": "high" }  // scope 缺省 = modelRoleStorage 当前值
// 202
{ "role": "default", "value": "anthropic/claude-sonnet-4-6", "source": "global", "resolved": { "provider": "anthropic", "id": "claude-sonnet-4-6" } }
```

实现:engine `assignModelRole({role, value, thinkingLevel, scope, directory})` → materialize 一个 headless 会话?**否**——角色赋值不需要会话:构造 `Model` 后调用 settings 层即可。但 `formatRoleModelValue`(model-controls.ts:228-235 内部使用)需要 registry 校验,设计为:engine 用 `modelRegistry.find(provider, id)` 解析 value,组装 role value 字符串(含 `:thinkingLevel` 后缀,格式对齐 `formatModelSelectorValue`,retry-fallback-chains.ts:122-124),写入按 scope 分层(修订轮 2,R2 评审 H3):`scope==='global'` → **boot 实例** `setModelRole(role, value)`(settings.ts:892;全局写唯一执行点,06 §5.1 R2);`scope==='project'` → 目标目录 keyed 实例上 `setProjectModelRole(role, value)`(settings.ts:931)——该实例即此目录全部会话共享的注入实例,写后 hook 在共享对象上即时触发(**同目录 live 会话即刻感知,新会话按本目录项目层解析——project 写真实生效,非仅落盘**);项目层可写仅限 modelRoles 子树、禁止 reloadForCwd 式切换(06 §5.1 R2/8.3)。对当前活跃且 `lastModelChangeRole===role` 的会话无强制切换(TUI 语义:角色赋值不改已开会话,Hub 里赋值仅写 settings;会话内应用靠轮换/重开)。

**(3) DELETE /api/omp/model-roles/:role?directory=...** — 取消赋值:`scope` 查询参数(project 时在目录 keyed 实例上 `clearProjectModelRole`,settings.ts:939——06 §5.1 R2/8.3;global 时 boot 实例 `setModelRole(role, undefined)`);`modelRoleStorage==='project'` 且未指定 scope 时按 TUI `#unassignRole` 语义删 `getModelRoleSource` 指出的层(model-hub.ts:804-813)。

**(4) POST /api/omp/sessions/{sessionID}/model?directory=...** — 会话内切换(双语义,对齐 3.3):

```jsonc
// 请求(mode 缺省 temporary —— 对齐 TUI alt+p 会话内切换为主路径)
{ "provider": "anthropic", "id": "claude-opus-4-8",
  "thinking": "high",                 // ConfiguredThinkingLevel: off|minimal|low|medium|high|xhigh|max|auto;缺省=模型 defaultLevel
  "mode": "temporary" | "role",
  "role": "default", "persist": false }  // mode==="role" 时:session.setModel(model, role, {persist, thinkingLevel})
// 200
{ "model": { "provider": "anthropic", "id": "claude-opus-4-8" },
  "thinkingLevel": "high", "configuredThinkingLevel": "high", "role": "temporary" }
```

engine 实现(新增 `async setSessionModel({sessionID, directory, ...})`):

- `temporary` → `agentSession.setModelTemporary(model, thinking)`(agent-session.ts:6911-6917;不写 settings;thinking 传 configured 级,`undefined` 时 SDK 自动用模型 defaultLevel 或继承,model-controls.ts:275-281);
- `role` → `agentSession.setModel(model, role, { thinkingLevel: concreteLevel, persist })`(agent-session.ts:6898-6908);`persist` 时 role 值落 settings(同 (2));thinking 的 `auto` 需随后 `agentSession.setThinkingLevel('auto')`(setModel 的 thinkingLevel 参数是具体级,model-controls.ts:210)。
- 成功后 engine 主动:`registry.update(dir, sid, { model: 'provider/id' })`(GAP-09)+ 发 `session.updated`(wire Session.model 刷新,engine.js:157-166)+ `omp.model.changed`(见 5.6)。
- 404 会话不存在;模型未认证 → 透传 SDK 错误文案(model-controls.ts:216「No API key for …」)。

**(5) POST /api/omp/sessions/{sessionID}/model/cycle?directory=...**:

```jsonc
{ "direction": "forward" | "backward" }   // 缺省 forward
// 200 { "role": "smol", "model": {...}, "thinkingLevel": "medium", "cycle": ["smol","default","slow"], "index": 0 }
// 409 { "error": "only-one-role-model" }   // 对齐 TUI "Only one role model available"(input-controller.ts:1887)
```

engine:`agentSession.cycleRoleModels(settings.get('cycleOrder'), direction)`(input-controller.ts:1885 同源调用);返回 `getRoleModelCycle` 的 index 供 UI 渲染 chip 轨道(对齐 TUI segment track,input-controller.ts:1898-1902)。

### 5.4 GAP-06:thinking 通路

- **UI 概念替换**:ModelControls 的 variant 槽(ModelControls.tsx:2152-2189 ArrowLeft/Right + 2245-2262 标签)改为 thinking 级槽:候选 = `['inherit','off','auto', ...model.thinking.supported]`(对齐 model-hub `#thinkingOptionsFor`:Inherit/Off/AUTO + getSupportedEfforts,model-hub.ts:816-818);数据源 `GET /api/omp/models` 的 `models[].thinking`(从 SDK `Model.thinking` 投影,`getSupportedEfforts`)。
- **应用时机**:选中模型时把 thinking 一并 POST 到 5.3(4)(`thinking` 字段);会话内单独调级 = 同端点只改 thinking(model 不变时 engine 走 `setThinkingLevel` 而非 setModel)。engine `setSessionModel` 增加「model 与 session.model 相同 → 仅 `agentSession.setThinkingLevel(configured)`」短路。
- **显示**:`thinking_level_changed` 事件(payload agent-session-events.ts:56-63)→ `omp.thinking.changed { sessionID, thinkingLevel, configured?, resolved? }`(公开名登记 05 章注册表,D6-R1;通道本体归 05);UI ThinkingPill(`packages/ui/src/components/session/ThinkingPill.tsx`)显示 `auto` 待分辨(⟳)/已分辨(auto → high,用 `resolved`)/具体级/off,对齐 TUI segments.ts:107-125 语义。
- **删除**:`variant` 全链(UI 参数、engine 透传忽略、`DefaultsSettings` defaultVariant 字段——见 GAP-11)。
- **transcript/恢复**:engine 无需自建持久化——SDK 已把 thinking 变更写 transcript(`appendThinkingLevelChange`,agent-session.ts:6762)并在恢复时还原(`restoreThinkingLevel`,agent-session.ts:7913-7919;model-controls.ts:146-149)。

### 5.5 GAP-05:UI 模型面改造

**ModelControls `renderModelSelector` 双层结构**:

1. **快速角色轨**(顶部 chips):`cycleOrder` 角色(smol/default/slow),每 chip 显示角色名 + 解析出的模型短名 + thinking 级;点击 = `POST .../model/cycle`(或直接 applyRoleModel 语义);当前命中角色高亮填充——像素级对齐 TUI alt+m 行为(input-controller.ts:1898-1902)与 `@smol` quick-role 提及。数据源 `GET /api/omp/models` 的 `roles` + `cycleOrder`。
2. **全模型列表**:providers 树 + favorites/recent 保留(纯 UI 资产);选中 = `POST .../model {mode:'temporary'}`(对齐 TUI alt+p session-only,selector-controller.ts:742-751);行内新增「设为角色」次级动作(行尾 role-strip 简化版:列出常用角色 + scope,提交 = 5.3(2) PUT)——对齐 Model Hub 角色条(model-hub.ts:820-851)。

**当前会话模型显示**:新增 `packages/ui/src/stores/useSessionModelStore.ts`(per directoryKey+sessionId):

```ts
{ model: {provider, id} | null, thinkingLevel?: string, configuredThinkingLevel?: string,
  role?: string, fallbackActive?: {from: string, to: string} | null }
```

来源:**不单设 per-session GET**(初始值取 wire `Session.model`,engine.js:157-166)+ `omp.model.changed`/`omp.thinking.changed` 事件刷新;fallback 态经 `omp.fallback.applied/succeeded`。ModelControls 触发器显示它而非 useConfigStore 的 currentModelId;ChatMessage 徽标兜底链改为 `message.info` → sessionModelStore(删除 localStorage 兜底,ChatMessage.tsx:281-363 相应段)。**重连对账不自设规则(D6-R12 resync)**:任何事件 gap 触发 05 章统一 bootstrap 顺序中 modes/model 段的权威 GET(capabilities → session snapshot → modes/model → dialogs → agents/jobs/queue → transcript 增量;05 章为唯一权威)——模型/思考态由 session snapshot + `GET /api/omp/models` 恢复,断流不是空状态(00-MASTER D2)。

**localStorage `contextStore` 降级**(contextStore.ts:40-49):`saveSessionModelSelection`/`saveAgentModelVariantForSession` 停写(服务器已权威);`getAgentModelForSession` 保留一个迁移窗口作为乐观缓存读取,6 周后随 07 章大扫除删除。**新会话草稿**:不再预解析默认模型——composer 头显示 role default(来自 models 快照),直到首个 message.updated 带回真实模型。

- **旁路调用方**(迁移归 08 章,契约在此):GitHubIssuePickerDialog.tsx:436-438、NewWorktreeDialog.tsx:487-489、AgentManagerView 多模型并行、scheduled tasks——一律改为:创建会话后需要非默认模型时 `POST /api/omp/sessions/{id}/model`,prompt 不带 model。

### 5.6 GAP-07+08+09:事件处置与投影缓存(REVISED:通道设计删除,改引 05 注册表)

**REVISED(修订轮,D6-R1)**:本节原「复用 wire `/event` SSE 承载 `omp.*`」的通道设计**作废**。按 00-MASTER D1/D6-R1,全部 omp 原生事件经 05 章唯一 `OmpEventBus → /api/omp/events` SSE 通道下发(envelope、事件 ID、durable/volatile、directory 作用域、`Last-Event-ID` 重放、schema 版本、快照端点、reducer 以 05 章注册表为唯一权威);本章只**按名声明** 4 个公开事件与 payload 语义,并按下表登记进 05 章注册表(SDK source → 公开名 → payload → producer → durable/volatile → 作用域 → 快照端点 → reducer):

| SDK 事件(`<s>`/session/agent-session-events.ts) | 公开事件名 | payload 草图 | engine 侧处置(producer) | 对账快照端点 |
|---|---|---|---|---|
| `model_changed`(:7272;payload-less,引擎内 session.model 即新值) | `omp.model.changed` | `{ sessionID, model:{provider,id}, thinkingLevel, role? }` | registry meta.model 同步(GAP-09)+ wire `session.updated` 重发(Session.model 刷新,engine.js:157-166) | wire session snapshot;`GET /api/omp/models`(角色面) |
| `thinking_level_changed`(:56-63) | `omp.thinking.changed` | `{ sessionID, thinkingLevel, configured?, resolved? }` | 无 wire 动作(thinking 非 wire 概念) | 同上 |
| `retry_fallback_applied`(:48) | `omp.fallback.applied` | `{ sessionID, from, to, role }` | 无额外 session.updated——`model_changed` 随后从 `#setModelWithProviderSessionReset` 发出(agent-session.ts:7271-7273) | wire session snapshot |
| `retry_fallback_succeeded`(:49) | `omp.fallback.succeeded` | `{ sessionID, model, role }` | 同上 | 同上 |

engine `#handleEngineEvent`(engine.js:506-614)新增四个 case(每个都是 D2 要求的显式处置;禁止无理由 `default:` 静默丢弃):

```js
case 'model_changed': {                       // payload-less;引擎内 session.model 即新值
  const model = modelSelector(hostSession.agentSession.model);
  this.registry.update(directory, sessionId, { model });          // GAP-09 同步
  const info = this.#wireSessionFromLive(hostSession);
  this.bus.emit('session.updated', { sessionID: sessionId, info }, directory);  // 既有 wire 面
  // omp 面:发布 omp.model.changed(payload 见上表)——经 05 章 OmpEventBus,
  // 发布机制/envelope/作用域/重放以 05 章注册表为准,本章不设计通道(D6-R1)。
  return;
}
case 'thinking_level_changed':                // → omp.thinking.changed(05 章注册表)
case 'retry_fallback_applied':                // → omp.fallback.applied
case 'retry_fallback_succeeded':              // → omp.fallback.succeeded
```

- **GAP-09 语义**:registry `meta.model` 保留但身份降级为「wire Session.model 投影 + 冷启动选择器」。一致性规则:engine 侧**所有**模型变化(用户 setModel、compat prompt 切换 engine.js:628-636、retry fallback、cycle)都会经 `model_changed` → 上面的同步代码写回 registry。分歧兜底:`getSession`/`listSessions` 对 live 会话始终用 `#wireSessionFromLive`(engine.js:333-341 已如此),冷会话才读 registry——SDK transcript 是终极真源,registry 过期最多影响一次冷启动选择器,且该选择器与 transcript 模型一致时无副作用。事件 gap 的对账不自设规则,走 05 章 resync 矩阵(D6-R12,见 5.5)。
- **徽标真相(GAP-08)**:`omp.fallback.applied {from, to}` → sessionModelStore 设 `fallbackActive` → ModelControls 触发器与 ChatMessage 徽标渲染 `model(to)` + ⚠ fallback 角标,直到 `omp.fallback.succeeded`(回切成功,event-controller.ts:2045 语义)或用户显式 setModel(clearActiveRetryFallback,model-controls.ts:222)。
- **fallbackChains 展示**:`GET /api/omp/models` 追加 `"fallbackChains": getRetryFallbackChains(settings)`(展开后的链,retry-fallback-chains.ts:151-155);UI 在模型详情 tooltip 里只读显示角色→链。**编辑器 P2**(06 章 model tab;本域不建编辑 UI)。

**通道裁决(REVISED,D6-R1)**:本章原「(a) wire Event union /(b) 独立 SSE /(c) 复用 `/event` + `omp.*`」三方案对比**删除**——D6-R1 已冻结唯一通道(05 章 `OmpEventBus → /api/omp/events`),本章 4 个事件仅登记名称与 payload;重连对账走 05 章统一 bootstrap/resync(D6-R12)。

### 5.7 GAP-10:范围裁定

| 项 | 裁定 | 级别 | 说明 |
|---|---|---|---|
| `enabledModels` | **入域**:models 快照透出原值 + providers/roles 解析全程过滤(5.1/5.3 已含);编辑 UI 归 06 章 | P1 | 过滤实现直接复用 `resolveAllowedModels`(model-resolver.ts:1628-1637);空匹配 → models 快照 `models: []` + `"enabledModelsWarning"` 字段(对齐 sdk.ts:2493 报错语义) |
| `modelRoleStorage` | **入域 P0**:PUT (2) 的 scope 缺省值 + GET (1) 透出;编辑 UI 归 06 章 | P0(读)/P1(编辑) | project 作用域经每目录 keyed 实例对会话真实生效(06 §5.1 R2;R2 评审 H3 前的「会话不消费项目层」问题已闭,8.3) |
| `providers.tinyModel` 本地模型(下载/设备/dtype) | **出域**:角色列表透出 `tiny` 角色即可;本地模型管理(下载进度、tinyModelDevice)归 06 章(settings 面)与 08 章(产品面)再议 | P3 | 标题生成已由引擎 `maybeStartTitleGeneration`(engine.js:691)自动走 tiny 链,无需 UI 干预 |

### 5.8 GAP-11:DefaultsSettings / project.defaultModel 字段命运(REVISED:R12 冻结迁移策略)

**REVISED(修订轮,D6-R12)**:原「弃用 vs 代理写」的开放取舍已冻结为——**弃用代理写;存量 `defaultModel` = 只读检测 + 用户确认显式导入;永不自动写 `modelRoles.default`;永不覆盖已有角色;原值保留 + 审计记录**。**冲突裁决**:06 章 §5.8/§6.2 的「role 缺失时自动写 `modelRoles.default`」设计与本裁决相反,按 00-MASTER D3(概念映射第 2 行)+ D6-R12 **以本章立场为准,06 章须对齐修订**(交叉引用:06 章设置体系;评审中危「defaultModel 存量迁移策略相反」条)。

1. **P0(随 GAP-01)**:所有**读取点**删除——级联退役后 `settings.defaultModel`/`settingsDefaultVariant`/`project.defaultModel` 不再参与任何解析(useConfigStore.ts:297-311 字段与 355-364 消费;openchamber-sessions/routes.js:140-144;DefaultsSettings.tsx 加载段 62-118)。`GET /api/config/settings` 响应保留字段(存量可见),`openchamber-control/service.js:190-193` 保留(桌面控制面向后兼容)。
2. **P1(随 06 章设置页改造)**:DefaultsSettings 的「Default Model/Default Thinking/Default Agent」三件套替换为「模型角色」区(默认角色赋值 + defaultThinkingLevel,数据 = 5.3(1)/(2));`project.defaultModel` 字段从 ProjectIdentityFields(ProjectIdentityFields.tsx:77-82)移除,项目页提示改指 `.omp/config.yml` 项目角色(modelRoleStorage=project 时)。settings.json 中的存量数据**原地保留**(回滚 + 导入审计需要),仅停止常规读写。

**导入流(R12 唯一迁移通道)**:

1. **只读检测**:`GET /api/omp/models` 快照附带 `legacyDefaults: { defaultModel: "provider/model" | null }`——仅当 OC settings.json `defaultModel` 存在且可解析时非 null;检测过程不写任何 omp 配置。
2. **用户确认**:检测非 null 且 `roles.default.configured === null` 时,DefaultsSettings 显示「检测到 OpenChamber 默认模型 X — 导入为 default 角色?」;确认后 `PUT /api/omp/model-roles/default`(5.3(2),scope 取 `modelRoleStorage` 当前值)。
3. **护栏(永不覆盖)**:`roles.default.configured !== null` → 不显示导入入口,端点侧再校验拒绝(409 `role-already-configured`);导入幂等,重复确认不再改写。
4. **审计与原值**:导入成功后 host 写审计记录到 OC settings.json(`migrations.legacyDefaultModelImport: { originalValue, importedRole: "default", scope, at }`);OC `defaultModel` 原值**原地保留不改写**(回滚基线,双保险)。

**代理写/自动迁移:否决(D6-R12)**——自动写会改变 omp TUI 行为与回滚基线(评审中危同名条);06 章自动写方案作废,须按本节对齐。



## 6. 迁移与兼容

### 6.1 存量会话选择(sidecar registry `meta.model`)

- **不迁移、不清写**:每项目 `openchamber-session-meta.json` 里的 `model` 条目继续被 `#materialize` 尊重(engine.js:448-450)——存量会话打开仍是用户当时选的模型(等价于 omp transcript 持久化的行为,体验无缝)。
- 唯一新写入点收敛为:compat prompt 显式切换(既有,engine.js:634)、新 setModel 端点、`model_changed` 同步(5.6)。新会话不再有创建期 pinning(openchamber-sessions 路由停止发 model 后,首 prompt 也不带 model → registry 无 `model` 键 → wire Session.model 省略 → UI 用 role default 显示,首个 message.updated 带回真实模型)。

### 6.2 存量设置与桌面数据

- OC settings.json `defaultModel`/`defaultVariant`/`smallModelUseDefault`:原地保留、停止读取(5.8);`lib/desktop.ts:124-129` 桌面字段保留定义一版(旧客户端兼容),桌面不再写入。
- omp `~/.omp/agent/config.yml`:P0 阶段只读 + 显式 PUT 写入;**永不自动迁移/自动写**(D6-R12:只读检测 + 用户确认显式导入,见 5.8;06 章「role 缺失自动写 `modelRoles.default`」设计按 master D3 作废,以本章为准)。
- wire 兼容:`POST /session/:id/prompt_async` 等仍接受 `model` 字段(engine.js:619 兼容层);`GET /config` 形状不变(仅 `model` 值变真);旧版 UI(桌面缓存的旧 bundle)对新 engine 依然可用——它们继续发显式 model,engine 照常切换。

### 6.3 并发会话

- 活跃多会话各自持模型(实例级,无冲突);`model_changed` per-session 同步 registry,写盘原子(registry.js 既有 temp+rename)。
- 角色 PUT 的 settings 生效面是**目录级**(修订轮 2,R2 评审 H3:每目录 keyed 实例,06 §5.1):`scope=global` 经 boot 实例写全局层,全部目录的新会话生效(非 boot 目录 live 会话的全局写跨实例传播缺口登记于 06 §5.1.7b);`scope=project` 只改该目录实例的项目层,其他目录不受影响——`GET /api/omp/models` 快照按 `?directory=` 返回各自项目层视图(= 该目录会话消费值)。赋值后**已开的会话不自动切换**(TUI 语义,5.3(2)——会话模型是 AgentSession 实例属性,settings 变更只影响后续解析)。

### 6.4 阶段开关与回滚(REVISED:本地 flag → capabilities,R2)

**REVISED(修订轮,D6-R2)**:原 UI 本地 feature flag `ompModelRoles`(useFeatureFlagsStore)方案**作废**。模型面由 `GET /api/omp/capabilities` 的 feature key **`modelRoles.v1`** 门控,**服务端裁决**(key 随 P0-b 端点组在 omp-host 上线);UI 在 capabilities 不可达或无该 key 时自动回退旧模型通路,不引入任何本地开关(master §7.3 的 flag 表述由 D6 取代)。

| 阶段 | 门控 | 回滚 |
|---|---|---|
| P0-a(GAP-03/07/09):engine `/config` 指针 + 事件 case + registry 同步 | 无门控(行为修正) | git revert;registry 已有 `model` 键不被破坏 |
| P0-b(GAP-01/02/04/05/06):RuntimeAPIs + UI 切换 | `capabilities.modelRoles.v1`(D6-R2) | 服务端关闭 key(或 engine 回退)→ UI 自动回旧级联 + 显式 model 发送;新端点无害残留(07 章前不删) |
| P1(GAP-08/10/11):fallback 徽标、enabledModels 过滤、字段弃用 + R12 导入流 | 徽标纯增量(随 P0-b key);enabledModels 过滤随 omp 配置存在性自然启用;导入流见 5.8 | 字段数据未删(原值 + 审计保留),恢复读取代码即可 |

**版本矩阵(R2 三矩阵)**:

- **新 UI + 旧 engine**:capabilities 不可达或无 `modelRoles.v1` → UI 走旧级联 + 每 prompt 显式 model(旧通路保留至 07 章删除);
- **旧 UI + 新 engine**:旧 bundle 不读 capabilities、继续发显式 model → engine compat 层照常切换(6.2 wire 兼容;engine.js:628-636 语义保留);
- **relay 旧 bundle**:等价旧 UI 矩阵——capabilities 由服务端裁决,relay 不参与、不引入本地 flag。

回滚安全性论证:全链路无破坏性写(不动 omp transcript、不删 settings.json 字段、registry 仅追加/更新既有键、R12 导入有审计且原值保留);上述三矩阵下均无死路。

---

## 7. 验证方案(设计;执行归实施 PR)

### 7.1 单元/集成(omp-host,bun:test;server JS `node --check`)

1. `configPayload` model 指针:配置 `modelRoles.default` → 指针 = 该模型;未配置 → priority/catalog 默认;`enabledModels` 排除后 → 指针落到允许集;无可用模型 → 无 `model` 字段(endpoints.js:135-151 改造点)。
2. `engine.prompt` 无 model:不调 `setModel`(mock AgentSession 断言);带 model 且不同 → setModel + registry 更新(既有行为回归);variant 字段被忽略(请求含 variant 无异常)。
3. `setSessionModel`:temporary 不写 settings(spy settings.setModelRole 未调用)、transcript 无 role 持久化(对齐 model-controls.ts:254-283);role+persist 写入且 scope 正确(global vs project);model 相同仅改 thinking → 只走 setThinkingLevel;未认证模型 → 4xx 带 SDK 错误文案。
4. `model_changed` → registry meta.model 更新 + `session.updated`(Session.model = 新值)+ `omp.model.changed`(含 role);`retry_fallback_applied/succeeded` → `omp.fallback.applied`/`omp.fallback.succeeded`;`thinking_level_changed` → `omp.thinking.changed` 原样透传 configured/resolved(envelope/重放断言以 05 章注册表为准,此处只测 engine producer 侧 payload)。
5. 存量兼容:构造带 `meta.model` 的 registry → `#materialize` 用该模型;`meta.model` 指向不可用模型 → 回落 settings 默认(engine.js:448-450 语义回归)。
6. roleSnapshot:角色配置/未配置/auto 兜底三种 `resolved`+`source`;cycleOrder 顺序与 `getRoleModelCycle` index 一致性;**按目录求值(修订轮 2)**:A/B 各设不同 project default 角色 → `roleSnapshot(A)`/`roleSnapshot(B)` 各自返回本目录 `source:"project"` 值,且 `#materialize` 注入的 `options.settings` 为各自 keyed 实例(注入断言归 06 §7.1.5,此处断言 roleSnapshot 与该实例同源)。
7. R12 导入护栏与 capabilities 门控:`legacyDefaults` 检测只读(不产生任何 omp 写);default 角色已 configured 时导入 PUT → 409 且配置不变;导入成功 → 审计记录写入、OC `defaultModel` 原值不变;`modelRoles.v1` 随端点组上线/摘除,capabilities 响应形状不变式。

### 7.2 E2E(dev 栈 5180 UI / 3902 server,浏览器驱动)

1. **默认角色生效**:PUT default 角色 → 新建会话发首条消息 → 会话头与 assistant 徽标显示该模型;TUI 对照:同 config 下 `omp` 新会话状态行模型一致(sdk.ts:1414-1418)。
2. **prompt 省略 model**:Network 面板断言 prompt_async 请求体无 `model`/`variant`;会话中途用 picker 切模型(POST model)→ 下一条 prompt 仍无 model → 徽标随 `omp.model.changed` 更新。
3. **快速角色轨**:连续 cycle → chip 高亮循环 smol→default→slow,模型随角色 resolved 值变化(对照 TUI alt+m,input-controller.ts:1878-1906)。
4. **thinking**:模型详情选 `auto` → ThinkingPill 显示 auto 待分辨;跑一轮后显示 `auto → <resolved>`;选 `off` → 后续 assistant 无 reasoning part。TUI 对照:segments.ts:107-125 的三态。
5. **fallback 徽标真相**:配置 fallbackChains(如 default → ["@smol"]),用失效凭证触发主模型失败 → 徽标出现 fallback 角标并显示 fallback 模型;fallback succeeded 后回切显示。TUI 对照:event-controller.ts:2036-2046 两相提示。
6. **存量会话**:迁移前创建的会话(带 meta.model)→ 打开后模型不变;`/config` 指针 ≠ 该会话模型(互不干扰)。
7. **降级矩阵(R2)**:capabilities 无 `modelRoles.v1`(模拟旧 engine 或 key 关闭)→ UI 自动回退旧级联并恢复显式 model 发送,旧级联 UI 完整可用。
8. **project-scope 会话级生效(修订轮 2,R2 H3)**:双目录 A/B——在 B 下 `PUT /api/omp/model-roles/default`(scope=project)→ B 新建会话发首条消息,会话头/徽标 = B 项目角色解析的模型(会话级消费,非仅设置页显示);A 的既有会话与 A 新建会话仍按 A(全局层或 A 项目层)解析;TUI 对照:在 B 目录运行 `omp` 新会话状态行模型一致(sdk.ts:1414-1418)。前置:`settings.projectScopes.v1`(06 §1.3)。

### 7.3 TUI 行为对照清单(规格说明引用)

| 行为 | TUI 证据 | OpenChamber 对应 |
|---|---|---|
| 新会话默认 = default 角色 | sdk.ts:1414-1418, 1464-1472 | GAP-01/03 |
| 会话内临时切换不写 settings | selector-controller.ts:742-751;model-controls.ts:254-283 | POST model mode=temporary |
| 角色赋值含 scope 选择 | model-hub.ts:785-788, 820-851 | PUT model-roles scope |
| cycle 高亮轨道 | input-controller.ts:1898-1902 | 快速角色轨 chips |
| 模型段 thinking 三态 | segments.ts:96-171 | ThinkingPill |
| fallback 两相提示 | event-controller.ts:2036-2046 | omp.fallback.applied/succeeded 徽标 |
| 模型切换全汇 model_changed | agent-session.ts:7257-7273 | engine case(5.6) |

---

## 8. 开放问题

1. ~~**DefaultsSettings/project.defaultModel:弃用(5.8 推荐)还是代理写 + 一次性导入?**~~ **已裁决(00-MASTER D6-R12,修订轮)**:弃用代理写;只读检测 + 用户确认显式导入;永不覆盖已有角色;原值保留 + 审计(5.8)。06 章自动写方案按 master D3 作废。剩余仅导入入口的文案/摆放(UI 细节,06 章)。
2. **角色 PUT 是否联动当前会话**:TUI Hub 赋值不改已开会话(5.3(2));OpenChamber 用户可能期望「改 default 立即影响当前会话」。建议保持 TUI 语义(会话模型是实例属性),UI 在赋值成功 toast 里提示「新会话生效;当前会话可在模型菜单切换」。
3. ~~**settings 单例的 project 作用域 vs host 多目录**~~ **已解决(修订轮 2,R2 评审 H3)**:`createAgentSession` 已有注入口——`options.settings ?? options.settingsManager ?? Settings.init({ cwd, agentDir })`(**sdk.ts:1273-1275**;类型 sdk.ts:554-560)。omp-host `#materialize`(engine.js:436,调用点 :454-473)注入每目录 keyed Settings 实例(06 §5.1 REVISED R2;派生原语 `cloneForCwd`,settings.ts:607-625,`loadIsolated` :439-442 为备选)——该目录会话消费「全局层 + 本目录 `.omp/config.yml` 项目层」,`GET models?directory=` 与 project-scope PUT/DELETE 走同一实例(5.3(1)-(3)),UI 展示值 = 会话消费值。**原「GET 侧直读文件、engine 按目录写辅助」的旁路建议作废**。D6-R6 中「项目层只读用 loadReadOnly 旁路」的表述由 06 §5.1 R2 取代(总纲回写登记于 06 §8 OQ-F1)。**残留(非本章阻塞)**:上游多实例并存确认、全局写跨实例传播缺口(06 §5.1.7 / OQ-F2——非 boot 目录 live 会话不热见全局写,新会话可见)。
4. **`@smol` 等角色别名提及**:composer 文本里 `@smol`(quick role)是否随本章进入 @-mention 语法(当前 @ 是 file/agent 提及,triggers.ts:83-91)?建议 P2,与 02 章 agent mention 重构合并设计,本章只保证 cycle 轨可用。
5. **fallback 徽标的持久化**:fallback 状态在重连/刷新后是否保留?建议:仅事件驱动(sessionModelStore 内存态),冷加载经 wire Session.model(GAP-09 同步)显示实际模型,不追溯 fallback 标记(简化,对齐 TUI「提示即焚」语义 event-controller.ts:2039)。

---

## 9. 依赖

**前置(硬依赖):**

- 05 章:唯一事件通道 `OmpEventBus → /api/omp/events`(D6-R1:envelope/事件 ID/durable-volatile/作用域/`Last-Event-ID` 重放/schema 版本唯一权威)+ 统一 bootstrap/resync 顺序(D2/D6-R12:capabilities → session snapshot → modes/model → dialogs → agents/jobs/queue → transcript 增量;模型/思考态对账入口,见 5.5)——GAP-06/07/08 的下发前提;本章 5.6 的 case 实现可先落(engine 侧),事件面 UI 消费等通道。capabilities 端点(D4 P0/D6-R2)与事件 schema 版本须同批交付(`modelRoles.v1` key 依赖)。
- 02 章:build/plan agent 二分删除决定 `engine.prompt` 的 agent 参数与 `meta.agent` 命运;本章 GAP-02 的 client.ts 改造与 02 章 sendMessage agent 参数改造**同文件同函数**,必须协同排期(建议同一 PR 序列)。

**前置(软依赖):**

- 06 章:DefaultsSettings/设置页承载角色编辑 UI(GAP-11 P1);enabledModels/modelRoleStorage 编辑面(5.7)。**P0 项 GAP-F10 实例拓扑(§5.1 REVISED R2:每目录 keyed Settings 实例注入)须与本章 GAP-04 同批落地**——5.3(1)-(3) 的目录感知读写与会话级 project 生效(7.2.8)以 `#settingsFor(directory)` 为前提;`settings.projectScopes.v1` capability 由 06 章声明、本章 E2E 消费。

**后置(消费本章契约):**

- 07 章:`variant` 参数、`opencodeDefaultModel` 字段、`SessionStatus retry` 死机器、`/config` OpenCode 语义残余的最终删除以本章 P0 落地为前提。
- 08 章:multirun/AgentManager、GitHubIssuePickerDialog.tsx:436-438、NewWorktreeDialog.tsx:487-489、scheduled tasks 的模型输入迁移,消费 5.3(4) 端点契约。
