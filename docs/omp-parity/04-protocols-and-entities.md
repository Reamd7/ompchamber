# 04 协议与实体:URI schemes / 会话树 / Agent Hub / jobs / IRC / drafts / artifacts

状态:设计稿(域 D;修订版 R2-2,2026-08-20 —— 落位评审裁决 R1/R2/R3/R4/R6/R7/R8/R12 与二轮评审 R2-H2(URI 隔离终裁)/R2-M5(historical 行)/R2-M6(MCP 降权)/R2-M11(scheme×读写矩阵),总纲 D6)
日期基线:2026-08-19
遵守:`00-MASTER.md` D1–D6(与早期章节文本冲突时以 D6 修订轮冻结契约为准)。行为规格以 omp TUI 源码为准,证据一律 file:line。
路径缩写:`SDK` = `C:/Users/reamd/.bun/install/global/node_modules/@oh-my-pi/pi-coding-agent/src`(安装副本,与 TUI 源 `oh-my-pi/packages/coding-agent/src` 一一对应);`engine` = `packages/web/server/lib/omp-host/engine.js`;`endpoints` = `packages/web/server/lib/omp-host/endpoints.js`。其余 OpenChamber 路径相对仓库根。

---

## 1. 域概述与边界

本章管四类"omp 原生协议实体"进入 OpenChamber 的全链路设计:

1. **15 个 URI scheme**(`local/ agent/ history/ artifact/ skill/ memory/ rule/ omp/ issue/ pr/ ssh/ vault/ security/ mcp/ xd`):宿主侧解析端点、UI 链接渲染与查看器、编辑器补全。**P1 = `local://` 一个**(读 + 写);`agent/ history/ artifact` 的宿主解析 capability 本轮恒 OFF——其隔离依赖上游 per-resolve context,落地前不开放(R2-H2,§5.2.3);其余 11 个 P2 按 scheme × 读/写门控透传,其中 `ssh/ vault/ security/ mcp` 的**读**能力同样须威胁评审后才可开启(R2-M11,§5.0)。
2. **会话树**(session entry tree):`getTree`/label/leaf 只读暴露、`navigateTree` 变更通道(含 summarize/`branch_summary`/ask 两阶段 re-answer)、web 树选择器 UI、与现有 revert/unrevert(线性 marker)模型的 reconcile。
3. **Agent Hub 与 parked 生命周期**:跨会话注册表聚合快照 + 事件、parked 复活/终止/聊天、逐字节增量 transcript 尾读协议、subagent HUD 行。资源名 `/api/omp/agent-runs`(R3;agent 定义 CRUD 归 Ch02 `/api/omp/agent-definitions`,拆分契约见 §5.5.1)。
4. **async jobs**:`getAsyncJobSnapshot` → `/api/omp/jobs`(capability 门控,R12)、async-result 交付的流内呈现、`/jobs` 视图。以及 P2 杂项:IRC 卡与 hub 工具消息面、drafts、artifacts 目录浏览(`local://` 范围,R2-H2)、HTML 导出。MCP 可执行端点**本轮不建设**(定义权移出本章,专项轮次承载,§5.7.5 存根,R2-M6)。

**明确不管(out of scope):**

- 事件通道与 reducer 的通用机制(customType 流内元素的映射框架、断线对账、`#handleEngineEvent` default 分支清零的完整处置表)→ **Ch05**。本章只定义 agents/tree/jobs 三类事件的 payload 形状与 producer,通道/命名/注册表一律引用 Ch05(R1);本章无自有 SSE。
- ask 对话框桥本体(对话框组件、恢复语义)→ **Ch03**。本章只消费其结果(`reanswerAskResult`)。
- 审批弹窗、model roles、模式(plan/goal/vibe)、设置面 → Ch03/Ch01/Ch02/Ch06;**agent 定义(发现/CRUD/选择器)→ Ch02** `/api/omp/agent-definitions`(R3 拆分),本章只管运行实例;**设置读取的权威模型 → Ch06**(R6:全局实例唯一真值,本章仅只读消费,无 per-directory 设置写)。
- OpenCode 残留删除清单(含 `/session/{id}/share` 501 桩的最终移除节奏)→ **Ch07**;本章仅登记接口交接点。
- OpenChamber 原创面(WorkStatusPanel、multirun、AgentManager)的输入迁移与共存 → **Ch08**;本章提供数据源(agent-run 行)。
- `omp /share`(加密 sealed 上传,`export/share.ts:486-511`)≠ OpenCode cloud share;按总纲 D3 属"另议(低优)",本章只在 §5.7 登记,不做设计。

**与其他章的接口契约:** 本章产出的事件(`omp.agents.updated` / `omp.tree.updated` / `omp.jobs.updated`,命名与注册表以 Ch05 为准,R1)与 `/api/omp/agent-runs|jobs|sessions/{id}/tree|uri/*` 端点组是 Ch05(事件接入)、Ch08(HUD/面板数据源)的直接上游;`reopenAsk` 载荷由 Ch03 的 ask 桥渲染;feature 门控经 `GET /api/omp/capabilities`(R2,§5.0)。

---

## 2. 现状分析(OpenChamber 侧)

### 2.1 omp-host 引擎骨架

- 一个 `HostSession` per OpenChamber session id,冷读走 SessionManager 投影、首个 prompt 物化完整 `AgentSession`(engine:1-9, `#materialize` engine:436-504)。物化时**每个会话注入私有 `AgentRegistry`**(engine:454-473,registry 注释在 459-462:"One registry per session: the SDK's global registry admits a single 'Main' agent per process generation")。这是 commit 0cd3585 的落点,也是本章 §5.5 聚合设计的起点。
- `#handleEngineEvent` 仅投影 `message_start/update/end`、`tool_execution_start/end`、`agent_start/end`、`todo_reminder`、`notice`;其余 default 丢弃(engine:506-614,default 在 611-612)。无任何 registry/树/job 事件出口。
- 空闲回收:`#sweepIdleSessions` 超过 12 个 live 会话后直接 `#disposeSession` + `sessions.delete`(engine:32-33, 122-133)。**没有 parked 中间态**——sessionFile 仍在盘上,任何 live 操作可重新物化,但没有任何可见性/复活入口。
- revert/unrevert:已经是树原语(`manager.branch(messageID)` / `branch(previousLeaf)` / `resetLeaf()`),但对外呈现为线性 marker——把 `{messageID, previousLeaf}` 存进 sidecar registry,Session 上只暴露 `revert.messageID`(engine:754-789)。fork 用 `SessionManager.forkFrom`(engine:723-748)。
- 端点面(`endpoints:157-547`):OpenCode wire 全集。session 树、URI、hub、jobs 均无暴露;`/session/{id}/share` 返回 501(endpoints:326-331)。
- 事件面:`WireEventBus`(events.js:10-58)单调 id + 2048 重放环 + 按 directory 定向;SSE 出口在 host.js:72-74(`/event`、`/global/event`)。web server 经 `directory-ws-bridge` 转发到 UI WS(directory-ws-bridge.js:73-85),`openchamber:*` 合成事件命名空间已存在(protocol.test.js:35-42;heartbeat 实例 directory-ws-bridge.js:59)。

### 2.2 网络与代理链

web server 的 `/api` 处理顺序 = feature 路由先注册(index.js:1704 `featureRoutesRuntime.registerRoutes`)→ `app.use('/api', apiProxy)` 全量兜底转发到上游 omp-host(proxy.js:828-846)。因此**在 omp-host 进程内新增的任何 `/api/omp/*` 路由无需改 web server 即可达 UI**;鉴权沿用 spawner 注入的 `OPENCODE_SERVER_PASSWORD` Basic auth(host.js:36-38、68-70)。UI 侧 `runtimeFetch` 只解析 `/api`、`/auth`、`/health` 前缀(runtime-fetch.ts:10-12),relay/E2EE 模式下同路径自动走隧道(runtime-fetch.ts:155-203)——新端点组零成本继承。

### 2.3 UI 侧现状

- `RuntimeAPIs` 形状(types.ts:1225-1241):terminal/git/files/settings/permissions/notifications/tools/editor/vscode/push/github/clientAuth。无 omp 组。
- chat markdown 渲染器只有两种链接处理:外链拦截(`useExternalLinkInteractions`,MarkdownRendererImpl.tsx:58-102,`data-openchamber-file-link` 优先放行 :90-92)与文件引用注解(`annotateFileLinks` 管线,MarkdownRendererImpl.tsx:495+,背后 `/api/fs/stat` 探测 :380)。**对 `local://`、`history://` 等内部 scheme 完全无感知**——它们在 tool 结果文本里是裸文本,不可点击。
- `/undo` `/redo`(ChatInput.tsx:1127-1136 → `handleSlashUndo/handleSlashRedo`,session-ui-store.ts:1583-1626)基于 revert marker:`revertToMessage`(session-actions.ts:1803-1900,乐观置 `session.revert.messageID` + prefill 输入框)与 `unrevertSession`(:1944-1968)。sync store 以 `session.revert` 派生可见时间线(session-actions.ts:1312-1341)——**线性单指针,非树**。
- 既有可复用面:文件查看/编辑器(openContextPreview,MarkdownRendererImpl.tsx:1040)、UI 本地草稿持久化(chatDraftPersistence.ts)、markdown 导出(lib/exportSession.ts)、WorkStatusPanel subagents 节(现显示 permission blockers,Ch08 迁移)。

### 2.4 关键架构事实(设计约束)

omp-host 与 TUI 的进程拓扑不同:**一个 omp-host 进程同时服务多个 directory 的多个 top-level 会话**,而 TUI 一个进程 = 一个 main 树。由此推出三个已验证的约束:

- **C1(router 全局态):** URI handlers 通过 `AgentRegistry.global()` 与模块级 `extraArtifactsDirs` 解析(`artifactsDirsFromRegistry` registry-helpers.ts:36-48;local:// 兜底 local-protocol.ts:468-481;SDK 注释 sdk.ts:1795-1800)。omp-host 的会话都注册在**私有** registry 里,全局 registry 为空 → `agent:// artifact:// history://` 的 live 解析在 omp-host 进程内**现状必然落空**,只有 `registerArtifactsDir(dir)`(registry-helpers.ts:12-19)注册过的目录参与磁盘扫描。local:// 在会话内正确(每会话派生 `localProtocolOptions` 经 `toolSession` 线程传递,sdk.ts:1811-1819),但宿主侧(无调用会话)解析需要显式提供 context。
- **C2(jobs 单管理器):** `AsyncJobManager` 只由进程内**第一个** top-level 会话创建;后续 top-level 会话 `scopedAsyncJobManager = undefined`(sdk.ts:1599-1616),`AgentSession` 无兜底(agent-session.ts:1045 `config.asyncJobManager ?? config.ownedAsyncJobManager`)。omp-host 多会话并存的进程里,只有最先物化的会话有 async jobs;且 SDK 未导出 `asyncJobManager` 注入项、`AsyncJobManager` 也不在包出口(index.ts 无 `async/` 行)。
- **C3(深路径可用):** 包无 `exports` map(package.json `main: ./src/index.ts`),`@oh-my-pi/pi-coding-agent/src/internal-urls/router.js` 等深路径可被 omp-host 直接 import(engine 已 import 包根,同进程 Bun 解析 TS 源)。

---

## 3. 目标语义(omp/TUI 侧)

### 3.1 InternalUrlRouter:15 scheme 一个进程级路由

- 注册:`InternalUrlRouter.instance()` 单例,constructor 注册全部 15 个 handler(router.ts:37-54;scheme 清单见文件头注释 :1-7)。`resolve(input, context?) → InternalResource`、`write(input, content, context?)`、`complete(scheme, query, context?)`(router.ts:107-152)。未注册 scheme 回落 MCP resource handler(router.ts:118-135)。
- handler 契约:`resolve/write?/complete?/immutable`(types.ts:153-195)。`InternalResource = {url, content, contentType: 'text/markdown'|'application/json'|'text/plain', size?, sourcePath?, notes?, immutable?, isDirectory?}`(types.ts:16-43);`sourcePath` 注释明确"for debugging, not exposed to agent"(:25)。
- `ResolveContext = {cwd?, signal?, localProtocolOptions?, skills?, xd?, skipDirectoryListing?, pathOnly?}`(types.ts:89-130)。`localProtocolOptions` 的解析顺序 = 调用方 context → 进程 override(`LocalProtocolHandler.setOverride`)→ 全局 registry 首个 `main`(local-protocol.ts:451-481,注释明确多会话宿主必须线程传递,#1608)。
- 模型面:system prompt 固化清单(system-prompt.md:51-71)——这是模型产出这些链接的源头(task 工具结果附 "…transcript at history://<AgentId>",task/index.ts:1076-1088)。**OpenChamber 无需改 system prompt 即可收到这些链接**,只需让它们可解析、可点击。

### 3.2 重点 scheme 精确语义(local:// 与 registry 依赖三 scheme)

> 范围注记(R2-H2):本节四 scheme 的语义均为"上游解阻后的开放规格"登记——P1 实际开放的宿主解析只有 `local://`;`agent/ history/ artifact` 依赖进程级扫描集合(§2.4 C1),其宿主解析 capability 在上游提供 per-resolve `ResolveContext.artifactsDirs` 前恒 OFF(§5.2.3、§8 第 8 条)。

**local://**(`internal-urls/local-protocol.ts`)
- 会话级可变草稿区,root = `<artifactsDir>/local`;Windows 路径 ≥180 字符时回退 `%TEMP%/omp-local/<sessionId>`(WINDOWS_LOCAL_ROOT_MAX_CHARS,~:88;shortLocalRoot ~:80)。
- `local://` 裸形式 = markdown 目录列表(buildListing :262-283);`local://<path>` 文件(二进制嗅检、1MiB 文本上限,buildLargeLocalTextResource :109-119)。root 解析顺序见 §3.1;**containment 由 handler 内 `resolveLocalTarget` 保证**(相对 root 归一)。
- 生产者:write 工具、eval 内核(`PI_EVAL_LOCAL_ROOTS` :326-333)、plan 模式 plan.md(plan-mode/plan-files.ts 按 mtime 列 `*plan.md`)、非视觉图片回退。会话迁移时 `copyLocalArtifacts`(:252-276)把 plans/scratch 带到新会话(plan-approve、handoff)。

**agent://**(`internal-urls/agent-protocol.ts`)
- subagent 持久输出(`<id>.md`),immutable。扫描**所有已注册 artifacts 目录**的 `*.md`(registry-helpers.ts:36-48,含 depth-2 子目录 `sessionFile.slice(0,-6)` 规则)。
- 形式:`agent://<id>` 整文件;`agent://<Parent>/<Child>` → `Parent.Child.md` 层级跳;`agent://<id>/<path>` 或 `?q=` = jq 风格 JSON 抽取(json-query.ts)。id 分配由 `AgentOutputManager` 去重(`Anna`、`Anna-2`;嵌套 `Anna.Bob`,task/output-manager.ts)。eval prelude 自动返回 `agent://<id>` 句柄。

**artifact://**(`internal-urls/artifact-protocol.ts`)
- 数字 id 溢出产物 `<N>.<toolType>.log`(session/artifacts.ts 的 ArtifactManager;resume 时 `#scanExistingIds` 防 id 撞车;子代理 adopt 父 manager → 一树一 id 空间)。immutable,8MiB inline 上限 + selector 提示(`artifact://3:1-3000`)。
- **多会话钉扎**:id 是每会话计数器,解析先钉到调用会话的 artifactsDir(artifact-protocol.ts:38-55,pinnedDir 从 `context.localProtocolOptions.getArtifactsDir()` 取)——`artifact://3` 语义 = "本会话的 3 号"。
- 生产者:bash/read 截断溢出、shake regions、async job 结果 >12k 字符(session/async-job-delivery.ts:26-27)。

**history://**(`internal-urls/history-protocol.ts`)
- 裸形式 = index 表 `| id | status | kind | parent | last activity |`,合并 registry refs + 盘上 `.jsonl`("on disk" 行;`sessionFilesFromDisk` registry-helpers.ts:64-92,depth≤8,`__advisor*` 排除)。advisor refs 全域隐藏。
- `history://<id>` = transcript markdown,来源优先级:live 内存消息 → parked sessionFile JSONL(只读)→ 磁盘扫描。消费者:task 工具结果(task/index.ts:1076-1088)与 Agent Hub 聊天视图(同一 JSONL)。

### 3.3 P2 scheme 速览(透传即可,语义不重建)

`memory://`(memory_summary + mnemopi 行,YAML frontmatter)、`skill://`(SKILL.md,plugin-root containment)、`rule://`(活跃 TTSR 规则)、`omp://`(内嵌 harness 文档)、`issue:// pr://`(gh + SQLite 缓存,`?state&limit&author&label&comments`)、`ssh://`(ControlMaster 远程读写,1MiB 上限)、`vault://`(Obsidian CLI,`vault.enabled` 门控)、`security://`(OMP scan store,门控)、`mcp://`(MCP resource 包装 + 外来 scheme 兜底)、`xd://`(虚拟工具设备管道,session-bound dispatcher,`xd://resolve|reject` 特例)。全部细节见 `internal-urls/` 对应文件;OpenChamber 的义务只是把它们送进 router 并渲染结果(§5.2)。

### 3.4 会话树

- **数据模型**:append-only JSONL entry `{id, parentId, timestamp, type…}`;类型含 `message / compaction / branch_summary / label / title_change / mode_change / model_change / thinking_level_change / ttsr_injection / session_init / custom / custom_message…`(session-entries.ts;`BranchSummaryEntry` :116-120、`LabelEntry` :154-158、`SessionTreeNode {entry, children, label?}` :289-295)。`SessionEntryIndex` 维护 parent→children 邻接、resolved labels(append-only label entries,`labelsInEffect()`)、active leaf、usage(session-manager.ts:226-356)。核心原语:`getTree()`(:2450-2452)、`branch(id)`(:2473 附近)、`resetLeaf()`(:2478)、`branchWithSummary(id, summary, details, fromExtension)`(:2500-2503)、`appendLabelChange(targetId, label|undefined)`(:2397-2400)、`discardEntryDurably`(:2478-2496,marker `discarded-entry-branch`)。
- **navigateTree**(`AgentSession.navigateTree(targetId, {summarize, customInstructions, allowAskReopen, reanswerAskResult})`,agent-session.ts:8273-8601):
  1. flush bash;目标 entry 必须存在(:8329-8332)。
  2. 已在目标 = no-op,除非 ask-reopen 中途(:8338-8347,issue #5642)。
  3. **ask 两阶段**:目标为 `ask` toolResult 且 `allowAskReopen` 且未带 `reanswerAskResult` → 不变更,返回 `{reopenAsk: {toolCallId, questions}}` 交调用方重开选择器(:8354-8375);第二阶段带 `reanswerAskResult` 回来时,在 `targetEntry.parentId` 下**追加兄弟 toolResult 新枝**(:8497-8518),原答案枝保持可达,置 `askReanswerCommitted`。
  4. summarize:收集 old-leaf→common-ancestor 的被弃 entry(:8377-8395),发 `session_before_tree`(可取消,:8411-8427),可选 LLM 摘要(:8429-8470);summary **挂在目标位**(`branchWithSummary(newLeafId, …)`,:8527-8546)。
  5. 落叶规则:user message → leaf = `parentId` + **editor 预填该消息文本/图片**(:8481-8486);custom_message(非 skill-prompt)同(:8487-8496);其余(assistant/toolResult/skill-prompt)→ leaf 落在选中节点(:8519-8525)。
  6. 重建 LLM context / checkpoints / advisor / todos(:8553-8560);返回 `{cancelled, editorText, editorImages, summaryEntry, sessionContext, reopenAsk, askReanswerCommitted}`;`askReanswerCommitted` 后由调用方先重建 UI 再 `resumeAfterAskReanswer()`(:8603-8614,issue #6483)。
- **TUI 交互**:`/tree` 或 `app.session.tree` 键或空编辑器双 Esc(默认 `doubleEscapeAction="tree"`)开 TreeSelectorComponent(tree-selector.ts:984+):ASCII 树、活动路径加亮、过滤模式 default/no-tools/user-only/labeled-only/all(Alt+D/T/U/L/A)、fuzzy 搜索、Shift+L 编辑 label、Enter 选择、Shift+Enter summarize-and-switch;summary 选择(No summary / Summarize / Custom prompt)受 `branchSummary.enabled` 门控(默认 false)。树选择器里 mode_change 显示 `[mode: plan]`。相关命令 `/branch /fork /new /fresh /clear /drop /resume /handoff /rename /move /session delete`(slash-commands/builtin-session.ts:443-477、builtin-lifecycle.ts:74-236)、`/btw`(:238-247 + btw-controller + `AgentSession.branchFromBtw` agent-session.ts:8182)。HTML 导出渲染同一棵树并支持 `?leafId=&targetId=` 深链(export/html/template.js:36-54、960-976)。

### 3.5 Agent Hub / parked / subagent HUD

- **AgentRef**:`{id, displayName, kind: main|sub|advisor, parentId?, status, session|null, sessionFile, createdAt, lastActivity, activity?, history?}`,`status: running|idle|parked|aborted`;`session === null` 当且仅当 parked/aborted;`aborted` 终态(tombstone `<file>.jsonl.tombstone`)(agent-registry.ts:18-41、72-87)。RegistryEvent:`registered | status_changed | metadata_changed | removed`(:91-97)。`setActivity` 心跳喂 roster,不发包(:223-231)。
- **parked 生命周期**:idle→parked TTL(`task.idleParkMs`,settings-schema.ts:4674)由 `AgentLifecycleManager` 管理;park 释放 session 保留 ref+sessionFile,`ensureLive` 复活(ref-bound CAS `registerIfAvailable`,agent-registry.ts:170-174);aborted 终态不可逆(:196-198)。transcript 布局:`<artifactsDir>/<AgentId>.jsonl`,孙代 `<artifactsDir>/<AgentId>/<AgentId>.<Child>.jsonl`(registry-helpers.ts:57-62)。
- **TUI Hub**:`/agents`、alt+a、ctrl+s、空编辑器双击 ←(config/keybindings.ts:185-195)。AgentHubOverlayComponent(agent-hub.ts:147+):按 STATUS_ORDER running>idle>parked>aborted 排序的树表、parentId 缩进、每行 metrics + context gauge;键位 j/k/wheel 选择、PgUp/PgDn 详情、Enter = 聚焦 live agent 或开 chat view、`t` 切视图、`r` 复活(仅 parked,:1082-1086;advisor 只读 :1077-1081)、`x` 终止(:1103-1128)、Esc 关闭;footer 提示行(:566)。
- **chat view**:全屏 transcript viewer 增量尾读 JSONL + 输入行;提交即复活再 prompt/steer(agent-hub.ts:9-12)。
- **增量尾读协议(AgentHubRemote)**:`readTranscript(id, fromByte) → {text, newSize, error?} | null`(agent-hub.ts:92-106)。客户端规则(agent-transcript-viewer.ts:349-414):text 只取到最后一个完整换行(不完整行由下次拼接,:392-395);`newSize < fromByte` = 宿主 transcript 轮换/截断 → 清空本地全量重拉(:376-387);轮询 250ms(:63);本地路径用 sentinel 检测 rewrite(:255-304)。
- **subagent HUD**:编辑器上方锚定 "Subagents" 块,列 detached running spawns `• Id ⟨role⟩: description`,行数上限 + "… N more running — open Agent Hub for full list"(interactive-mode.ts:455-507)。

### 3.6 async jobs

- `AsyncJob = {id, type: 'bash'|'task', status: running|completed|failed|cancelled, startTime, label, resultText?, errorText?, latestDetails?, ownerId?, agentId?, queued?}`(job-manager.ts:32-63)。进程级单例,maxRunning 15、5min 保留、owner-scope 投递 sink(500ms→30s 重试)、无 sink 完成走 dead-letter(job-manager.ts:804-808 per scout)。
- `AsyncJobSnapshot = {running, recent, delivery}`;`AsyncJobSnapshotItem = Pick<AsyncJob, 'id'|'type'|'status'|'label'|'startTime'>`(agent-session-types.ts:64-72);`AgentSession.getAsyncJobSnapshot({recentLimit})`(agent-session.ts:1808-1811,owner 过滤)。
- 投递:settled 结果 → owner 会话的 yield queue 排出 `async-result` CustomMessage(customType `async-result`,display:true),>12k 字符溢出 artifact + 4k 预览,epoch 字段跨 `/new`/handoff 丢帧(session/async-job-delivery.ts:26-88)。
- TUI:`/jobs` 渲染 Running/Recent 两表(command-controller.ts:486-521,`[id] type (status) — duration` + label);transcript 里 "Background job completed [type] <jobId> (duration)" 行(transcript-render-helpers.ts:26-32)、`BACKGROUND_TAN_DISPATCH` pill(background-tan-message);detached task/tan/vibe job 同时是 Hub 行(`AsyncJob.agentId` 关联 ref)。
- **多会话限制 = C2**(§2.4):仅首个 top-level 会话有 job manager。

### 3.7 IRC / hub 工具 / drafts / artifacts(P2 简述)

- IRC:进程级 IrcBus,IrcMessage `{id, from, to, body, ts, replyTo?}`,fire-and-forget + 回执 `injected|woken|revived|failed`(parked 收件人经 lifecycle 复活);transcript 卡 `IRC ← From` / `IRC → To`(autoreply/meta "auto"/"reply"/age,messaging.ts:472-563)。
- hub 工具:`send/wait/inbox/list/jobs/cancel` + 进程监督 `start/ps/logs/stop/restart/describe`(tools/hub/index.ts:1-141)——模型面,web 无需重建 UI,但卡要渲染。
- drafts:`SessionManager.saveDraft(text)` 写 `<artifactsDir>/draft.txt`,空文本 unlink,`.draft-only-session` marker(session-manager.ts:1985-2010);`consumeDraft()` 一次性读+删(:2012-2046);TUI Ctrl+D 退出链保存、下次 resume 恢复进编辑器(interactive-mode.ts:1190-1197)。
- artifacts 目录 = sessionFile 去 `.jsonl`(session-manager.ts:106-109):`<N>.<tool>.log`、`<AgentId>.md`、`<AgentId>.jsonl`、`local/`、`draft.txt`、嵌套目录。HTML 导出 = 单文件 standalone,含 subSessions + 树深链(export/html/index.ts:192-296)。

---

## 4. 差距清单

| 编号 | 差距 | 分类 | 优先级 | 风险 | 对应设计 |
|---|---|---|---|---|---|
| GAP-01 | 无 URI 解析/补全/写端点;模型产出的大量 `scheme://` 链接在 web 侧是死文本 | 建 | **P1**(仅 `local://` 读+写;agent/history/artifact 延后至上游,R2-H2) | 中(写边界与作用域校验,R8) | §5.2.1 |
| GAP-02 | chat markdown 渲染器不识别内部 scheme;无 URI 查看器(P1 仅提升 capabilities 内 scheme,其余保持纯文本,R2-H2) | 建 | **P1** | 中(渲染管线是热点路径,需限流) | §5.2.5 |
| GAP-03 | 私有 registry ≠ router 全局态;曾设计的"互斥 + 进程级集合切换"与"稳态并集注册"均构成跨目录泄露(C1;R7 初裁 → **R2-H2 终裁:capability OFF + local:// 会话钉扎 + 零全局变更**);resolve 响应曾设计回显绝对 `sourcePath` | 建/改 | **P1**(安全阻断项) | 高(跨目录数据泄露,评审高危) | §5.2.3-5.2.4 |
| GAP-04 | 会话树不可见:`getTree`/labels/leaf 无只读暴露 | 建 | **P1** | 低(纯投影) | §5.4.1 |
| GAP-05 | `navigateTree` 变更通道缺失(summarize/branch_summary/ask 两阶段/editor 预填) | 建 | **P1** | 高(变更 LLM context,须防流中竞态) | §5.4.2-5.4.4 |
| GAP-06 | revert/unrevert 线性 marker 与树模型冲突;`/undo /redo` 语义要重定基 | 改 | **P1** | 中(存量客户端依赖 `session.revert`) | §5.4.5 |
| GAP-07 | Agent Hub 无数据面:注册表聚合、快照、事件(资源名 `/api/omp/agent-runs`,定义面归 Ch02,R3) | 建 | **P2** | 中(多目录聚合口径,总纲开放问题 4) | §5.5.1 |
| GAP-08 | 无 parked 生命周期:sweeper 直接删;无 revive/kill/chat;磁盘行须区分 `historical`(仅 transcript)与 `parked`(进程内可复活)(R2-M5) | 建 | **P2** | 中(复活即重物化,复用 `#materialize`;重启后全磁盘行 = historical,descriptor 持久化另案 P2) | §5.5.2 |
| GAP-09 | 无 agent transcript 增量尾读端点 | 建 | **P2** | 低(协议照抄 AgentHubRemote) | §5.5.3 |
| GAP-10 | 无 subagent HUD 行 | 建 | **P2** | 低 | §5.5.4 |
| GAP-11 | jobs 无快照端点;async-result/BASE_TAN pill 无流内呈现;无 `/jobs` 视图;C2 单 manager 限制 → capability 门控 + ownerSessionID(R12) | 建 | **P2**(能力门控) | 中(上游限制需登记) | §5.6 |
| GAP-12 | IRC 卡(irc:incoming/autoreply/relay)与 hub 工具回执无渲染 | 建(卡)/留(工具) | **P2** | 低(依赖 Ch05 customType 通道) | §5.7.1 |
| GAP-13 | drafts 只有 UI 本地持久化,无服务端 `draft.txt` 跨端语义 | 建 | **P2** | 低 | §5.7.2 |
| GAP-14 | artifacts 目录无浏览面;无 HTML 导出(`/share` 明确不做) | 建 | **P2** | 低 | §5.7.3 |
| GAP-15 | OpenCode share 残留端点(`/session/{id}/share` 501 桩)移交删除 | 删 | P3 | 低 | 交接 Ch07 |
| GAP-16 | P2 scheme(11 个)端点透传 + 查看器适配(门控:`vault.enabled`、`security.enabled` 等按设置关闭);**写能力(ssh/vault/xd)须威胁评审后 P2+(R8);读能力(ssh/vault/security/mcp)亦须同级评审 + 审计 + 大小/速率 + principal 作用域(R2-M11)** | 建 | P2 | 中(远端读写威胁面) | §5.2、§5.7.4 |
| GAP-17 | MCP 可执行管理端点:本轮不建设,定义权移出本章(专项轮次);UI 只读、开关禁用是长期稳态(R12 → R2-M6) | 登记 | 本轮不建设 | 低(07/08 已按只读门控) | §5.7.5(存根) |

---

## 5. 设计方案

### 5.0 端点组架构(GAP-01/04/05/07/09/11 公共底座)

**决策 D04-1(R4 重申):所有 omp-parity 端点实现在 omp-host 进程内(endpoints.js 新 `/api/omp/*` 组),经既有 `/api` 代理兜底直达 UI,不新增 web server 路由;omp-host Basic auth 之下,web server 仅做透传代理、不承载任何 omp-parity 逻辑。**
理由:引擎状态(私有 registry、SessionManager、router 全局态)都在 omp-host 进程内;代理链(proxy.js:846)、Basic auth(host.js:36-38)、`x-opencode-directory` 目录头(engine 现有约定)、relay 隧道(runtime-fetch.ts:155-203)全部免费继承。备选"web server 直连引擎 IPC"需要新进程间通道,收益为零,否决。备选"塞进 OpenCode wire 路径组"违反总纲 D1(wire 不扩张),否决。**本章无自有 SSE/事件出口**:事件一律经 05 章 `OmpEventBus → /api/omp/events` 唯一通道(R1);SSE 与 `Last-Event-ID` 的鉴权穿透要求由 05 章 + 总纲 R4 承载。

**决策 D04-2:目录作用域 = 现有 `directory` 语义(query/body 字段 + 目录头),不用 agentDir 全局。** 回答总纲开放问题 1 的本章部分:agent-runs/jobs/tree 全部以 directory 过滤(一个 omp-host 进程服务多目录,聚合面按项目切分);URI 解析按 §5.2.3:scheme capability 门控(P1 仅 `local://`)+ 会话钉扎。agentDir 全局面(跨项目 `history://` 索引)**不开 UI 端点**——跨目录合并视图已按 R7 取消、R2-H2 进一步关闭宿主解析(§5.2.3),模型侧 router 行为的收窄登记 §8.8。

**RuntimeAPIs 扩展**(types.ts:1225-1241 增加 `omp` 键;实现走 `runtimeFetch('/api/omp/...')`,模式照抄 FilesAPI;URL 一律由本接口拼装,组件不得自行拼 URL,R3):

```ts
export interface OmpResourceToken { id: string; expiresAt: number }   // §5.2.4
export interface OmpUriResource {   // = SDK InternalResource(types.ts:16-43)减 sourcePath(R7)
  url: string; content: string;
  contentType: 'text/markdown' | 'application/json' | 'text/plain';
  size?: number; notes?: string[];
  immutable?: boolean; isDirectory?: boolean;
  token?: OmpResourceToken;         // open/download 能力(§5.2.4)
}
export interface OmpAgentRun { /* §5.5.1 */ }
export interface OmpJobsResponse { available: boolean; ownerSessionID: string | null; snapshot?: OmpJobSnapshot }
export interface OmpParityAPI {
  resolveUri(u: string, opts?: { directory?: string; sessionID?: string; pathOnly?: boolean }): Promise<OmpUriResource>;   // 受 capabilities.uri.schemes 门控(P1=['local'],R2-H2)
  completeUri(scheme: string, query: string, opts?: { directory?: string }): Promise<Array<{ value: string; label?: string; description?: string }>>;
  writeUri(u: string, content: string, opts: { directory: string; sessionID: string }): Promise<{ ok: true }>;  // P1 仅 local://(R8)
  readUriToken(id: string, opts: { directory: string; download?: boolean; range?: string }): Promise<Response>;  // §5.2.4
  getTree(sessionID: string, directory: string): Promise<OmpTreeSnapshot>;
  navigateTree(sessionID: string, directory: string, req: OmpNavigateRequest): Promise<OmpNavigateResult>;
  setLabel(sessionID: string, directory: string, targetId: string, label?: string): Promise<{ ok: true }>;
  listAgentRuns(directory: string): Promise<{ agentRuns: OmpAgentRun[]; generatedAt: number }>;
  agentRunAction(sessionID: string, agentId: string, directory: string, action: { kind: 'revive' } | { kind: 'kill' } | { kind: 'chat'; text: string; mode: 'prompt' | 'steer' }): Promise<{ ok: boolean; status?: string; error?: string }>;
  readAgentRunTranscript(sessionID: string, agentId: string, directory: string, fromByte?: number): Promise<{ text: string; newSize: number } | null>;
  getJobs(sessionID: string, directory: string, recentLimit?: number): Promise<OmpJobsResponse>;   // §5.6(R12:501 → {available:false})
  getDraft(sessionID: string, directory: string): Promise<{ text: string | null }>;   // GET 即 consume
  saveDraft(sessionID: string, directory: string, text: string): Promise<{ ok: true }>;
  listArtifacts(sessionID: string, directory: string, sub?: string): Promise<OmpUriResource>; // local:// 列表复用
  exportHtml(sessionID: string, directory: string): Promise<Blob>;
}
```

**事件(R1 修订:本章无自有通道,只做 producer)**:以下事件经 05 章 `OmpEventBus → /api/omp/events` 唯一 SSE 通道下发;envelope/事件 ID/durable-volatile/作用域/`Last-Event-ID` 重放/缺口 resync 以 05 章事件注册表为唯一权威,本章只定义 payload 形状与 producer(注册表条目在 05):

```
omp.agents.updated  { directory, agentRuns: OmpAgentRun[], revision }   // AgentRunsAggregator,250ms 合并快照
omp.tree.updated    { directory, sessionID, leafId, kind: 'navigate'|'label'|'summary', entryId? }
omp.jobs.updated    { directory, sessionID, snapshot: OmpJobSnapshot }  // 仅 capabilities.jobs=true(§5.6)
```

(`omp.queue.changed` 若 08 章方案 B 落地,同样经 05 注册表;本章不定义。)`openchamber:omp-*` 并行命名空间**废止**(评审 R1),wire `/event` 只承载既有 wire 事件。快照式(agents/jobs)而非增量式:行数小(≤ 数十)、权威状态在服务端、重连对账只需"最后一次快照全量替换",符合 sync-state-invariants 的权威状态原则;tree 用轻量 delta + 客户端重拉 `getTree`。

**capabilities 门控(R2;R2-M11 修订为 scheme × 读/写矩阵)**:本章 feature 键(协商端点 `GET /api/omp/capabilities` 的全局定义见总纲 D6-R2,本章只消费):`uri.schemes`(string[] —— 宿主侧 READ 解析允许的 scheme 清单;**P1 = `['local']`**,`agent/history/artifact` 不在列,R2-H2)、`uri.write.<scheme>`(逐 scheme 写开关;P1 仅 `uri.write.local=true`;`uri.write.ssh|vault|xd` 威胁评审前恒 false,§5.2.1)、`tree.read`、`tree.navigate`(P1)、`agentRuns`、`jobs`(上游注入前恒 false,R12)、`drafts`、`artifacts`、`export`。**读侧评审规则(R2-M11)**:`ssh/vault/security/mcp` 四 scheme 的 READ 纳入 `uri.schemes` 前,须通过与写能力同级的威胁评审(§5.2.1 维度 + 读侧特有:宿主凭据与内网可达面、外部敏感内容向浏览器暴露、审计日志、大小/速率限制、principal 作用域)——P2 默认不透传这四个 scheme 的读。服务端裁决;UI 不得以本地 flag 判定(§6.1)。

### 5.2 URI 桥(GAP-01/03/16;R7/R8 修订)

#### 5.2.1 端点与载荷

```
GET  /api/omp/uri/resolve?u=<urlencoded>&directory=<dir>[&sessionID=<sid>][&pathOnly=1]
GET  /api/omp/uri/complete?scheme=<s>&query=<q>&directory=<dir>
POST /api/omp/uri/write     { u, content, directory, sessionID }
GET  /api/omp/uri/tokens/{id} 与 /api/omp/uri/tokens/{id}/content     (§5.2.4)
```

- `resolve` 实现:深路径 import(C3)`import { InternalUrlRouter } from '@oh-my-pi/pi-coding-agent/src/internal-urls/router.js'`,按 §5.2.3 构造 `ResolveContext`(P1 仅 `localProtocolOptions` 会话钉扎一项,**不触碰任何 SDK 全局态**,R2-H2)后调 `router.resolve(u, ctx)`;响应 = `InternalResource` 直译 + 资源令牌,**剥离 `sourcePath`**(R7,§5.2.4)。`pathOnly` 透传给大 artifact 的 selector-only 解析(types.ts:129)。
- scheme 门禁(两层):① scheme ∈ `capabilities.uri.schemes`,否则 `501 {error:'scheme-not-enabled', scheme}`(P1 对 `agent://`/`history://`/`artifact://`/`mcp://` 等一律如此,R2-H2/R2-M11);② `router.canResolve(u)`(router.ts:90-97)通过才处理;`file:`/`http:`/`https:` 永不进入(router 层已排除,`#route` 的 MCP 兜底只收未注册非标准 scheme,router.ts:118-135)。未知 scheme 返回 `404 {error:'unknown-scheme'}`,不暴露 MCP 兜底读取(`mcp://` 的读开启另受 R2-M11 评审门控,§5.7.4)。
- `complete` = `router.complete(scheme, query, ctx)`(router.ts:112-116),供编辑器 `@` 补全之外新增 `scheme://` 前缀补全(仅 completionSchemes 白名单 router.ts:99-106 **∩ `capabilities.uri.schemes`**,R2-H2)。
- **写边界(R8,REVISED)**:P1 仅开放 `local://` 写,且**限同 directory + 该 directory 名下会话**——`sessionID` 必填(`local://` root 由该会话 localProtocolOptions 钉扎,§5.2.3)。其余:
  - `ssh://` / `vault://` / `xd://` 写 → `501 {error:'write-not-enabled', scheme}`(capabilities `uri.write.ssh|vault|xd` 在威胁评审通过前恒 false);评审维度 = 远程横向移动面(ssh:服务端进程凭据与可达内网主机)、第三方系统完整性(vault:Obsidian 库合并冲突/不可逆写)、副作用执行(xd:write 即触发真实工具设备调用,等同任意工具执行)、审计与速率、containment/回收、capability 独立开关与撤销、prompt-injection 暴露面(模型可自行产出写链接)。
  - 无 write 钩子的其余 scheme(types.ts:173-181 之外)→ `405 {error:'not-writable'}`。
  - 越界:local:// 跨 directory → `403 {error:'scope'}`;缺 sessionID → `400 {error:'session-required'}`。
- **尺寸/超时**:resolve 响应 ≤8MiB(artifact inline 上限内),超限时 handler 自身已返回 selector 提示文案(artifact-protocol 语义),端点不再截断;统一 15s 超时。

#### 5.2.2 鉴权与路径安全(path traversal)

- 鉴权(R4):本章全部路由只注册在 omp-host 进程内,经进程级 Basic auth(host.js:36-38,spawner 注入),与全部 wire 流量同级;web server 仅做既有 `/api` 透传代理,不承载任何 omp-parity 逻辑;web server 侧另有 ui-auth 会话层;直连/桌面/VS Code/relay 的认证头转换表按总纲 R4 全局方案执行。**不引入 per-scheme 额外鉴权**——信任模型与 `/api/fs/*`(全盘文件读写)一致;目录隔离不靠鉴权分层,靠 scheme capability 关闭 + `local://` 会话钉扎(§5.2.3,R2-H2)。
- **traversal 防线 = 复用 handler 内部 containment,端点层绝不自己做路径拼接**。P1 实际可达面只有 `local://`:`resolveLocalTarget` 在 handler 内做 root 归一(local-protocol.ts;1MiB 文本上限/二进制拒绝 :109-119),Windows 短 root 回退规则同源。其余 scheme 的 handler containment(`skill://` 的 `validateRelativePath` + plugin-root `containRoot` 约束、`artifact://` 的纯数字 id 强制(artifact-protocol.ts:27-36)、`agent://` 的 jq 查询不触 FS 路径)保持原义,待各自 capability 开启时生效,端点层不重复实现。
- 端点层附加校验:`u` 长度 ≤2KiB;`u` 解析后 `rawPathname` 含 `..` 段即透传给 handler 拒绝并回显其错误文案——**不提前改写**以免破坏 `rawPathname` 保留语义(types.ts:70-72)。
- `sourcePath`:**不回显**(R7,REVISED)。SDK `InternalResource.sourcePath` 在端点序列化前剥离,以 §5.2.4 受鉴权令牌替代;`OmpUriResource` 类型不含该字段。(原"直译保留/与 /api/fs 等权"的论证作废——多目录 omp-host 下绝对路径本身就是跨项目信息泄露。)

#### 5.2.3 解析作用域与隔离(GAP-03 核心,回答 C1;R7 初裁 → R2-H2 终裁)

**决策 D04-3(R2-H2 REVISED,取代 R1/R7 版"目录允许集 + 进程级互斥切换"):宿主侧对 `agent://`、`history://`、`artifact://` 的解析 capability 本轮恒 OFF(`capabilities.uri.schemes` 不含这三个 scheme);UriBridge 的 P1 范围收窄为 `local://`——经 `ResolveContext.localProtocolOptions` 会话钉扎解析,零 SDK 全局态变更。上一版"互斥 + registerArtifactsDir 集合切换"设计废止:互斥只能串行化 UriBridge 自身请求,拦不住并发 AgentSession 的模型工具在切换窗口内读 router——目录 A 的模型调用可能命中目录 B 的允许集,构成反向跨目录泄露(R2 评审高危;凡以共享可变全局集合作隔离机制的设计一律不得作为安全边界)。**

- **为什么 `local://` 可以安全保留**:local-protocol 的 root 解析顺序第一位就是 caller-supplied `context.localProtocolOptions`(local-protocol.ts:451-470,注释明确多会话宿主必须线程传递),全程不读 `AgentRegistry.global()`、不经 `registerArtifactsDir`、不设 `LocalProtocolHandler.setOverride`;containment 在 handler 内(`resolveLocalTarget` 相对 root 归一)。无 options 时 handler 显式抛 "No session - local:// unavailable"(local-protocol.ts:484-488)→ 端点 409,失败模式封闭。
- **会话钉扎(P1 协议)**:`local://` resolve/write 一律要求 `sessionID`(R8 写边界规则推广到读:root 是会话私有的,"取该目录最近活跃会话"会静默读到别的会话草稿区)——从该会话 SessionManager 取 `{getArtifactsDir, getSessionId}` 构造 `localProtocolOptions`(冷会话先 `SessionManager.open` 只读,不物化);缺 `sessionID` → `400 {error:'session-required'}`。
- **稳态注册同样废止**:R1 版曾把活跃会话的 artifactsDir 维持在 SDK 进程级集合(`registerArtifactsDir`,registry-helpers.ts:12-19)以支持会话内模型工具解析。该稳态并集本身就是泄露面——无需任何切换窗口,任一目录会话的模型工具都能看到所有活跃目录的集合——一并取消(R2-H2)。代价:会话内模型工具对 `agent://`/`history://`/`artifact://` 的解析维持 omp-host 现状(C1:私有 registry + 空全局集合,本就落空),非回归;修复依赖上游(§8 第 8 条)。SDK 自身的临时注册(structured-subagent.ts:362、vibe/runtime.ts:1427)不受影响。
- **目录台账(降级为非 router 用途)**:`Map<directoryKey, Set<artifactsDir>>` 台账保留,但只作 omp-host 自有枚举的作用域数据源——agent-runs 冷扫描与 transcript 端点定位(§5.5.1/§5.5.3,纯 FS,不经 router);未来 scheme capability 开启时它才是允许集数据来源。记账时机:`#materialize` 成功记活跃会话;启动及 session store 变更时按 `engine.listSessions({directory})`(engine.js:177-180)+ `artifactsDirectoryFor`(sessionFile 去 `.jsonl`,session-manager.ts:106-109)补冷会话;进程存活期内不注销(parked/dead 可达性,同 TUI "on disk" 行语义)。
- **上游解 blocker**:`agent://`/`history://`/`artifact://` 的磁盘扫描读模块级 `artifactsDirsFromRegistry()`(agent-protocol.ts:50、registry-helpers.ts:36-48),不感知 `ResolveContext`。向 omp 上游提案 per-resolve `ResolveContext.artifactsDirs`(context 优先、模块级回落)——落地后本节 capability 与允许集设计重新评估开放(§8 第 8 条)。
- **备选(登记为未来选项,均不进 P1,R2-H2)**:
  1. *每目录独立 router 实例*——`InternalUrlRouter.instance()` 是单例(router.ts:37-54),需上游支持实例注入;15 个 handler 的 per-instance 状态语义(local-protocol override、registry 引用)须重新定义,可行性未验证。
  2. *每目录独立 worker 进程*——进程边界隔离最彻底,但与"一个 omp-host 进程服务多目录"的部署事实(§2.4)冲突,内存/IPC 代价高;若多租户需求升级可作部署形态演进(§8 第 4 条多租户登记,届时与总纲另议)。
  3. ~~互斥 + 集合切换(R1 版)~~——已被 R2-H2 否决(反向泄露,理由见上),不再列为备选。

**备选对比(D04-3 为什么不是另外两条):**
1. *改共享全局 AgentRegistry、每会话以唯一 id 注册*——被否:注册表按 id 扁平寻址(hub 工具、IRC、`history://`),跨会话 id 冲突(`Anna` × 2)会互相覆盖(agent-registry.ts:159 `Map.set`);且改变模型可见的寻址语义,违反"TUI 为准"。
2. *完全绕开 router,omp-host 重实现各 scheme 的 web 投影*——被否:重复 containment/钉扎/jq 抽取逻辑,必然漂移;router 语义就是规格。(R2-H2 补注:该否决同时封死"宿主侧自行重建 `agent://`/`history://` 裸索引投影"的捷径——§5.7.3 的两个页签随 scheme capability 一并延后。)

#### 5.2.4 资源访问令牌:sourcePath 的受鉴权替代(R7 新增)

**决策 D04-3b:解析/动作响应一律不回显绝对 `sourcePath`(SDK 注释本就定位该字段为 "for debugging, not exposed to agent",types.ts:25),以不透明受鉴权令牌替代;绝对路径只存在于 omp-host 进程内存。**

- **令牌形状**:`OmpResourceToken = { id: string, expiresAt: number }`。`id` = 256-bit CSPRNG base64url(43 字符),不含任何路径/资源信息;服务端内存记录(无持久化、无跨进程):`{ absolutePath, resourceUrl, directory, sessionID?, contentType, filename, modes: ['read','download'], issuedAt, expiresAt, maxReads, reads }`。
- **scope(绑定)**:单资源(一次 resolve 的结果)+ 签发目录 + 操作集;`filename` 只取 basename。
- **expiry/限额**:默认 10 分钟(硬上限 1h)、`maxReads` 32(分页/重试余量);过期/超额 → 404,viewer 重新 resolve 即得新令牌(成本 = 一次 resolve)。
- **消费端点与查看器往返**:
```
GET /api/omp/uri/tokens/{id}           → { url, contentType, size, immutable, editable, filename }   // 描述,无路径
GET /api/omp/uri/tokens/{id}/content  → 字节流(支持 Range,大 artifact 分段)
    ?download=1                        → Content-Disposition: attachment; filename=<basename>
```
  InternalUriViewer 持令牌拉描述 + 内容;mutable `local://` 的"编辑"= 令牌拉取内容 + `POST /api/omp/uri/write`(同 directory+session,R8)回存——绝对路径全程不出服务端。
- **校验链**:omp-host Basic auth + web 会话层(R4,与 resolve 同级)→ 令牌存在/未过期/未超额 → `x-opencode-directory` 头 == 签发目录(纵深防御,不匹配 403)→ 允许集复核(签发目录台账仍含该资源目录;dispose 不撤销 parked 资源可达性)→ 流式读。
- **威胁注记**:①不可猜(256-bit,且爆破面在鉴权层之后);②单资源绑定,无目录遍历升级面(路径不进任何响应与令牌本体);③时效 + 次数上限限制重放;④日志只记 token id 与 resourceUrl,不记 absolutePath;⑤进程内存态,重启自失效,无持久化泄漏面。

#### 5.2.5 渲染与补全(GAP-02)

**链接拦截**——两层,全部挂在现有 markdown 管线上:

1. **显式链接**:`MarkdownRendererImpl` 的锚点处理处(MarkdownRendererImpl.tsx:85 起)加一个分支:`href` 匹配 `^(local|agent|history|artifact|skill|memory|rule|omp|issue|pr|ssh|vault|security|mcp|xd)://` **且该 scheme ∈ `capabilities.uri.schemes`** → `preventDefault` + 打开 InternalUriViewer,样式与文件链接一致(下划线 + scheme 色点);不在清单内的 scheme 保持普通文本样式(无死链,R2-H2)。
2. **裸文本链接**(tool 结果里的大量 `… at history://Anna`):在 `annotateFileLinks` 同一 pass 里加 `scheme://[A-Za-z0-9_./-]+` 的 span 提升逻辑,**只提升 `capabilities.uri.schemes` 内的 scheme**(P1 即 `local://`;其余保持纯文本,R2-H2),复用现有 debounce/限流骨架(MarkdownRendererImpl.tsx:443-513,`fileReferenceLinkLimit` 同源上限)。**不做 stat 探测**(URI 解析比 `/api/fs/stat` 贵),首点开时才 resolve。

**InternalUriViewer**(新组件 `components/omp/InternalUriViewer.tsx`):
- Dialog 全屏(移动端 bottom sheet),内容按 `contentType` 分流:markdown → 复用 MarkdownRenderer(递归渲染时禁用内部 scheme 之外的链接处理防嵌套);json → JsonTree;plain/log → 代码查看器(复用文件查看器,支持行号与 `artifact://N:a-b` selector 高亮)。
- 顶栏:scheme 图标 + URL + 操作(复制、下载 = 令牌 content?download=1、mutable local:// 时"编辑"= 令牌往返 §5.2.4、immutable 徽标)。**不展示任何绝对路径**。
- `isDirectory` 或 local:// 裸列表 → 目录列表视图,行点击 → 二级导航(压栈)。
- 加载态/错误态直接展示 handler 的用户友好错误文案(resolve 抛错即 404 body,§5.2.1)。
- **editor 补全**:composer 语言层(triggers.ts 的 picker 体系)加 `scheme://` 前缀触发,数据源 `completeUri`;仅 completionSchemes 白名单 ∩ `capabilities.uri.schemes`(R2-H2)。

### 5.4 会话树(GAP-04/05/06)

#### 5.4.1 只读快照(GAP-04)

```
GET /api/omp/sessions/{sessionID}/tree?directory=<dir>
→ {
    "sessionID": "ses_…", "directory": "…",
    "leafId": "e_…", "pathToLeaf": ["e_1", "e_4", "e_9"],
    "revision": 42,
    "nodes": [   // 扁平数组(UI 虚拟化),按 parentId 组树
      { "id": "e_9", "parentId": "e_4", "type": "message", "timestamp": "…",
        "label": "探索阶段",                       // resolved label(session-manager.ts:300-306)
        "gist": { "role": "assistant", "toolName": "read", "preview": "…≤80 chars…", "mode": null, "model": "glm-…/…" } }
    ]
  }
```

- **冷读实现**:与 `#projectedMessages` 同款(engine:379-393)——`SessionManager.open(file.path)` 只读 `getTree()` + `pathTo(leafId)`,不物化。live 会话直接读 `agentSession.sessionManager`。
- gist 提取规则(对齐 TreeSelectorComponent 显示):`message` → role/toolName/preview(用户消息取文本前 80 字符,assistant 取文本或首个 tool 名);`branch_summary` → preview=summary 前 80 字;`mode_change` → `mode` 字段(树里显示 `[mode: plan]`);`label` entry 不产生节点(它只改 target 的 resolved label);advisor/custom 隐藏项照 TUI 规则。`revision` = entries 计数,客户端带 `If-None-Match` 式缓存比对(简化:直接比对 revision,不同才换树)。

#### 5.4.2 变更通道(GAP-05)

```
POST /api/omp/sessions/{sessionID}/tree/navigate
{ "directory": "…", "targetId": "e_…",
  "summarize": false, "customInstructions": null,
  "allowAskReopen": true,
  "reanswerAskResult": null }        // 第二阶段:{content:[…], details:{…}, isError:false}
→ { "cancelled": false, "editorText": "…", "editorImages": [],
    "summaryEntry": null, "reopenAsk": null, "askReanswerCommitted": false,
    "leafId": "e_4", "revision": 43 }
```

- 实现:engine 新方法 `navigateTree({sessionID, directory, …})` → `#materialize`(navigate 需要完整 AgentSession:summarize 用模型、context rebuild)→ `agentSession.navigateTree(targetId, options)`(agent-session.ts:8273)→ 成功后 emit `omp.tree.updated`(经 Ch05 OmpEventBus,R1)+ wire `session.updated`(既有 wire 面),客户端按 §5.4.4 时序重拉消息。
- **`allowAskReopen` 恒传 true**(web 端实现了 ask 桥,Ch03);`reopenAsk` 返回时**不落地任何变更**(SDK 保证第一阶段零变更,agent-session.ts:8354-8375),UI 拉起 ask 对话框,答案作为 `reanswerAskResult` 二次调用。
- label:`POST /api/omp/sessions/{sessionID}/tree/label {targetId, label?}` → `manager.appendLabelChange`(session-manager.ts:2397-2400;`label: undefined` = 清除)。冷会话也可(只需 SessionManager)。
- `branchSummary.enabled`(默认 false)与 `summarize` 的 Custom prompt 输入,由 UI 在确认弹层提供,对齐 TUI 三选(No summary / Summarize / Custom prompt)。

#### 5.4.3 状态机(navigate 服务端)

```
             ┌────────────────────────────────────────────────┐
             │ session streaming?                             │
             └──────┬─────────────────────────┬───────────────┘
                    │ 是                       │ 否
                    ▼                          ▼
              409 {busy:true}          [navigating] ──session_before_tree cancel──▶ [idle](返回 cancelled:true)
                                         │
                          summarize? ────┴──── no
                             │                    │
                             ▼                    ▼
                      [summarizing](LLM,可中断)   │
                             │ aborted            │
                             ▼                    ▼
                      [idle](cancelled:true)   [committed]
                                         │ branch / branchWithSummary / resetLeaf
                                         │ rebuild context(agent-session.ts:8553-8560)
                                         ▼ emit omp.tree.updated + session.updated
                                         │
                          askReanswerCommitted?
                             │ yes            │ no
                             ▼                ▼
                    (UI 重建后调 resumeAfterAskReanswer   [idle]
                     —服务端自动:engine 检测到 flag 即调
                     agent-session.ts:8612-8614)
```

**决策 D04-4:流中(istreaming)拒绝 navigate,返回 409。** TUI 允许随时开树(navigateTree 只 flush bash),但 web 侧流中 navigate 会在 context rebuild 与流式投影(StreamProjector)之间产生竞态(UI 半渲染旧行半渲染新行)。差异登记于 §8.2。abort 后重试即可。

#### 5.4.4 客户端时序(含 ask 两阶段)

```
UI           /api/omp/sessions/{id}/tree/navigate    engine               SDK
│ ── navigate(target) ─────────────────────────▶ │ ── navigateTree ──▶ │
│                                                │                     │ reopenAsk?(一阶段,零变更)
│ ◀──────────── {reopenAsk:{toolCallId,questions}} ──────────────────── │
│ (Ch03 ask 桥渲染问题;用户作答)                                          │
│ ── navigate(target, reanswerAskResult) ──────▶ │ ── navigateTree ──▶ │
│                                                │                     │ 兄弟 toolResult 落枝(:8507-8518)
│ ◀──────────── {askReanswerCommitted:true, leafId, revision} ──────── │
│ (UI: 重拉 tree + messages;然后服务端已自动 resumeAfterAskReanswer)      │
```

普通导航(非 ask):一次调用 → `{leafId, editorText?}` → UI 重拉 `GET /session/{id}/message`(wire 既有)替换时间线;`editorText/editorImages` 预填 composer(对齐 TUI:user message 落叶在 parent + 编辑器预填,agent-session.ts:8481-8486)。

#### 5.4.5 与 revert/unrevert、`/undo` `/redo` 的 reconcile(GAP-06)

**决策 D04-5:树为唯一权威;revert marker 降级为兼容投影;`/undo` `/redo` 重定基到 navigateTree。**

- 事实:engine 的 revert 本来就是 `manager.branch()`(engine:759-761)——树原语。差距只在"只暴露单指针 + UI 靠 marker 隐藏消息"。
- 目标行为:
  - `/undo` = 对当前路径上**最后一条 user message** 调 navigate(`summarize:false`)→ 叶落到其 parent、composer 预填该消息文本。等价旧行为(revert+prefill)但树原生:被弃分支可再访。
  - `/redo` = navigate 回**上一个 leafId**。客户端内存记录 lastLeaf;跨刷新由 engine 在 sidecar registry 记 `lastLeafId`(与现有 `revert.previousLeaf` 同槽位语义,engine:762-765 迁移)。
  - 树 UI 里选中"当前叶"的兄弟分支 = 天然 redo 入口(TUI 同)。
- 兼容:wire Session 继续带 `revert` 字段一个迁移期(旧客户端靠它隐藏消息);`/undo` 新实现落地后,`handleSlashUndo`(session-ui-store.ts:1583-1619)改调 `apis.omp.navigateTree`,`revertToMessage` 的乐观 marker 逻辑保留为旧路径回滚。sync store 的时间线隐藏改为"重拉 messages"后自然正确,`session.revert` 分支(session-actions.ts:1320-1341)标记 deprecated,随 Ch07 清理。
- **不迁移存量数据**:树结构本来就在 JSONL 里,`revert` marker 只是 sidecar 视图,无转换需求。

### 5.5 Agent Hub / parked(GAP-07/08/09/10)

#### 5.5.1 聚合快照与事件(GAP-07,回答总纲开放问题 4;R3/R1 修订)

**决策 D04-6(REVISED):保持每会话私有 AgentRegistry(engine:462);omp-host 新增 `AgentRunsAggregator`(新文件 `omp-host/agent-runs-aggregator.js`,由原 HubAggregator 更名以对齐资源名)订阅各 registry 的 `onChange`(agent-registry.ts:298-301),聚合成目录级快照;资源面 = `/api/omp/agent-runs`(R3)。**

**与 02 章的拆分契约(R3)**:`AgentDefinition`(定义:发现/CRUD/选择器数据)→ 02 章 `/api/omp/agent-definitions`;`AgentRun`(运行实例:live/parked 快照 + revive/kill/chat/transcript 动作)→ 本章 `/api/omp/agent-runs`。两类型不得合流,GET/POST 语义互不兼容;配置选择器只消费前者,Hub/HUD/WorkStatus 只消费后者(08 章选择器照此修正)。

`OmpAgentRun`(聚合投影,UI 行数据;**响应不携带任何绝对路径**——transcript 经端点双段寻址,R7):

```json
{
  "key": "ses_abc123::Anna",          // UI 唯一键 = sessionID + "::" + agentId(跨会话扁平 id 冲突消解)
  "sessionID": "ses_abc123", "directory": "C:/proj",
  "agentId": "Anna", "displayName": "Anna",
  "kind": "sub", "parentId": "Main",
  "status": "running",                 // running|idle|parked|aborted|historical(R2-M5:historical = 磁盘扫描行,仅 transcript 只读)
  "createdAt": 0, "lastActivity": 0,
  "activity": "reading engine.js",     // setActivity 心跳 gist(agent-registry.ts:223-231)
  "history": { "model": "glm-…", "resolvedModel": "…",
               "metrics": { "tokens": 0, "requests": 0, "tools": 0, "cost": 0,
                            "durationMs": 0, "contextTokens": 0, "contextWindow": 0 },
               "readOnly": false, "outputPath": "agent://Anna" },
  "hasTranscript": true                // sessionFile 存在(registry-helpers.ts:104-122 语义)
}
```

- **模型面寻址不受影响**:hub 工具/IRC/`history://<id>` 在会话自己的私有 registry 内解析(flat id,TUI 语义原样);`key` 双段形式只存在于 UI 聚合层,不进模型上下文。这是"每会话私有 registry + 宿主聚合"优于"全局共享 registry"(§5.2.3 备选 1)的根因。
- 冷启动与冷会话行 = **historical**(R2-M5):进程重启后私有 registry 全空,`AgentRunsAggregator` 依据目录台账(§5.2.3)扫描出的"on disk"行(照 `sessionFilesFromDisk` registry-helpers.ts:64-92 语义)一律置 `status:'historical'`——**仅可读 transcript(§5.5.3,纯 FS 尾读),不提供 revive/kill/chat**。live 行合并规则:registry 行覆盖同 key 磁盘行(即"可见"与"可复活"是两个状态:historical 行永远可见、绝不可复活)。historical → 可复活需要持久化 revival descriptor(P2 另案,§5.5.2;SDK 已有的持久契约来源 = 每代理 JSONL 的 `session_init` entry,persisted-revive.ts:44-51、60-64),落地前磁盘行恒 historical。
- 事件(R1):registry `onChange` → 250ms 合并 → 快照 diff(按 key 比对)→ `omp.agents.updated` 全量快照包,经 Ch05 `OmpEventBus → /api/omp/events` 唯一通道下发(envelope/durable/作用域/重放以 05 注册表为准,本章为 producer)。UI `useOmpAgentRunsStore`:权威状态 = 最后快照;乐观更新只在本地行动作(revive 点击即置 `status:'running'` pending,失败回滚)。

```
GET /api/omp/agent-runs?directory=<dir>            → { agentRuns: OmpAgentRun[], generatedAt }
POST /api/omp/agent-runs/{sessionID}/{agentId}?directory=<dir>
  { "kind": "revive" }                              → 复活(仅 parked)
  { "kind": "kill" }                                → abort + tombstone
  { "kind": "chat", "text": "…", "mode": "prompt" | "steer" }
GET /api/omp/agent-runs/{sessionID}/{agentId}/transcript(§5.5.3)
```

双段寻址 `{sessionID}/{agentId}`:agentId 只在会话树内唯一(跨会话 `Anna` × 2 并存,agent-registry.ts:159 扁平 `Map.set` 会互相覆盖),评审示例的单段 `/agent-runs/{agentId}` 无法消解,故以 sessionID 段定域(R3 复数集合内的资源子路径)。`agentId === 'Main'` 时即会话本身。

#### 5.5.2 生命周期状态机(GAP-08)

```
                    spawn(task/tan 工具)
                         │ register
                         ▼
   ┌──────────────── running ────────────────┐
   │   │ stream end          │ TTL idleParkMs │
   │   ▼                     ▼                │
   │  idle ───────────▶ parked ──────────┐    │ chat/revive/IRC
   │   │ dispose(保留 ref)              │    │ ensureLive(CAS)
   │   │                     abort       ▼    │
   │   │                       │    [reviving]│
   │   │                       │        │     │
   │   ▼                       ▼        └─回 running/idle
   │ (sweeper: 超 MAX_LIVE 且闲置)        │
   │   engine dispose session 仅对 main ──┘
   ▼
 aborted(终态,tombstone;不可 revive,agent-registry.ts:196-198)

 磁盘扫描(冷启动/冷会话,R2-M5)──无 registry ref──▶ historical
                                                      │
                      transcript 尾读(§5.5.3)◀───────┘ 唯一动作面
                      revive/kill/chat → 409(UI 不渲染动作入口)
```

- **park 的 omp-host 实现**:`#sweepIdleSessions` 改造(engine:122-133)——TTL 到期不再 `sessions.delete`,而是 `#parkSession`:对**会话树内 sub agent refs** 置 `parked` + `detachSession`(agent-registry.ts:250-255);main 会话 dispose `agentSession` 但保留 HostSession 壳(即"main 隐性 parked",可被任何操作重新物化)。TTL 读 `task.idleParkMs` 设置(Ch06 面暴露;读取遵循 R6——全局实例唯一真值,宿主只读旁路,不做 reloadForCwd,亦无 per-directory 写)。
- **revive(仅进程内 parked 行,R2-M5)**:主 ref → `#materialize`(engine:436-504 已是完整的从 sessionFile 重建逻辑,零新代码);sub ref → 以私有 registry 的**内存** revival descriptor(AgentRef 元数据 + executor reviver 闭包)重放 createAgentSession(参照 TUI `AgentLifecycleManager.ensureLive` 与 persisted-revive.ts:101-151 的重建口径:toolset/systemPrompt/outputSchema/spawns 取自 `session_init`,taskDepth 取自父链)。advisor 拒绝(只读,对齐 agent-hub.ts:1077-1081)。
- **historical 行的动作面(R2-M5)**:revive/kill/chat → `409 {error:'historical', revivable:false}`;transcript 端点仍可用(纯 FS)。重启后私有 registry 空 → 全部磁盘行 historical,直至 revival descriptor 持久化落地(P2,§8 第 9 条):descriptor = 每代理 sessionFile 的 `session_init` entry(systemPrompt / tools / spawns / outputSchema(+mode)/ restrictToolNames / modelRole→resolvedModel / advisor,persisted-revive.ts:86-100、111、128-136)+ ref 元数据(id/parentId/displayName)+ 目录归属与版本号;omp-host 侧另需定义父会话重物化顺序与 descriptor 版本化策略——专项设计,落地前不实现。SDK 自身对缺 `session_init` 或 cwd 失效的文件也选择 transcript-only 而非错误复活(persisted-revive.ts:60-64),与本裁决同向。
- **kill**:`agentSession.abort()` + 等 idle + `unregister` 或置 `aborted` + 写 tombstone 文件(agent-registry.ts:18-23)。
- **chat**:parked → 先 revive(同上)→ `engine.prompt({delivery:'steer'|'queue'})`(engine:619-704 已实现 steer/followUp 双行为,mode 'steer' 映射 `steer`、'prompt' 落 `followUp`,与 TUI "revives then prompts/steers" 一致)。响应 `{ok:true, status:'running'}`;对话内容经既有 wire message 流呈现于主 transcript(主会话)或 agent transcript 尾读(子代理)。

#### 5.5.3 transcript 增量尾读(GAP-09)

```
GET /api/omp/agent-runs/{sessionID}/{agentId}/transcript?directory=<dir>&fromByte=<n>
→ { "agentId": "Anna", "text": "…完整 JSONL 行…", "newSize": 1048576 }   // 200
→ { "text": "", "newSize": 0, "error": "no transcript" }                 // 404 语义可 200+error
```

- **作用域(R7)**:服务端从请求 directory 的目录台账(§5.2.3)内定位该 run 的 sessionFile 路径;跨目录或未知 run → 404。响应不含任何文件路径(agentId/text/newSize 之外无路径字段)。historical 行同样可读(R2-M5:transcript-only 的"读"正是其全部动作面)。

**协议逐字复刻 AgentHubRemote**(agent-hub.ts:92-106 + agent-transcript-viewer.ts:349-414):
- 服务端:`fs.open` + `read(buffer, 0, len, fromByte)` 读 `[fromByte, stat.size)`,截到最后一个 `\n`,返回 `newSize = fromByte + 截后字节数`。头一行不完整部分留给下次(客户端不需要 pending 字段——服务端只回完整行)。
- 客户端轮询 250ms(对齐 POLL_MS);`newSize < fromByte` → 轮换/截断,清空全量从 0 重拉(:376-387)。
- 行解析:`parseSessionEntries` 等价逻辑在 UI 侧做——但 JSONL entry 结构是 SDK 类型,UI 不 import SDK;**服务端顺带返回轻量事件投影**:`?render=1` 时每行附 `{role, toolName?, preview, model}` 摘要(UI 只渲染摘要 + 原文折叠),避免 UI 依赖 SDK entry 联合类型。默认 0(裸 JSONL,供调试视图)。
- 大小上限:单次响应 ≤512KiB,超出分页(newSize 即游标)。

#### 5.5.4 subagent HUD(GAP-10)

- 数据:`useOmpAgentRunsStore` 过滤 `status === 'running' && agentId !== 'Main' && sessionID === 当前会话`,按 TUI 上限截断行数 + "… N more running" 尾行(interactive-mode.ts:455-507)。
- 位置:composer 上方锚定块(Ch08 协调与 WorkStatusPanel subagents 节的分工:HUD = 行内常驻最小面,面板 = 详情)。
- 行点击 = 打开 Hub 侧栏对应详情;`history://<id>` 链接在该 scheme 宿主解析 capability 开启前保持纯文本(R2-H2;开启后经查看器按会话钉扎解析)。

### 5.6 async jobs(GAP-11;R12 修订:capability 门控 + ownerSessionID)

**能力门控(R2/R12)**:`capabilities.jobs` 在上游提供 AsyncJobManager 注入项(§8.5)之前**恒为 false**。UI 一切 jobs 面(/jobs 视图入口、jobs 卡、`omp.jobs.updated` 消费)以该键门控;false 时 UI 不发起请求、不渲染入口、不把 501 当错误态。

```
GET /api/omp/jobs?directory=<dir>&sessionID=<sid>&recentLimit=5
// capabilities.jobs = false(当前稳态):
→ 501 { "error": "jobs-unavailable", "reason": "sdk-single-manager",
       "ownerSessionID": "ses_first" | null }        // 恒结构化 501,绝不 404(R12)
// capabilities.jobs = true(上游注入落地后):
→ 200 { "ownerSessionID": "ses_…",                   // 所有响应必带 owner(R12)
        "running": [{"id","type","status","label","startTime"}],
        "recent":  [同上], "delivery": {"queued":0,"delivering":false} }  // = AsyncJobSnapshot
```

- **owner 语义**:ownerSessionID = 当前持有 AsyncJobManager 的会话(C2:进程内首个 top-level 物化会话,sdk.ts:1599-1616;注入落地后 = 请求会话)。capability=false 时若进程内已有隐性 manager(首会话),仍如实返回其 id——"可用性"(capability)与"归属"(ownerSessionID)分离,任何会话、任何时序请求都得到同一确定性响应,杜绝"非 owner 会话随机 404"(评审中危)。UI 侧 RuntimeAPIs 客户端把 501 body 映射为 `{available:false, ownerSessionID}`。
- **快照来源**:`#materialize` 后从 `hostSession.agentSession.getAsyncJobSnapshot({recentLimit})`(agent-session.ts:1808-1811)直取;事件 `omp.jobs.updated`(名称与注册表归 Ch05,R1)仅在 capability=true 时生产,producer 钩子 = job 注册/落定 + async-result 抵达;端点轮询兜底(UI 每 5s,仅 /jobs 视图打开时)。
- **async-result 呈现**(卡片设计与能力开关解耦:capability=false 时进程内本就无 job,无卡片可出):`async-result` CustomMessage 经 yield queue 以 custom message 进入 transcript——事件映射是 Ch05 的 customType 通道;本章定义卡片:标题 "Background job completed [type] <jobId> (duration)"(对齐 transcript-render-helpers.ts:26-29),正文 = 4k 预览 + "查看完整结果" 链接(`artifact://N` 或 agent:// 句柄)→ InternalUriViewer;链接可达性另受 `uri.schemes` 门控——`artifact://`/`agent://` 未开启时只渲染预览文本、不渲染链接(R2-H2)。`BACKGROUND_TAN_DISPATCH` pill("◌ Tangent dispatched [task] <jobId>")同通道。
- **`/jobs` 视图**:command palette 输出卡(command-controller.ts:486-521 的 web 版):Running/Recent 两表 + 每行 duration;点击行 → 展开最新 latestDetails(若有)。capability=false 时入口不渲染。

### 5.7 P2 杂项

#### 5.7.1 IRC 卡与 hub 工具面(GAP-12)

- IRC 卡:`irc:incoming / irc:autoreply / irc:relay` customType → 流内卡片 `IRC ← From` / `IRC → To`(meta "auto"/"reply"/age;发送回执按 `injected|woken|revived|failed` 着色,messaging.ts:472-563)。渲染细节归 Ch05 的 registerMessageRenderer 等价机制;本章定卡面语义。hub 工具的进程监督(start/ps/logs…)是**模型面工具**,web 不重建 UI,仅其消息卡透传渲染。OpenChamber 的 `hub` 工具调用出现在 tool part 时按普通工具渲染(不专门设计)。
- 前置:私有 registry 下 IrcBus 跨会话投递语义需与 §5.5.1 聚合一致(IRC 的收件人解析在 SDK 侧走注入的 registry——engine 已传私有 registry,sdk.ts:526-527,同会话树内 peers 可达,跨会话不可达,与模型面 flat 寻址约定一致)。

#### 5.7.2 drafts(GAP-13)

```
GET /api/omp/sessions/{sessionID}/draft?directory=   → { text }   // consumeDraft 一次性(session-manager.ts:2012+)
PUT /api/omp/sessions/{sessionID}/draft  { text }    → { ok }     // saveDraft;空文本 unlink(:1985+)
```

- 触发:UI 会话关闭/切换时 PUT 当前 composer 文本(节流 2s,空不写);会话打开时 GET——**有文本才回填**(一次性,二次打开不重复回填,对齐 TUI)。与 UI 本地 chatDraftPersistence 的关系:服务端 draft 只在"打开会话且 composer 为空"时生效一次,本地持久化继续服务快速切换。归并细节开放问题 §8.6。

#### 5.7.3 artifacts 浏览与导出(GAP-14)

- 浏览:复用 `resolveUri('local://', …)` 列表 + InternalUriViewer 目录导航(P2 亦仅此范围,R2-H2);`agent://` 与 `history://` 裸索引页签随其宿主解析 capability 一并延后——不在 omp-host 侧自行重建索引投影(绕开 router 已在 §5.2.3 备选 2 否决)。**不把 artifacts 目录挂进 /api/fs 文件树**(它在 agentDir 下,与项目无关,避免污染项目文件面板)。
- HTML 导出:`POST /api/omp/sessions/{sessionID}/export/html` → 深路径 import `export/html`(exportSessionToHtml,export/html/index.ts:192-296)→ 以 `Content-Disposition: attachment` 回 standalone HTML(含 subSessions 与 `?leafId=` 深链)。替代现有 markdown 导出的增强项而非替换(lib/exportSession.ts 保留)。
- `/share`(omp sealed 上传)与 OpenCode share 均不做:前者低优另议(总纲 D3),后者归 Ch07 删(交接 GAP-15)。

#### 5.7.4 P2 scheme 门控(GAP-16;R6/R8)

`vault:// security://` 等设置门控由 SDK handler 自查(`vault.enabled` 等);端点透传后 403/错误文案直达 UI,无需 omp-host 重复实现。**R6**:这些读取发生在 SDK 全局 settings 实例上,遵循 06 章裁决——全局实例为唯一真值,宿主不做 reloadForCwd 式前台切换;本章端点亦不提供任何 per-directory 设置写入。**R8**:全部 P2 scheme 写能力(ssh/vault/xd)在威胁评审(维度见 §5.2.1)通过并开启对应 capabilities 键之前,端点恒 `501 write-not-enabled`。**R2-M11(读侧)**:`ssh/vault/security/mcp` 的读能力纳入 `uri.schemes` 前,须通过与写同级的威胁评审(宿主凭据与内网可达面、外部敏感内容向浏览器暴露、审计日志、大小/速率限制、principal 作用域,§5.0)——通过前这四个 scheme 连读也返回 `501 scheme-not-enabled`,P2 透传清单默认不含它们。`xd://` 的 session-bound dispatcher 依赖调用会话(§5.2.3 钉扎规则),`xd://resolve|reject` 特例设备返回只读说明。

#### 5.7.5 MCP 可执行端点(本轮不建设;GAP-17 存根,R2-M6)

**裁决(R2-M6,取代 R12 的"本章持有定义权"占位):MCP 可执行管理端点(connect/disconnect/重启、状态机、连接所有权、credential、失败/重连、dispose)本轮不建设,定义权移出本章,由专项轮次承载;本章不把它列入任何建设清单。**在专项落地前:07 §5.10 只读状态面与 08 §5.5 管理开关禁用是**长期稳态**而非"落地前临时态",不得出现"看似成功"的 UI 操作;`mcp://` resource 的只读透传亦不在默认 capability 集内(R2-M11:读侧评审未通过,§5.7.4)。本节仅作交接登记,不含设计。

### 5.8 engine.js / UI 触点汇总

| 位置 | 改动 |
|---|---|
| engine.js `#materialize`(436-504) | 成功后:UriBridge 按 directory 记账 artifactsDir(目录台账,§5.2.3;仅作 agent-runs/transcript 作用域数据源,**不触碰 SDK 进程级集合**,R2-H2);AgentRunsAggregator 订阅该会话私有 registry `onChange`;记录 main ref 元数据 |
| engine.js `#disposeSession`(135-142) | park 语义分离(§5.5.2);台账不注销(冷会话/parked 可达性) |
| engine.js `#sweepIdleSessions`(122-133) | TTL → `#parkSession`(sub refs → parked + detach;main 保壳) |
| engine.js `#handleEngineEvent`(506-614) | **无新增 case**(agents/tree/jobs 事件不经引擎事件流,走 registry/manager 钩子)——customType(async-result 等)映射归 Ch05 |
| engine.js 新方法 | `navigateTree` / `setTreeLabel` / `getTreeSnapshot` / `agentRunAction` / `readAgentRunTranscript` / `getJobsSnapshot`(501 语义,§5.6)/ `draftGet/Put` / `exportHtml` |
| endpoints.js | `/api/omp/*` 路由组(§5.0,R3 复数规约);全部注册于 omp-host、经既有 Basic auth(R4);复用 `directoryFromRequest`(endpoints:85-93) |
| 新文件 `omp-host/uri-bridge.js` | §5.2.3 目录台账 + `local://` 会话钉扎解析(零 SDK 全局态变更,R2-H2);§5.2.4 令牌服务 |
| 新文件 `omp-host/agent-runs-aggregator.js`(原 HubAggregator 更名) | §5.5.1 聚合 + 快照事件 + 磁盘恢复(historical 行,按目录台账,R2-M5) |
| 事件(经 Ch05 `OmpEventBus → /api/omp/events`) | `omp.agents.updated` / `omp.tree.updated` / `omp.jobs.updated`(名称+注册表归 05,R1);废止 `openchamber:omp-*` |
| packages/ui `lib/api/types.ts` | `OmpParityAPI` 接口 + `RuntimeAPIs` 新增 `omp` 键(types.ts:1225-1241 扩展)(§5.0) |
| packages/ui `lib/ompParity/` | runtimeFetch 客户端封装(jobs 501 → `{available:false, ownerSessionID}` 映射,§5.6) |
| packages/ui stores | `useOmpAgentRunsStore` / `useSessionTreeStore` / `useOmpJobsStore`(sync-state-invariants:权威快照 + 乐观动作;jobs 挂 capabilities 门控) |
| packages/ui components | `InternalUriViewer`(令牌消费,§5.2.4)、`TreeDialog`(过滤/label/summarize 三选/ask 桥)、Hub 侧栏(chat view = 尾读 viewer + 输入行)、HUD 块、jobs 卡、markdown scheme 链接 pass(MarkdownRendererImpl.tsx) |
| packages/ui `session-ui-store.handleSlashUndo/Redo`(1583-1626) | 重定基 navigateTree(§5.4.5) |

---

## 6. 迁移与兼容

1. **阶段开关(R2 修订)**:本地 flag 全部废弃——host 设置 `ompParityEndpoints` 与 UI flag `ompParity`(照 planModeExperimentalEnabled 的 /health 下发模式)不再使用;统一 `GET /api/omp/capabilities`(总纲 D6-R2)feature 键门控:`uri.schemes`(P1 = `['local']`,R2-H2)/ `uri.write.local`(P1)/ `tree.read` / `tree.navigate`(P1)、`agentRuns` / `drafts` / `artifacts` / `export`(P2)、`jobs`(上游注入前恒 false,R12)、`uri.write.ssh|vault|xd`(威胁评审通过后,§5.2.1;`ssh/vault/security/mcp` 读侧评审规则见 §5.0,R2-M11)。服务端裁决开关;新 UI+旧 engine、旧 UI+新 engine、relay 旧 bundle 三矩阵由 capabilities 版本字段承载(总纲 R2),本章不另设回退开关。
2. **存量会话**:树/jobs 从既有 JSONL + artifacts 目录投影,零数据迁移;旧会话(omp-host 早期或 TUI 产生)同样可读(`getTree` 的磁盘扫描与 entry 解析是 SDK 原生兼容层)。URI 面:`history://` 等宿主解析受 R2-H2 关闭影响(P1 不可解析,链接保持纯文本);重启后 agent-runs 磁盘行一律 historical、只读(R2-M5)。
3. **并发会话**:AgentRunsAggregator 快照对并发 registry 事件做 250ms 合并;`revision` 单调,UI 丢弃乱序旧包(按 generatedAt)。
4. **revert 兼容**(§5.4.5):`session.revert` wire 字段保留一个迁移期;`/undo` 切换由 capabilities(`tree.navigate`)控制,回滚 = 切回旧 `revertToMessage` 路径。sidecar `revert.previousLeaf` → `lastLeafId` 字段迁移(读旧写新,不删旧)。
5. **回滚策略**:整组端点无状态投影 + 显式入口,关 capability 即回滚;唯一持久化新写入 = tombstone 文件(SDK 原生格式)与 `draft.txt`(SDK 原生)、`lastLeafId`(sidecar)。无 schema 破坏。令牌为进程内存态,重启自清(§5.2.4)。
6. **多运行时(runtime switching / relay)**:端点走 runtimeFetch 与 `/api` 前缀,多实例切换与 E2EE relay 自动生效(§2.2),无额外工作。

---

## 7. 验证方案(D5:设计态,列出待执行)

**omp-host 单元/集成(bun:test,`omp-host.uri-bridge.test.js` / `omp-host.uri-tokens.test.js` / `omp-host.tree.test.js` / `omp-host.agent-runs.test.js`)**

- URI 门控与隔离(R2-H2 安全面,先于端点上线):
  - scheme 门控:`agent://`/`history://`/`artifact://`(及 `mcp://`)resolve/complete → `501 {error:'scheme-not-enabled'}`——无论进程内并发多少目录的会话;未知 scheme(`foo://`)→ 404 unknown-scheme;`file://` → 拒绝。
  - 零全局变更断言:任意 resolve/write 前后,SDK 进程级扫描集合与 `LocalProtocolHandler` override 状态不变(UriBridge 全程不调 `registerArtifactsDir`/unregister/`setOverride`)。
  - `local://` 钉扎与 containment:同目录两会话各自的 `local://` 互不可见;缺 `sessionID` → 400;`local://../../etc/passwd` 类 → handler 拒绝,404 带文案;`local://` 裸列表含 plan.md;1MiB 文本上限/二进制拒绝(local-protocol.ts:109-119)。
  - 写边界(R8):`local://` 同 directory+session 写 → ok;跨 directory → 403;无 sessionID → 400;`ssh://`/`vault://`/`xd://` 写 → 501 write-not-enabled。
  - (双目录 404 隔离与钉扎用例随 `agent/history/artifact` capability 开启(上游落地后)再建,届时按 R7 口径:目录 A 请求 B 产物 → 404;同目录冷会话可解析、B 目录冷会话不可;`history://` 裸索引只含 A 的行;`artifact://3` 各会话各命中(artifact-protocol.ts:38-55);`agent://Anna?q=` JSON 抽取与 `agent://Parent/Child` 层级。)
- 令牌(R7):resolve 响应断言无绝对路径(正则扫盘符/根路径模式,全响应体);`GET /api/omp/uri/tokens/{id}/content` 正常且支持 Range;过期/超额/伪造 id → 404;`x-opencode-directory` 与签发目录不匹配 → 403;`download=1` 的 Content-Disposition 文件名只含 basename。
- 树:冷会话 getTree 不物化(断言 `engine.sessions` 空);navigate 落叶规则四分支(user/custom_message/ask/其他);`branch_summary` 挂目标位;流中 navigate → 409;label 设置/清除/labelsInEffect 投影。
- Hub:聚合去重(两会话同名 `Anna` → 两行不同 key);park TTL 后 sub ref = parked 且 transcript 端点仍可读(限同目录);跨目录 transcript 请求 → 404;进程内 revive(parked)→ running + 快照事件;kill → aborted + tombstone + 再 revive 拒绝(terminal,agent-registry.ts:196-198);**historical 行(R2-M5):新进程冷启后全部磁盘行 = historical,revive/kill/chat → 409、transcript 尾读仍可用,registry 行覆盖同 key 磁盘行**;`newSize < fromByte` 轮换分支。
- jobs(R12):`capabilities.jobs=false` 时 `GET /api/omp/jobs` → 501 `{error:'jobs-unavailable', ownerSessionID}`(断言任何会话均非 404、响应确定);stub manager 存在时 → 200 且必带 ownerSessionID。
- `node --check` 全部新/改 server JS。

**E2E(dev 栈 5180/3902,浏览器驱动)**

1. 发起含 subagent 的任务 → tool 结果里的 `local://` 链接被提升为可点击 → viewer 正常打开;同消息中的 `history://Anna` 裸文本**保持纯文本**(P1 capabilities,R2-H2);下载走令牌,响应头与体均无绝对路径。
2. 双目录隔离(评审硬性安全场景):浏览器双开目录 A 与 B 的会话;A 中手工调 resolve 指向 B 产物的 `agent://`/`history://` → `501 scheme-not-enabled`(能力关闭即隔离,不依赖任何运行期集合状态);UI 不提升 B 产物链接。
3. 树:打开 TreeDialog → 过滤 user-only → 选历史 user message → 时间线重拉 + composer 预填;Shift+Enter summarize-and-switch 产生 branch_summary 分隔条(对齐 TUI compaction divider 形态,Ch05 渲染)。
4. `/undo` → 叶回退 + 预填;`/redo` → 回原叶。
5. Hub:等待 idleParkMs(测试配置调小)→ 行变 parked → `r` 复活 → chat 发消息 → 主 transcript 出现该 agent 新回合;**重启 dev 栈 → 同一行变 historical,无复活键,transcript 仍可读(R2-M5)**。
6. `/jobs`:capabilities.jobs=false 的 dev 栈 → UI 不渲染 /jobs 入口;手工调 `GET /api/omp/jobs` → 501 结构化 payload + ownerSessionID(capability 打开后补 Running 表 + "Background job completed" 卡 + artifact 链接场景)。
7. ask re-answer:选中 ask 结果节点 → ask 桥重开 → 换答案 → 兄弟分支可见、agent 自动续跑。

**TUI 对照点**:TreeDialog 过滤键位集 == tree-selector.ts 的 default/no-tools/user-only/labeled-only/all;hub 排序 == STATUS_ORDER running>idle>parked>aborted(omp-host 另有 historical 只读行,R2-M5);jobs 表 == command-controller.ts:486-521 输出;transcript 尾读行为 == agent-transcript-viewer.ts(250ms/换行截断/轮换重拉);URI 裸索引与 TUI 的分歧(单工作位全量 vs 多目录;P1 = 宿主解析关闭,R2-H2)== §5.2.3 登记项。

---

## 8. 开放问题

1. **(总纲问题 4 的本章裁决;R3 修订)Agent Hub 数据口径**:本章采用"每会话私有 registry + 宿主聚合投影(UI 层双段 key)",资源名 `/api/omp/agent-runs`;agent 定义面(发现/CRUD/选择器)归 Ch02 `/api/omp/agent-definitions`(R3 拆分,契约见 §5.5.1)。若未来上游 SDK 提供"多 top-level 会话 + 共享 registry"的一等支持(如 id 自动加会话前缀),聚合层可简化为直读——但模型面 flat 寻址语义必须保持 TUI 一致,故不主动推动共享 registry。**建议:维持 D04-6(修订版)。**
2. **流中 navigate**:409 拒绝(D04-4)vs TUI 的"随时可开树"。若产品要求流中导航,需先解决 StreamProjector 与 context rebuild 的竞态(可能要暂停投影 + navigate + 全量重放)。**建议:P1 先 409,观察需求。**
3. ~~URI 端点的 sourcePath 暴露~~ **已裁决(R7)**:响应不再回显绝对 `sourcePath`,以 §5.2.4 受鉴权资源令牌替代。令牌参数(expiry 10min / maxReads 32)为工程默认值,上线后按使用体验微调,不构成开放决策。
4. ~~跨目录可见性~~ **已裁决(R7;R2-H2 收紧)**:`agent://`/`history://` 跨目录合并视图**取消**——且宿主侧解析整体 OFF 至上游 context 化(§5.2.3:capability 不含这三 scheme,`local://` 之外零解析)。与 TUI 单工作位语义的分歧显式登记:TUI 单进程单工作位,"合并"与"本工作位"天然同一;omp-host 多目录下逐目录切分,裸索引只呈现请求目录的行。**残留面(R2-H2 重述)**:R1 版"稳态 = 活跃会话并集注册"已废止(并集本身即泄露面,§5.2.3);会话内模型工具解析维持 omp-host 现状(C1 落空,非回归),修复依赖上游(第 8 条);多租户/团队部署的进一步加固(per-principal ACL、令牌审计、路径白名单)在需求出现时另行评审,不进当前阶段。总纲 §7.4 "经 registerArtifactsDir 补全局解析 / HubAggregator" 的旧措辞由 D6-R7 与本章更名(`AgentRunsAggregator`)取代。
5. **上游 jobs 单 manager 限制(C2)**:向 omp 上游提案 `createAgentSession({ asyncJobManager })` 注入项(或导出 AsyncJobManager 于包出口);落地前 `capabilities.jobs` 恒 false、`GET /api/omp/jobs` 恒结构化 501 + `ownerSessionID`(§5.6),杜绝按物化时序的随机 404(R12)。**建议:提 upstream issue,注明 omp-host 嵌入场景。**
6. **drafts 双层归并**:服务端 draft.txt(一次性)与 UI 本地 chatDraftPersistence(常驻)的优先级与覆盖时机(§5.7.2 的"打开且 composer 为空才回填"是否足够)。**建议:按 §5.7.2 上线后收集体验再定。**
7. **HTML 导出的深链消费**:`?leafId=&targetId=` 深链在 standalone HTML 内有效;是否要反向支持"从 web 树节点生成带深链的导出"并支持导入回放。**建议:仅导出,不做导入。**
8. **上游:per-resolve `ResolveContext.artifactsDirs`(R2-H2 升格为本章首要上游提案)**:`agent://`/`history://`/`artifact://` 的磁盘扫描读模块级 `artifactsDirsFromRegistry()`(registry-helpers.ts:36-48),不感知 `ResolveContext`——这是宿主解析 capability 关闭的唯一技术根因。提案内容:context 优先、模块级回落;落地后该 capability 开放 + §5.2.3 允许集设计重启评估(含会话内解析的收窄)。备选(每目录 router 实例 / 每目录 worker 进程,§5.2.3)登记为未来选项,不进 P1。**建议:与第 5 条合并为一个 upstream issue 包,注明 omp-host 多目录嵌入场景。**
9. **revival descriptor 持久化(P2 另案,R2-M5)**:historical 行升级为可复活所需的 descriptor 设计——基于 SDK 既有 `session_init` 持久契约(persisted-revive.ts:44-51)定义 omp-host 侧的目录归属、父会话重物化顺序、descriptor 版本化与"缺字段即保持 historical"的降级规则。落地前重启后磁盘行恒 historical(只读)。**建议:P2 开题,先写 descriptor 字段清单再动端点。**

---

## 9. 依赖

**前置:**
- Ch01(P0):hub 行的 model/history 展示依赖 roles 语义定稿(`history.modelRole` 等字段口径)。
- Ch02(R3 拆分):`/api/omp/agent-definitions`(定义 CRUD)与本章 `/api/omp/agent-runs`(运行实例)是同一次拆分的两面;`AgentDefinition` / `AgentRun` 类型边界与消费方约定见 §5.5.1。
- Ch03(P1):ask 对话框桥是树导航 ask 两阶段(§5.4.4)的硬前置;无桥则 `allowAskReopen` 恒 false(降级:ask 结果节点只读)。
- Ch05(P1,R1/R2):`omp.agents.updated`/`omp.tree.updated`/`omp.jobs.updated` 的注册表条目、envelope、重放与 resync 矩阵归 05;本章是 producer。本章 feature 键经 `GET /api/omp/capabilities` 承载。
- Ch06(R6):`task.idleParkMs`、`branchSummary.enabled`、`async.maxJobs` 设置面暴露;本章一切设置读取遵循 06 全局实例裁决(只读旁路,不做 reloadForCwd 式切换,亦不提供 per-directory 设置写)。

**后置:**
- Ch07:GAP-15(share 501 桩)删除;`session.revert` deprecated 分支清理(§5.4.5);MCP 只读状态面为本轮恒态(§5.7.5 专项轮次,R2-M6)。
- Ch08:subagent HUD 与 WorkStatusPanel subagents 节的分工落地;multirun/AgentManager 与 agent-runs 聚合面并轨(数据源统一 `useOmpAgentRunsStore`);MCP 管理开关本轮恒禁用(§5.7.5,R2-M6)。
