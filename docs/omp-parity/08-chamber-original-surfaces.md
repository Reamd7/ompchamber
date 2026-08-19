# 08 · 域 H:OpenChamber 原创面保留与适配

状态:设计稿 v3(R2 修订轮;依据 master `00-MASTER.md` §2.3、D1–D6)
日期基线:2026-08-19(R2 修订:2026-08-20)
裁决口径:**OpenChamber 原创增值面保留,但所有"默认模型 / 默认 agent(→ persona 与 worker 定义分型,ch02)/ variant / goal"输入必须迁移到 omp 概念体系;permission 输入删除(无人值守 fail-closed,R10);凡 omp 已规划同义面(ch02/04/05),OpenChamber 面不得重复建设,按所有权地图收敛。**

> **v2 修订摘要**(评审裁决落地本章者):R10 删除 per-session 审批端点依赖,无人值守任务 fail-closed(§5.3);化身输入分型 —— persona = `/api/omp/personas`(可选层,默认无)、worker = `/api/omp/agent-definitions`(02 §5.2/5.2a,R3);subagents 节数据源 = ch04 `/api/omp/agent-runs`、MCP 节只读 + 开关禁用(04 §5.7.5 落地前,R12);队列写入协议(requestId 幂等 + server ack + outbox)+ 事件定名 `omp.queue.changed` + 纳入 05 §5.2.4 矩阵(R12,§5.7);命令发现走 `GET /api/omp/commands`,wire `/command` 空返直至 ch07 删除(R12,§5.4);通知触发只引用 ch05 注册表事件、经唯一通道(R1,§5.6);capabilities 门控(`queue.v1`/`commands.v1`)+ 回滚三矩阵(R2,§6);goal P1 = 投影、自主续跑 P2(R12,§5.3/5.5)。

> **v3(R2 评审)修订摘要**:H6 —— §5.7 方案 B 的写入协议(requestId 幂等 + server ack + outbox)经源码核验在现有 SDK 上**不可实现**(`getQueuedMessages()` 仅返回文本数组,agent-session.ts:6425-6429;入队 `PromptOptions` 无 ID 字段,agent-session-types.ts:290-307;`clearQueue` 整批语义,:6398-6411;引擎无版本化队列事件);方案 B 与 capabilities `queue.v1` 整体 gated 于上游 SDK 队列扩展三件套(入队带稳定 ID + 快照携带 ID + 带 version 变更事件 + 跨重启持久台账),此前维持方案 A(client queue 原样、行为零改动),且不注册文本-only 的半吊子 queue 快照端点(§5.7,OQ-1)。M9 —— §5.6 增设通知权威规则:终局 `agent_end` 为完成/失败系统通知的**唯一权威**,`omp.notice.raised` 降级为应用内瞬时 toast(notice 载荷无 sessionID/turnID/errorID,按 ID 去重不可实现;角色互斥免去重,与 TUI 语义一致)。

---

## 1. 域概述与边界

本域管什么:

- **OpenChamber 原创产品面的完整清单与逐项处置**(保留原样 / 适配输入 / 吸收 omp 概念 / 退役):WorkStatusPanel 及其全部 section、multirun/AgentManager、scheduled tasks、projects/worktrees 体系、GitHub 集成、magic slash commands、通知系统、files/terminal/browser/context 视图、消息队列(client queue)、tray/CommandPalette/PromptNavigatorRail/walkthrough/TTS/mini chat 等次要原创面。
- **输入迁移规格**:凡原创面自己解析"默认模型/默认 agent/variant"(multirun 表单、scheduled task 编辑器、GitHub issue picker、NewWorktreeDialog 四处 `resolveDefaultModelSelection` 族)→ 统一迁移到 ch01 的 role 解析与 ch02 的 agent 列表。
- **共存规则**:原创面板与 ch02(模式/agents)、ch04(Agent Hub/MCP/URI)、ch05(usage row/todo/流内元素)计划面之间的元素级所有权地图;magic slash commands 与 omp 内建 `/command` 的优先级与碰撞处理。
- **通知触发事件迁移**:OC 自有设置层与推送通道全保留,触发源从 OpenCode wire 事件(`message.updated` finish、`question.asked`、`permission.asked`)迁移到 ch05 注册表事件:`agent_end`(isTerminal 分支,05 §5.7)、`omp.notice.raised`、`omp.dialog.requested/settled`(ch03 桥,kind=approval/ask;05 §5.0.3-C)—— 一律经 ch05 唯一 `OmpEventBus → /api/omp/events` 通道消费,不自建通道(R1)。
- **队列语义对账**:OC client queue(`messageQueueStore`)vs omp 引擎 steer/followUp 队列(engine 已按 `delivery` 映射对齐),列出 delta 并给出终局设计。

本域不管什么:

- model roles 机制与设置键本身(ch01);agents 定义/模式产品化(ch02);审批弹窗与 ask 对话框桥(ch03);URI schemes/会话树/Agent Hub 端点/jobs/IRC/drafts(ch04);12 类 drop 事件映射与流内元素渲染(ch05);设置面代理 omp schema(ch06);OpenCode 残留删除顺序(ch07)。
- OpenCode wire 契约的形状(vendored gen,不手改,D1)。

与其他章的接口(依赖契约,详见 §9):

- **ch01**:提供 `GET /api/omp/models?directory=…`(模型+角色快照,含 `roles`/`cycleOrder`/`legacyDefaults`;01 §5.3);本域所有原创面输入默认值消费它的 role 面。
- **ch02**:提供 persona 面(`/api/omp/personas`,OpenChamber 原创可选层,默认无;02 §5.2a)与 worker 定义面(`/api/omp/agent-definitions`;02 §5.2)、goal 状态/事件(`omp.goal.updated` + `GET /api/omp/sessions/{id}/goal`,02 §5.6);本域 goal row、multirun/scheduled/GitHub/NewWorktree 化身输入消费它。
- **ch03**:提供对话框桥(`omp.dialog.requested/settled` 经 ch05 通道 + `GET /api/omp/dialogs` 权威快照;无人值守任务 fail-closed,R10);本域 WorkStatus subagents 阻塞显示、通知触发、tray 审批菜单消费它。
- **ch04**:提供 `/api/omp/agent-runs` 聚合快照(R3;`OmpAgentRun` live/parked 行 + revive/kill 动作,04 §5.5.1)与 MCP 可执行端点定义权(04 §5.7.5;落地前 UI 只读);本域 subagents section 与 MCP section 消费它。
- **ch05**:提供唯一事件通道 `OmpEventBus → /api/omp/events` 与事件注册表(05 §5.0.3)、bootstrap/resync 矩阵(05 §5.2.4,含 queue 对账);本域通知触发、tasks section、context+cost 数据源、队列对账消费它。

---

## 2. 现状分析(OpenChamber 侧)

路径基准:`packages/ui/src`(UI)、`packages/web/server/lib`(服务端)。omp-host 引擎侧:`packages/web/server/lib/omp-host/{engine,endpoints}.js`。

### 2.1 WorkStatusPanel(chat 列内状态卡)

- 容器与节可见性:`components/chat/work-status/WorkStatusPanel.tsx:64-75`(docstring:报告 session、其 branch 与其 subagents 的状态卡)、`:247-259` 按 section 渲染、`:70` `useUIStore.workStatusHiddenSections`、`:240` presence provider 统计已渲染节数。**纯 OC 原创,无 omp 对应物。**
- **session 节(context+cost)**:`components/chat/work-status/WorkStatusPrimaryGroup.tsx:192-203` —— `contextUsage.percent`(由消息 token 求和而来,`work-status/contextUsage.ts`) + `session.cost`($ 花费,wire `Session.cost`),`:214-232` 渲染百分比 + 花费 + 仪表条。
- **goal row**:`WorkStatusGoalRow`(WorkStatusPanel.tsx:252 挂载)。数据源是 OC 自有 session-goal 体系:`hooks/useSessionGoal.ts:14-22` —— goal payload 挂在 `session.updated` 的 metadata 上(`lib/sessionGoalMetadata.ts`);服务端 `web/server/lib/session-goal/runtime.js` 负责创建/结算。**OC 原创 goal 系统,与 omp goal mode(/goal + budget + goal_updated)是平行宇宙。**
- **repository 节**:`WorkStatusPrimaryGroup.tsx:239-335` —— branch、ahead/behind、changed files、PR+checks、merge/rebase/cherry-pick 等 attention 标签(`:182-190`)。数据来自 OC 自有 git 栈(`useGitStore`/`useGitHubPrStatusStore`)。
- **usage quotas 节**:`WorkStatusUsageSection` + `work-status/usageHeadline.ts:20-26`(quota provider 别名 openai→codex、anthropic→claude、gemini→google)、`:43-69`(`pickUsageHeadline` 取最短窗口配额做头条)。数据源 `stores/useQuotaStore.ts:100,166`(`/api/config/settings` + `/api/quota/{providerId}`,OC 自有 provider 配额抓取)。
- **tasks 节**:`WorkStatusTasksSection.tsx:41-55` —— live `state.todo[sessionId]`(`todo.updated` wire 事件)优先,`useTodosPersistStore` 兜底;`:25-29` 按 in_progress/pending/completed 排序。事件语义归 ch05,面板/持久化为 OC 原创。
- **subagents 节**:`WorkStatusSubagentsSection.tsx:19-23`(docstring:子会话的 permission 请求在 transcript 无表示,此面板是唯一可见处)、`:30-33`(children = `liveSessions.filter(parentID === sessionId)`)、`:37-38`(订阅 `state.permission`/`state.question`)、`:44-53`(空→出现沿自动展开)、`:75`(busy 计数)。**其"阻塞可见性"职责建立在 ch07 将删除的 permission/question 协议上。**
- **MCP 节**:`WorkStatusMcpSection.tsx:22-40` —— 每目录 MCP server 连接开关,数据 `useMcpStore.getStatusForDirectory`。当前 omp-host 下 OpenCode MCP 面为空(master §2.1"MCP 空面")。
- **pinned 节 / context sources 节**:`WorkStatusPinnedSection`(WorkStatusPanel.tsx:14,258);`WorkStatusContextSection.tsx:19-40` —— 该 session 指向的 GitHub 线程(`lib/linkedIssues.ts`)+ 可用 skills/MCP 计数。
- **Header 内降级显示**:`components/layout/Header.tsx:2097-2114` `ContextUsageDisplay`(WorkStatusPanel 隐藏时显示 token 总量 + %)。与 TUI footer 的 `context_pct`/`cost` 段同位(见 §3)。

### 2.2 multirun / AgentManager(多模型并行)

- 视图:`components/views/agent-manager/AgentManagerView.tsx:34-40`(groups + createMultiRun)、`AgentManagerSidebar.tsx:48-181`(分组列表、删除连 worktree `:56-68`)、`AgentGroupDetail.tsx:35-44,145-150`(每 session 状态点、组级 busy)、内嵌 `ChatContainer`(`AgentGroupDetail.tsx:11`)。
- 创建表单:`AgentManagerEmptyState.tsx:26`(`MAX_MODELS = 5`)、`:53-54`(selectedModels/selectedAgent 本地 state)、`:506-534`(multirun `AgentSelector` + `ModelMultiSelect`)、`:309-316`(合法性:组名+prompt+≥1 模型+基分支+git 仓库)。
- 输入组件:`components/multirun/AgentSelector.tsx:29-52`(`getVisibleAgents()` + `isPrimaryMode` 过滤,默认取 `currentAgentName`);`components/multirun/ModelMultiSelect.tsx`(370 行,显式 provider/model/variant 多选)。
- 执行:`stores/useMultiRunStore.ts:118-337` `createMultiRun` —— git 仓库且未关隔离时逐模型建 worktree(`:158-163,230-240`)、建 session(`:252-258`)、再逐 run `routeMessage({providerID, modelID, variant, agent, files})`(`:296-321`)。**每 run 显式 pin 模型 + agent + variant,这正是产品语义(同 prompt × N 模型),但默认值解析走 OC 级联(见 2.7)。**
- 分组来源:`stores/useAgentGroupsStore.ts:53-77` `parseSessionTitle` —— 按 `groupSlug/provider/model[/index]` 会话标题约定反解析归组。**分组是字符串约定,非持久实体。**
- 融合:`components/multirun/MultiRunFusionDialog.tsx` + magic prompt `session.fusion.*`(`lib/magicPrompts.ts:52-53`)。

### 2.3 scheduled tasks(定时任务)

- 类型与 API:`lib/scheduledTasksApi.ts:5-41`(`ScheduledTask`:schedule daily/weekly/once/cron + timezone;execution = prompt + `providerID/modelID/variant/agent/goalEnabled/goalTokenBudget/permissionAutoAccept`;`loopFile` 指向 `.agents/loops/*.md`)、`:63-94`(`GET/PUT /api/projects/{id}/scheduled-tasks`)。
- 编辑器:`components/session/ScheduledTaskEditorDialog.tsx:14-15`(复用 `sections/agents/ModelSelector` + `sections/commands/AgentSelector`)、`:737-751`(新建任务默认值 = `currentProviderID/currentModelID/currentVariant/currentAgentName`,即 OC 级联解析后的当前选择)、`:1500-1516`(ModelSelector 必填)、`:1519-1548`(thinking-level = variant 下拉)、`:1551-1563`(AgentSelector + `isPrimaryMode` 过滤)、`:1688-1701`(permissionAutoAccept 开关)、`:1174-1178`(保存载荷)。
- 服务端 runner:`web/server/lib/scheduled-tasks/runtime.js:455-470`(`prompt_async` 载荷:`model:{providerID,modelID}` + agent + variant + parts)、`:491-518`(`/command` 透传,`model: "provider/model"` 字符串)、`:560-588`(执行前 enroll permissionAutoAccept;goalEnabled 时 `createSessionGoal({objective, tokenBudget, providerID, modelID})`)、`:389-393`(`.agents/loops` 文件与持久任务表的对账:文件在则权威,删文件即解除调度)、`:75-83`(prompt 以 `/` 开头走 command 解析)。
- **goal 结算推送**:`web/server/lib/notifications/runtime.js:713-745` `sendGoalSettlePush`(goal_complete/goal_blocked/goal_budget 三型,依赖 OC session-goal runtime)。

### 2.4 projects / worktrees

- `stores/useProjectsStore.ts:47-73`:项目 CRUD、颜色/图标、**`defaultModel` 项目级默认模型元数据**(`:62`)、VS Code workspace 同步、desktop settings 持久化。
- worktrees:`lib/worktrees/*`(worktreeManager/worktreeCreate/worktreeBootstrap/worktreeStatus)、`components/views/WorktreesView.tsx`、会话-Worktree 迁移 `lib/worktrees/sessionWorktreeMove.ts`、setup commands 持久化(`useMultiRunStore.ts:7` `getWorktreeSetupWaitEnabled/saveWorktreeSetupCommands`)。sidebar 按 worktree 分组(`components/session/sidebar/DOCUMENTATION.md:3`)。**纯 OC 原创,omp 无对应物。**

### 2.5 GitHub 集成

- Issue→会话:`components/session/GitHubIssuePickerDialog.tsx:237-252`(`resolveDefaultAgentName`,**`'build'` 硬回退 `:249`**)、`:254-273`(`resolveDefaultModelSelection`,读 `settingsDefaultModel`)、`:275-297`(variant 解析)、`:433-489`(发起时 default→current→lastUsed 三级兜底取模型/agent;magic prompts `github.issue.review.*` 组装 visible+instructions+context 三段;`setLinkedIssue` 落上下文源)。
- 同构副本:`components/session/NewWorktreeDialog.tsx:442-445,486-490,615-619`(同一套 resolver 复制粘贴)。
- 其余:PR 视图(`views/PullRequestView.tsx`、`git/PullRequestSection.tsx`)、PR 状态/上下文 store(`useGitHubPrStatusStore.ts`、`usePrContextStore.ts`)、linked issues(`lib/linkedIssues.ts`,作为 additionalParts 注入 `buildOutgoingMessage.ts:115-231`)、GitHub 魔法提示(`git.commit.generate`/`git.pr.generate`/`github.pr.review` 等,`lib/magicPrompts.ts:4-21`)。

### 2.6 magic slash commands 与命令面

- 定义:`components/chat/composer/submit/slashCommands.ts:22-40`(`MagicPromptCommand`:visible+instructions 双提示对)、`:61-129`(九条:`summary/workspace-review/plan-feature/craft-goal/schedule-task/catch-up/debug/weigh/explore`)、`:155-170`(parse/canRun/buildVariables)。
- 执行路径:`sync/session-ui-store.ts:152-193` —— 首词 `/name` 先查同步命令(wire `GET /command`)**再查 OC commands store、再查 skills store**,命中则 `sendCommand({command, arguments, agent, model, variant, files})`;未命中落到 ChatInput 的 magic-prompt 分支;都不是才作为普通文本。
- composer 本地处理:`components/chat/ChatInput.tsx:546-558`(`knownSlashNames` 硬编码 `init/review/undo/redo/timeline/compact/summary/…/explore` + handoff-review)。
- omp-host 现状:`web/server/lib/omp-host/endpoints.js:393`(`GET /command` 返回 `[]` —— 同步命令表恒空)、`:273-284`(`POST /session/{id}/command` 把命令文本**作为 prompt 转发**,注释:omp 在 session 物化时自行展开斜杠命令)。
- 引擎侧已具备的展开链:SDK `session.prompt()` 对 `/` 开头文本依次尝试 extension 命令 → custom TS 命令 → 文件式 markdown 命令(SDK `session/agent-session.ts:5318-5342`),TUI 另有 ~130 个内建 `/命令`(见 §3.4)。

### 2.7 默认模型/agent 级联(所有原创面输入的共同上游)

`stores/useConfigStore.ts:282-293`(注释级联)+ `:294-399` `resolveDefaultAgentModelSelection`:

- Agent:`settings.defaultAgent`(OC host)→ OpenCode `default_agent` → **名为 `build` 的 agent** → 首个 primary → 首个(`:329-348`,build 硬回退 `:344`)。
- Model:`project.defaultModel` → `settings.defaultModel`(OC host)→ agent pin → OpenCode `config.model` → **硬编码 `opencode/big-pickle`** → 首个(`:350-396`)。
- 调用点:`useConfigStore.ts:2204-2210,2581-2586,2698-2699`;表单侧另有 GitHubIssuePickerDialog/NewWorktreeDialog 的本地副本(2.5)与 ScheduledTaskEditorDialog 的 `currentXxx` 默认(2.3)、multirun AgentSelector 的 `currentAgentName` 默认(2.2)。**这正是 master §2.3 所指"输入须迁移到 omp role 体系"的全部面。**

### 2.8 队列(steer/queue)

- 行为开关:`stores/messageQueueStore.ts:9-15`(`FollowUpBehavior = 'steer' | 'queue'`,默认 queue;legacy `'immediate'` 已折叠为 steer,注释"OpenCode 只支持 steer|queue")。
- 提交分派:`components/chat/ChatInput.tsx:1362-1366` —— busy+有内容时:queue 行为 → `handleQueueMessage()`(入 client 队列);steer 行为 → 立即 `handleSubmit({delivery:'steer'})`。**queue 模式从不向引擎发送 `delivery:'queue'`,引擎 followUp 队列路径对 UI 不可达。**
- 引擎对齐(已完成):`web/server/lib/omp-host/engine.js:695-702`(`delivery==='queue' → streamingBehavior 'followUp'`,否则 steer;经 `prompt()` 而非 `steer()` 以保 `/` 命令中turn可用),测试 `omp-host.engine.test.js:71-91`。
- client 队列语义:`messageQueueStore.ts:63-64`(上限 50 目标 × 每队列 20 条)、persist 中间件(`:139-142`,跨刷新持久);派发 `hooks/useQueuedMessageAutoSend.ts:238-320`(**idle 才派、每次一条**(head,`:264`)、abort 后 2s 保持窗(`:16,70-77`)、auto-review 运行中挂起(`:252-255`)、捕获时 sendConfig 优先否则当前配置(`:277-288`)、失败指数退避 2s→60s(`:18-19`))。
- 队列 UI:`components/chat/QueuedMessageChips.tsx`(逐 chip send/edit/pop-to-input + dnd 拖排序);`composer/ui/ComposerActionButtons.tsx` docstring(idle→send;busy+内容→浮起 queue 按钮;busy→stop)。

### 2.9 通知系统

- 设置层(OC 自有,保留对象):`stores/useUIStore.ts:722-731`(`nativeNotificationsEnabled/notificationMode/notifyOnSubtasks/notifyOnCompletion/notifyOnError/notifyOnQuestion` + 模板)、`:1057-1069`(默认值)、`:2283-2285`(setter)、`:2674-2684`(持久化字段)。
- 触发层(迁移对象):`web/server/lib/notifications/runtime.js` `maybeSendPushForTrigger` —— `message.updated` 且 `info.finish==='stop'` → ready(`:333-432`,5s 冷却、模板变量、subtask 用 subtask 模板 `:378-380`);`finish==='error'` → error(`:434-502`);`question.asked` → 输入提醒(500ms 防抖,`:507-585`);`permission.asked/replied` → 审批提醒(500ms 防抖 + requestKey 去重 + auto-accept 抑制,`:587-707`);goal 结算(`:713-745`)。subtask 抑制用 parentID 缓存(`:115-171`)。
- 交付层(保留对象):desktop/OSC 通知 + UI SSE 广播(`emitter-runtime.js`,SSE 事件类型 `openchamber:notification`,路由 `routes.js:219-277` `GET /api/notifications/stream`)、web-push + 可见性心跳(`push-runtime.js`)、iOS APNs(中继/直发双模,`apns-runtime.js`,通用文案 `APNS_TITLE_BY_TYPE` runtime.js:52-60,徽章按 tag 计数 `:23-48`)、web 端 `hooks/useWebNotificationStream.ts:8,37-55`(EventSource + 设置门控)。通道级 fanout 与"桌面可见则不发手机"路由:`runtime.js:77-96`。
- 桌面 tray:`hooks/useTraySync.ts:113-117,274-275,597`(审批菜单项,建立在 permission 协议上 → ch03/ch07)。

### 2.10 files / terminal / browser / 其他视图

- `views/FilesView.tsx`(4381 行)、`views/TerminalView.tsx`(1134 行,配套 `web/server/lib/terminal/*` PTY 运行时)、`layout/ContextPanel.tsx`(1240 行:文件树/git/diff/嵌入式会话面板)、`views/GitView.tsx`/`DiffView`/`PierreDiffViewer`/`DiagramView`/`PullRequestView`。**全部 OC 原创;omp 仅有 `/browser headless|visible` 等命令级能力,无同类 GUI 面。**
- 次要原创面:CommandPalette(`ui/CommandPalette.tsx`)、PromptNavigatorRail、ThinkingPill、mini chat(`renderElectronMiniChatApp.tsx`)、walkthrough(`web/server/lib/walkthrough/*` + `components/walkthrough/*`)、TTS/听写(`useServerTTS/useSayTTS/useLocalTTS/useDictation`)、inline diff comments(`useInlineCommentDraftStore`)、export-as-markdown(`lib/exportSession.ts`)。
- plan 实验面(处置=退役,归 ch02):`useFeatureFlagsStore.ts`(flag `planModeExperimentalEnabled`,App.tsx:415-416 注入)、`hooks/usePlanDetection.ts:29-52`(以合成文本 `'User has requested to enter plan mode'`/`'The plan at '` 扫描检测)、`views/PlanView.tsx` + plan rail(`lib/surfaces/registry.ts:186-187` 门控)。

---

## 3. 目标语义(omp / TUI 侧)

### 3.1 队列与 followUp

- 提交语义:SDK `session.prompt(text, {streamingBehavior})`,`streamingBehavior` 缺省 `followUp`(SDK `main.ts:293-334`);TUI Enter = `steer`,`app.message.followUp` 和弦(Ctrl+Enter,Windows 终端回退 Ctrl+Q)= `followUp`(`config/keybindings.ts:34,136-138,523-525`;`modes/controllers/input-controller.ts:1367-1369` 注释)。
- 引擎队列:`agent-session.ts:5983-6022` `#queueUserMessage`(steer/followUp 双队列,图片描述伴随消息);空闲排空 `:6024-6056`,门控 `:6061-6083`(`#canAutoContinueForFollowUp`:steer 可从任意尾部恢复;followUp-only 在用户主动打断后抑制自动恢复)。
- 排空模式:设置键 `followUpMode: 'all' | 'one-at-a-time'`,默认 **one-at-a-time**(`config/settings-schema.ts:1660-1663`;`session/agent-session.ts:7005-7006` setFollowUpMode 会写设置)。
- 出队:`app.message.dequeue`(Alt+Up/Shift+Up,`keybindings.ts:36,147-150`)→ `restoreQueuedMessagesToEditor`(`input-controller.ts:1139-1146,1439-1454`):`session.clearQueue({forInterrupt})` **整批取回 steering+followUp(含 compaction 期暂存)回编辑器**;Esc 打断时丢弃非用户 steer(`:1441-1443`)。空提交 + 流中 + 有队列 → abort 本turn后排空(`:650-653`)。
- 队列显示:pending messages 容器列出 steering/followUp 文本 + `Alt+Up to edit` 提示(`modes/utils/ui-helpers.ts:892-924`);队列简写 `->`/`=>` 与编号列表自动拆分(`modes/queue-input.ts:114-120`)。**TUI 无逐条编辑/拖排序;取回是整批语义。**

### 3.2 usage / context / cost 呈现位

- TUI footer:pwd+branch、↑in ↓out R/W cache、`$cost` ★premium、彩色 context%(可选 auto 标)(OmpProductSurface 调研,`components/footer.ts:152+`)。
- 状态栏(可选)24 段:`config/settings-schema.ts:163-188`(`StatusLineSegmentId` 含 `token_in/out/total/rate`、`cost`、`context_pct/total`、`cache_read/write/hit`、`usage`、`subagents`、`mode`…)。`usage` 段语义 = **tier + 5h/7d/monthly 配额窗口**。
- 逐turn usage row:assistant 消息尾部暗色行 `HH:MM:SS ↑in ↓out ⛁cache ⏱ttft ⚡tok/s`(`components/usage-row.ts`,归 ch05)。

### 3.3 Agent Hub 与子 agent 数据

- live roster(`components/agent-hub.ts`,Alt+A/Ctrl+S):状态计数 running/idle/parked/aborted、逐 agent task+activity;r 复活 parked、x 中止、chat 视图输入;parked = 空闲子 agent 按 `task.idleParkMs` 换页到磁盘(`settings-schema.ts:4674`),消息/IRC 自动复活(`session/irc-bridge.ts` 经 `irc/bus.ts:130-152`)。OpenChamber 侧 omp-host 已有私有 AgentRegistry(master 开放问题 4)。
- 状态段 `subagents`(settings-schema.ts:169)在状态栏有专门段 —— 会话级子 agent 汇总在 TUI 也是一等公民,WorkStatus subagents 节与其同位不同面(见 §5.5 所有权)。

### 3.4 斜杠命令体系

- TUI 内建 ~130 个 `/命令`(slash-commands/builtin-*.ts,名字全集由源码 `name: "…"` 枚举),关键碰撞对象:`debug`(调试工具选择器,`builtin-lifecycle.ts:282-287`)、`compact`(手动压缩,带 soft/remote/snapcompact 子命令,`builtin-lifecycle.ts:122-129`)、bundled custom command `review`(`extensibility/custom-commands/bundled/review/index.ts:476`)、`new`/`model`/`agents`/`plan`/`goal`/`queue`/`usage`/`share` 等。
- 引擎(SDK)层展开链:`session.prompt()` 对 `/` 文本依次 extension 命令 → custom TS 命令 → 文件式 markdown 命令(`session/agent-session.ts:5318-5342`);skills 以 `/skill:<name>` 走 `promptCustomMessage`(`modes/skill-command.ts:46-68`)。**TUI 内建命令在 TUI 进程内分发,不经 SDK prompt;引擎只会展开 extension/custom-TS/markdown 三类。**

### 3.5 通知触发与设置

- 触发:`agent_end` 事件读取本turn最后一条 assistant 的 `stopReason`:error → `sendErrorNotification`(跳过 `isTerminal===false` 的非终局 settle 与 retry 窗口,`modes/controllers/event-controller.ts:2206-2248`);非 aborted/error → `sendCompletionNotification`(`:2250-2273`,两者互斥)。ask 工具等待输入由 `ask.notify` 治理(`settings-schema.ts:1961-1971`)。
- 设置键(TUI 本地):`completion.notify`(默认 on)、`error.notify`(默认 off)、`ask.notify`(默认 on)、`ask.timeout`(`settings-schema.ts:1919-1959`)。
- 相关引擎事件:`agent_end {messages, willContinue, isTerminal}`(SDK `extensibility/shared-events.ts:193-197`;`session/agent-session-events.ts:12-17` isTerminal 语义)、`notice {level: info|warning|error, message, source}`(`agent-session-events.ts:55`)、`tool_approval_requested/resolved {sessionId, toolCallId, toolName…}`(`extensibility/extensions/types.ts:871-883`)。
- goal 模式(ch02 域):`/goal set|drop|pause|resume|budget|status`、`goal_updated` 事件、budget 限额、状态图标 active/paused/complete/budget-limited/dropped(status line mode 段)。

---

## 4. 差距清单

| 编号 | 差距 | 分类 | 优先级 | 风险 |
|---|---|---|---|---|
| GAP-01 | 四处原创面输入(multirun/scheduled/GitHub issue/NewWorktree)各自复制 `resolveDefaultModelSelection`/`resolveDefaultAgentName` 且 `useConfigStore` 级联含 `build` 硬回退与 `opencode/big-pickle` 兜底;未接 ch01 roles | 改 | **P0** | 中:四处行为不一致;漏改一处即"平行宇宙"复发 |
| GAP-02 | multirun/AgentSelector、ScheduledTaskEditorDialog 的 `isPrimaryMode` 过滤与 `currentAgentName` 默认基于 OpenCode agent 模型;omp agents(ch02)下无 primary 概念 | 改 | P1 | 低 |
| GAP-03 | scheduled task execution 字段迁移:`permissionAutoAccept` 删除(R10:无人值守 fail-closed,不依赖任何 per-session 审批端点,03 §5.3.3;上游 overlay 归 03 OQ-3)与 `goalEnabled/goalTokenBudget`(→ ch02 goal,P1 投影/P2 续跑) | 改+删 | P1 | 中:存量任务文件兼容、无人值守语义不能断 |
| GAP-04 | WorkStatus subagents 节的阻塞显示订阅 `state.permission`/`state.question`(ch07 删);子列表仅 wire parentID,无 parked/aborted 状态 → 数据源 = ch04 `/api/omp/agent-runs`(R3) | 改 | **P1** | 中:该节是子会话阻塞唯一可见处,迁移窗口不能留空窗 |
| GAP-05 | WorkStatus goal row 数据源为 OC session-goal(session.updated metadata + server runtime);应吸收 ch02 goal 投影 —— P1 数据 = ch02 投影事件/快照 + 显式操作(02 GAP-B08);自主续跑 = P2(02 GAP-B14,R12) | 改 | **P1**(投影)/P2(续跑) | 中:双 goal 系统并存期用户困惑 |
| GAP-06 | WorkStatus usage 节数据模型(别名映射 + 任意窗口行)与 omp usage 段语义(tier + 5h/7d/monthly)未对齐 | 改 | P3 | 低 |
| GAP-07 | WorkStatus MCP 节 + ContextPanel MCP 计数在 omp 下无数据(OpenCode MCP 空面);ch04 §5.7.5 可执行端点落地前只读 + 开关禁用(07 §5.10 Step 1 同步,R12),数据源切换随 ch04 | 改 | P1(只读化)/P2(数据源) | 低:空态渲染已是默认行为 |
| GAP-08 | 斜杠命令解析无统一管线:wire `/command` 恒空、magic 命令在 sendCommand 未命中后兜底;与 omp ~130 内建命令存在名字碰撞(`debug`/`compact`/`review`)无处理规则;裁决 = 发现走 `GET /api/omp/commands`,wire `/command` 空返至 ch07 删除(R12) | 建+改 | **P1** | 高:碰撞即语义劫持(用户输入 `/debug` 得到非预期行为) |
| GAP-09 | 通知触发建立在 wire `message.updated(finish)`/`question.asked`/`permission.asked` 上;omp 侧 = ch05 注册表事件(`agent_end` isTerminal 分支、`omp.notice.raised`、`omp.dialog.requested/settled`),经 ch05 唯一通道(R1) | 改 | **P1** | 中:ch07 删协议前必须切换,否则 ready/error/审批通知全哑 |
| GAP-10 | client queue 与引擎 followUp 队列双轨:queue 模式永不发 `delivery:'queue'`;多客户端(VS Code/web/mini chat)互相看不见队列;chip 逐条编辑/拖排序为 OC 增值,TUI 为整批取回;方案 B(含写入协议 requestId 幂等 + server ack + outbox)**整体 gated 于上游 SDK 队列扩展**(入队带稳定 ID + 快照携带 ID + 带 version 变更事件;SDK 现状支撑不了该协议,§5.7 事实核,R2-H6)+ 05 §5.2.4 矩阵 + capabilities `queue.v1`(前置合入前恒 false) | 改 | P2(SDK 扩展前不可启动) | 中:多客户端一致性 vs 现有 chip UX;上游落地时点不可控 |
| GAP-11 | 保留面所有权未成文:projects/worktrees/files/terminal/browser/git/GitHub 视图、通知设置层与交付通道、CommandPalette 等 —— 与 ch02/04/05 计划面的元素级归属需要地图,防重复建设 | 留 | **P0**(成文) | 低 |
| GAP-12 | Header `ContextUsageDisplay` 与 WorkStatus session 节共用 `contextUsage`(消息 token 求和);ch05 事件化后需单一数据源声明 | 改 | P2 | 低 |
| GAP-13 | plan 实验面(合成文本标记检测 + PlanView + rail)与 ch02 plan mode 重复;magic `/plan-feature` 与 plan mode 概念易混 | 删 | P2 | 低:flag 默认关(`useFeatureFlagsStore.ts` 默认 false) |

---

## 5. 设计方案

### 5.0 处置总表(覆盖 §2 全部原创面)

| 面 | 处置 | 说明 |
|---|---|---|
| WorkStatusPanel 容器/节开关/presence | **keep-as-is** | 纯展示壳,无 OpenCode 依赖(WorkStatusPanel.tsx:64-75) |
| └ session 节(context+cost) | **adapt-inputs** | 数据源切 ch05 context 事件;`session.cost` 由 omp-host 投影继续供给(§5.6) |
| └ repository 节 | **keep-as-is** | OC git 栈自有 |
| └ goal row | **absorb-omp** | 渲染 ch02 goal 投影(`omp.goal.updated` + `GET …/goal`;P1 投影/显式操作、P2 续跑);OC session-goal runtime 退役(GAP-05) |
| └ usage quotas 节 | **adapt-inputs**(轻) | 行模型对齐 omp usage 段 {tier, windows}(GAP-06) |
| └ tasks 节 | **keep-as-is** | 事件语义/todo_auto_clear 归 ch05;面板与持久化不动 |
| └ subagents 节 | **absorb-omp** | 子列表/状态接 ch04 `/api/omp/agent-runs`(R3,含 parked);阻塞显示接 ch03 对话框事件(`omp.dialog.*`,经 ch05 通道;GAP-04) |
| └ MCP 节 | **只读化 + 禁用**(R12) | ch04 §5.7.5 可执行端点落地前只读呈现、连接开关禁用、无 fake-success 操作;数据源切换随 ch04(GAP-07) |
| └ pinned / context sources 节 | **keep-as-is** | linkedIssues/skills 计数;skills 计数源迁 omp skills(ch04 skill://) |
| Header ContextUsageDisplay / ThinkingPill | **keep-as-is** | TUI footer context% 的 OC 同位物;单一数据源声明(GAP-12) |
| AgentManager 视图×5 + 融合对话 | **keep-as-is** | multirun 产品面保留 |
| └ multirun AgentSelector | **adapt-inputs** | 改 persona 选择器(ch02 `/api/omp/personas`,默认"标准";worker 定义不进 multirun,02 D-B2);去 `isPrimaryMode`(GAP-02) |
| └ multirun ModelMultiSelect | **keep-as-is**(显式 pin) | N 模型并行 = 显式选择是产品语义;默认首项 = role default(GAP-01) |
| └ useMultiRunStore.createMultiRun | **adapt-inputs** | `routeMessage` 的 model/agent/variant 参数改可省略(role 解析下沉引擎;agent 参数语义 = persona);标题约定扩展 role 名 + persona 段(agentGroupsStore.ts:53-77) |
| ScheduledTaskEditor/Dialogs/列表 | **keep-as-is**(壳) | |
| └ 编辑器 Model/Agent/variant 输入 | **adapt-inputs** | 默认值 ch01 roles;agent 输入 → persona 选择器(ch02 `/api/omp/personas`,默认无);variant→thinking level(ch01)(GAP-01/02) |
| └ `permissionAutoAccept` | **retire**(R10) | 删除;无人值守 fail-closed、不获会话级豁免、不改全局审批设置;编辑器仅检测警告;上游 overlay 归 03 OQ-3(§5.3) |
| └ `goalEnabled/goalTokenBudget` | **absorb-omp** | → ch02 goal(`POST /api/omp/sessions/{id}/goal` op=set;P1 投影/显式操作、P2 续跑;§5.3) |
| └ 服务端 runner(prompt_async/command) | **adapt-inputs** | model 可省略(引擎 role 解析);loops 对账保留 |
| └ OC session-goal runtime + sendGoalSettlePush | **retire**(迁移后) | goal 结算改由 ch02 `omp.goal.updated` 驱动通知(P1 投影落地后;§5.3) |
| projects(useProjectsStore)/worktrees 全家 | **keep-as-is** | 唯一改动:`project.defaultModel` 元数据语义 → role 指针(ch01/ch06 联动) |
| GitHub 集成(pickers/PR/linkedIssues/magic prompts) | **keep-as-is** | |
| └ GitHubIssuePickerDialog/NewWorktreeDialog 本地 resolver 副本 | **adapt-inputs** | 删除副本,统一消费 ch01 解析器(GAP-01) |
| magic slash commands ×9 | **keep-as-is** + 碰撞治理 | 三层管线,`/debug` 让名(GAP-08,§5.4) |
| composer 本地命令(init/undo/redo/timeline/compact/handoff-review) | **改边界** | `/compact` 让位 omp 语义;其余保留为 OC 内建层(§5.4) |
| 通知:设置层 + desktop/SSE/web-push/APNs 交付 + tray 壳 | **keep-as-is** | |
| 通知:触发层 | **absorb-omp** | 事件源切 ch05 注册表事件(`agent_end` isTerminal 分支/`omp.notice.raised`/`omp.dialog.*`),经 ch05 唯一通道(R1;GAP-09,§5.6);notice 仅 toast、不触发系统通知(§5.6 权威规则) |
| messageQueueStore/Chips/AutoSend | **keep**(现行,方案 A)/ **改**(终局,方案 B;gated) | 方案 B 切换门槛 = 上游 SDK 队列扩展三件套(R2-H6 前置)+ 写入协议 + 05 §5.2.4 矩阵 + capabilities `queue.v1`(前置合入前恒 false;R12;§5.7) |
| files/terminal/browser/context/git/diff/diagram 视图 | **keep-as-is** | 无 omp 重复面 |
| CommandPalette/PromptNavigatorRail/mini chat/walkthrough/TTS/听写/inline comments/export | **keep-as-is** | |
| plan 实验面(PlanView/plan rail/usePlanDetection) | **retire** | ch02 plan mode 取代(GAP-13);magic `/plan-feature` 保留(是提示模板,非模式) |
| tray 审批菜单项 | **adapt-inputs** | 响应源切 ch03 对话框事件(`omp.dialog.requested/settled` + respond,经 ch05 通道;壳保留) |

### 5.1 GAP-01:统一 role-based 输入解析(输入迁移规格)

**问题**:同一"默认模型+agent+variant"解析存在 6 份实现(useConfigStore 主级联 + GitHub picker + NewWorktree + ScheduledTask 默认 + multirun AgentSelector 默认 + multirun 表单隐式),其中 2 份含 `'build'` 硬回退(GitHubIssuePickerDialog.tsx:249;useConfigStore.ts:344),1 份含 `opencode/big-pickle` 兜底(useConfigStore.ts:384-396)。

**设计**(契约归 ch01,此处为消费规格):

- ch01 提供 `GET /api/omp/models?directory=…`(D1 RuntimeAPIs 面,01 §5.3 形状:模型列表 + `roles` 快照 + `cycleOrder` + `legacyDefaults`);本域经它取 `roles.default/smol/slow` 的解析结果。
- 新建单一 UI 解析器 `resolveOmpDefaults({ directory, role?: 'default' })` → `{ model?: {providerID, modelID, thinkingLevel?}, persona?: string }`,放 `packages/ui/src/lib/omp-defaults.ts`(新文件);内部只消费 ch01 角色快照 + ch02 persona 列表(`/api/omp/personas`;`persona` 缺省 = 标准,02 D-B2),**不再有 build/big-pickle 回退**——解析不到就返回 undefined 并让表单显示"跟随默认(role)"态,由引擎侧 settings 默认兜底(engine.js 已不 pin fallback model,`omp-host/DOCUMENTATION.md:95-98`)。
- 调用点改造(全部为默认值来源替换,表单结构不动):
  1. `AgentManagerEmptyState.tsx:53-54` —— `selectedModels` 初始 = `[]`,占位文案改"跟随默认模型(role: default)";首项由用户显式添加(产品语义 = 显式 pin)。`selectedAgent` 默认空 = 引擎默认 agent。
  2. `ScheduledTaskEditorDialog.tsx:745-751,806-825` —— `toDraft(task, defaults)` 的 defaults 改为 `resolveOmpDefaults()` 结果;ModelSelector 增加一个"跟随默认 role"空选项(值 `''`),校验 `:582-584` 相应放宽(modelID 可空 = role 跟随)。
  3. `GitHubIssuePickerDialog.tsx:254-297` 与 `NewWorktreeDialog.tsx:442-445` —— 删除两个本地 resolver,发起处(`GitHubIssuePickerDialog.tsx:433-445`、`NewWorktreeDialog.tsx:486-490`)改:`providerID/modelID/variant` 全部可省略,`sendMessage` 不带 model 参数(session-ui-store `routeMessage` 的 `providerID/modelID` 参数改 optional,透传到 wire body 时省略字段,omp-host `engine.prompt` 的 `model` 为 undefined 即走 SDK settings 默认,engine.js:619-648 已支持)。
  4. `multirun/AgentSelector.tsx:29-52` —— 改 persona 选择器:数据源 = ch02 `/api/omp/personas`(默认"标准" = 无 persona);去掉 `isPrimaryMode` 过滤;worker 定义(`/api/omp/agent-definitions`)不进 multirun 选择器(worker 经 task 工具/ch04 Hub 派发,02 D-B2);默认值 `currentAgentName` → 空(标准)。
- `useConfigStore.resolveDefaultAgentModelSelection`(294-399)保留为 legacy 路径但删除 build/big-pickle 分支,随 ch01 P0 落地后由 `resolveOmpDefaults` 取代;`GitHubIssuePickerDialog.tsx:237-252` 的 `resolveDefaultAgentName` 同步删除。

**备选与取舍**:(a) 各表单直接读 `modelRoles.default` —— 简单但重复 6 次+失去"解析不到"的统一语义;(b) 服务端解析(表单只存 role 名,发起时引擎解析)—— 对 scheduled task 是终局(§5.3),但对交互式表单会让"显示当前会讲用什么模型"变异步。**采用:UI 单一解析器 + 发起参数可省略双保险。**

### 5.2 GAP-02:化身输入分型(persona vs worker)与 multirun 语义

- **分型裁决(02 D-B2/R12)**:顶层会话的"agent"输入 = **persona**(OpenChamber 原创可选层,独立资源 `/api/omp/personas`,默认无,"标准" = 无 systemPrompt 覆盖;02 §5.2a);**worker 定义(`/api/omp/agent-definitions`,02 §5.2)不进 multirun/scheduled/GitHub/NewWorktree 选择器** —— worker 经 task 工具派发、运行在子会话,其可见面 = ch04 `/api/omp/agent-runs`(§5.5 subagents 节)。`/api/omp/agents` 路径已废弃(02 R3 拆分),本章不得引用。
- 旧 OC 级联的 `opencodeClient.listAgents` + `/api/config/agents/{name}` scope 组合(useAgentsStore.ts:302-375 的 OC 附加 scope/group 字段保留为 OC 元数据)随定义面切 `/api/omp/agent-definitions`(CRUD 归 ch02;本域只读消费 persona/定义名)。
- `isPrimaryMode` 全部调用点(multirun/AgentSelector.tsx:48-52、ScheduledTaskEditorDialog.tsx:1555、ModelControls.tsx:503-505 归 ch02)删除;选择器不过滤。multirun 的 `routeMessage` agent 参数语义 = persona 名(engine 侧 `meta.persona`,02),省略 = 标准。
- multirun 标题约定(useAgentGroupsStore.ts:53-77)扩展:role 跟随的 run 标题用 role 名(如 `group/default/2`),显式 pin 保持 `provider/model`;persona ≠ 标准时以附加段表达(如 `group/default/2/alice`),默认不占段、兼容既有解析。`getMultiRunSessionTitle`(`lib/multirun/title.ts`)同步。

### 5.3 GAP-03:scheduled task execution 字段迁移

数据模型(`scheduledTasksApi.ts:21-30` → 目标形状,向后兼容读取):

```ts
execution: {
  prompt: string;
  /** 显式 pin;省略 = 跟随 modelRoles.default(发起时解析) */
  providerID?: string; modelID?: string; variant?: string;   // 旧字段保留可空
  modelRole?: 'default' | 'smol' | 'slow';                    // 新:role 引用
  persona?: string;                                           // ch02 persona 名(可选层,默认无;02 §5.2a)
  goal?: { enabled: boolean; tokenBudget?: number };          // 取代 goalEnabled/goalTokenBudget(ch02)
}
```

- **pin vs role 决策**:默认新任务 = `modelRole: 'default'`(无人值守也应跟随用户后来的 role 调整);编辑器保留"固定模型"开关显式写 providerID/modelID(可复现性)。服务端 runner(`scheduled-tasks/runtime.js:455-470`)在 `modelRole` 存在且无 pin 时**省略 model 字段**发给 `prompt_async`,由 omp-host 引擎按 settings 解析(同 5.1);显式 pin 路径不变。
- `permissionAutoAccept` → **删除(R10,REVISED)**:v1 设计的 per-session 审批模式端点依赖**取消** —— 该端点不存在、从未在任何章定义,ch03 明确不提供 per-session approvalMode、产品面也无会话级审批开关(03 §5.3.3 D-C4)。无人值守任务 **fail-closed**:不改全局 `tools.approvalMode`/`tools.approval`、不获任何会话级豁免;capability `dialogs.v1` 上线后,任务会话遇 prompt 档工具 → SDK wrapper 既有 fail-closed 错误,任务步失败且原因进任务诊断与 transcript(capability 关闭时即现状:非 yolo 工具直接抛错)。编辑器创建时检测全局 `approvalMode ≠ yolo` 即警告"无人值守运行将在需要审批的工具上失败"(03 §5.3.3 同款)。旧字段 `permissionAutoAccept` 读取即忽略(P1 起停止生效、不映射任何审批语义;P3 随 03 GAP-C6 删除 UI 输入);上游会话级 settings overlay 是唯一增强路径(03 OQ-3,落地前不提供)。
- `goalEnabled` → `goal`:runner 不再调 OC `createSessionGoal`(`runtime.js:574-588`),改经 ch02 goal 契约(`POST /api/omp/sessions/{id}/goal`,body `{op:'set', objective, tokenBudget}`,02 §5.6);goal 结算通知由 `sendGoalSettlePush` 改订阅 ch02 `omp.goal.updated`(经 ch05 唯一通道;`goal.status ∈ complete|budget-limited|dropped` 映射 goal_complete/goal_budget/goal_blocked 三型文案,runtime.js:52-60;精确映射随 02 §5.6 定稿核对)。**P1 语义 = 设置 + 状态投影 + 显式操作(R12):任务回合自然结束即停,goal 状态保持、不自动续跑;自主续跑待 02 GAP-B14(P2,预算/幂等/abort/重启恢复测试先行)由引擎侧驱动器承担,本域不实现任何续跑逻辑**。
- `.agents/loops` 对账(`runtime.js:389-393`)与 `/` 前缀 prompt 走 command(`:75-83,491-518`)保留;`/command` 转发链已验证(endpoints.js:273-284)。
- OC session-goal runtime(`session-goal/*`)与 `useSessionGoal`/`sessionGoalMetadata` 在 goal 迁移完成后退役(依赖 ch02 落地)。

### 5.4 GAP-08/13:斜杠命令三层管线与碰撞

**目标管线**(客户端唯一解析入口,替换 session-ui-store.ts:152-193 的两查逻辑):

```
用户提交 "/name args"
 ├─ Tier A: omp 内建语义命令(客户端执行,数据/动作走 /api/omp/*)
 │   名单来自 GET /api/omp/commands(omp-host 汇总,含 tier 标记)
 │   例:/model /new /compact /agents /plan /goal /usage /share …(ch02/ch04 分批实现)
 ├─ Tier B: 引擎展开命令 —— 原文经 prompt/command 通道发给引擎
 │   extension 命令、custom TS 命令、文件式 markdown 命令、/skill:name
 │   (SDK agent-session.ts:5318-5342 已实现;发现与元数据走 GET /api/omp/commands,
 │    wire /command 保持空返、不改造成返回真实列表 —— R12 裁决,删除归 ch07 GAP-G12)
 └─ Tier C: OC magic prompts(slashCommands.ts:61-129)+ composer 本地命令
     仅当 name 不在 Tier A/B 名单中才命中
```

- **`GET /api/omp/commands` 形状**:`[{ name, description, tier: 'client-builtin' | 'engine', source: 'builtin' | 'extension' | 'custom' | 'file' | 'skill' }]`。omp-host 侧:Tier B 部分从物化 session 的 `customCommands`/`slashCommands`(agent-session.ts:5062-5069)与全局 skills 汇总;Tier A 名单先硬编码客户端侧、由 ch02/ch04 随实现迁移进 registry。**端点经 capabilities `commands.v1` 门控(R2)**:key 缺失/关闭时客户端回退现状两源解析(skills store + OC commands store);wire `GET /command` 维持空返 `[]`(endpoints.js:393 不动,D1:wire 不扩张)直至 ch07 GAP-G12 删除(R12)。
- **碰撞规则(裁决:TUI 语义优先)**:
  - `/debug`:omp 内建(builtin-lifecycle.ts:282-287)vs OC magic `debug`(slashCommands.ts:108-114)。**OC 侧重命名为 `/troubleshoot`**(语义:"帮我调试这个问题"),`/debug` 让名给 omp 调试工具选择器;`knownSlashNames`(ChatInput.tsx:551-553)同步,旧名输入给出一次性 toast 提示迁移。
  - `/compact`:OC composer 本地处理(ChatInput.tsx:551-555)vs omp 内建带 soft/remote/snapcompact 子命令(builtin-lifecycle.ts:122-129)。**采用 omp 语义**:OC 的 compact 动作改调 ch05 压缩端点并透传子命令参数;无参数时等价现行为。
  - `/review`:在 `knownSlashNames` 中(ChatInput.tsx:552)但非 magic 命令;omp 有 bundled custom command `review`(bundled/review/index.ts:476,属 Tier B)。OC 不再本地占用该名。
  - `init/undo/redo/timeline/handoff-review`:omp 无同名内建(命令名全集核对),保留为 OC Tier C/composer 本地命令。
- **magic prompts 本体保留**:九条命令与 `lib/magicPrompts.ts` 模板体系不动;`plan-feature` 保留(它是提示模板,与 ch02 plan mode 是不同层概念;描述文案中避免使用 "plan mode" 字样以降噪)。GAP-13 的 PlanView/usePlanDetection/plan rail 退役(surfaces/registry.ts:186-187 门控移除),由 ch02 plan mode + review overlay 接管。
- **sendCommand 路径**:Tier B 命令继续走 `POST /session/{id}/command`(endpoints.js:273-284 转发 prompt);Tier A 不走该端点。

### 5.5 GAP-04/05/06/07:WorkStatusPanel 吸收设计

- **subagents 节**(GAP-04):
  - 子列表:`liveSessions.filter(parentID === sessionId)`(WorkStatusSubagentsSection.tsx:30-33)改为消费 ch04 `/api/omp/agent-runs?directory=`(R3 资源名;AgentRunsAggregator 目录级聚合快照,04 §5.5.1),本节过滤 `sessionID === sessionId && agentId !== 'Main'` 的行;行数据 = ch04 `OmpAgentRun {agentId, displayName, status: running|idle|parked|aborted, activity, …}`;parked 行显示"已驻留"态并可触发复活(`POST /api/omp/agent-runs/{sessionID}/{agentId}` `kind:'revive'`,04 §5.5.1)。busy 计数(`:75`)改为 `status==='running'` 计数。更新经 `omp.agents.updated`(经 ch05 唯一通道,05 §5.0.3-D;bootstrap/resync 对账入 05 §5.2.4 agents 段);能力门控按 ch04 agent-runs 端点组的 capabilities key。
  - 阻塞显示:`state.permission`/`state.question` 订阅(`:37-38`)改为 ch03 对话框面:`GET /api/omp/dialogs?directory=` 权威快照 + `omp.dialog.requested/settled` 事件(经 ch05 唯一通道,05 §5.0.3-C),过滤本会话子树的 `kind:'approval'`/`kind:'ask'` 待决;行点击打开 ch03 审批弹窗/ask 对话框并 `POST respond`(替代现在的跳子会话,`:55-69` 的移动端/嵌入降级路径保留)。
  - 空→出现自动展开行为(`:44-53`)保留。
- **goal row**(GAP-05):数据源从 `getSessionGoal(session)`(useSessionGoal.ts:20)改为 ch02 goal 投影:**P1 = 投影事件/快照 + 显式用户操作(02 GAP-B08,R12)**——`omp.goal.updated`(经 ch05 唯一通道,05 §5.0.3-B)+ 冷读 `GET /api/omp/sessions/{id}/goal`(权威快照,02 §5.6;bootstrap/resync 对账入 05 §5.2.4 modes 段);显示 objective 摘要 + 状态图标(active/paused/complete/budget-limited/dropped,对齐 TUI mode 段)。交互入口(pause/resume/drop)改调 ch02 端点(`POST /api/omp/sessions/{id}/goal` op 动词)。**自主续跑 = P2(02 GAP-B14),本域不实现任何续跑驱动**。
- **usage 节**(GAP-06):`UsageLimitRow` 增加 `tier` 维度,窗口行固定枚举 `5h/7d/monthly`(多余窗口折叠进 "其他");`pickUsageHeadline` 最短窗口逻辑不变(usageHeadline.ts:43-69)。数据抓取(OC `/api/quota/*`)不动。
- **MCP 节**(GAP-07):**ch04 §5.7.5 定义可执行 MCP 端点(connect/disconnect/重启,含失败与重连语义)之前一律只读**(R12):连接开关禁用并附说明("MCP server 经引擎配置加载,管理面建设中",07 §5.10 Step 1 同款),不得出现"看似成功"的 no-op 操作;只读态门控经 capabilities feature 状态(R2,服务端裁决)。04 端点落地后,数据源与操作按 capability 切换到 omp-parity MCP 端点(事件/重连语义以 ch04 为准);若 04 裁决不建端点,管理面随 07 GAP-G10 终局删除。
- **tasks 节**:不改;todo 事件语义、todo_auto_clear 的清空规则由 ch05 定义,本节与 `useTodosPersistStore` 按新语义消费。

### 5.6 GAP-09/12:通知触发迁移与 context 单源

**通知触发**(事件源 = ch05 注册表,经 ch05 唯一 `OmpEventBus → /api/omp/events` 通道消费,不自建通道 —— R1;设置层与交付层零改动):

| OC 现触发 | 证据 | 目标触发 | 备注 |
|---|---|---|---|
| `message.updated` finish=stop → ready | runtime.js:333-432 | `agent_end` 终局(05 §5.7 处置)且最后 assistant `stopReason ∉ {aborted,error}` 且 `isTerminal !== false`(`isTerminal:false` → `omp.session.settled`,抑制完成通知) | TUI `sendCompletionNotification` 同构(event-controller.ts:2250-2273);5s 冷却/模板/subtask 模板逻辑保留 |
| `message.updated` finish=error → error | runtime.js:434-502 | `agent_end` `stopReason==='error'`(retry 窗口由 `isTerminal`/ch05 retry 事件表达)—— **唯一错误系统通知权威** | TUI `sendErrorNotification` 同构(event-controller.ts:2206-2248);`omp.notice.raised{level:'error'}` **不触发系统错误通知**,仅应用内 toast(见下权威规则) |
| `question.asked` → 输入提醒 | runtime.js:507-585 | `omp.dialog.requested`(kind:'ask';ch03 桥) | 500ms 防抖保留;`ask.notify`(omp 设置)与 OC `notifyOnQuestion` 并存见 §8 |
| `permission.asked/replied` → 审批提醒 | runtime.js:587-707 | `omp.dialog.requested/settled`(kind:'approval';ch03 桥 = tool_approval 的注册表投影) | requestKey 去重保留;auto-accept 抑制不再需要(对话框事件仅在 `dialogs.v1` 开启且需审批时产生,capability 关闭 = fail-closed 错误路径) |
| goal 结算 push | runtime.js:713-745 | `omp.goal.updated`(ch02,经 ch05 通道;status 映射见 §5.3) | 文案与 APNs 类型不变 |

**通知权威规则(R2-评审 M9,裁决:角色互斥,不按 ID 去重)**:完成/失败的**系统通知**(desktop/OSC、web-push、APNs)**唯一权威 = 终局 `agent_end`**(isTerminal 分支按 stopReason 分派 ready/error,见上表);`omp.notice.raised` 一律仅为**应用内瞬时 toast**(经既有 SSE 广播通道 `openchamber:notification`,不触发任何系统通知、不进通知持久面)。裁决理由:同一失败可同时产生 terminal `agent_end(stopReason:error)` 与 `omp.notice.raised(level:error)`(扩展/引擎错误既结算 turn 又发 notice),若两者均可触发系统通知即双发;现有去重(requestKey)只覆盖对话框事件,无法跨这两类事件去重 —— SDK notice 载荷仅 `{level, message, source?}`,无 sessionID/turnID/errorID(agent-session-events.ts:55),"按 ID 关联去重"的备选在扩展事件载荷前不可实现。故取**角色互斥**:notice 与系统通知的角色在结构上不重叠,双发不可能,且与 TUI 语义一致(TUI 系统错误/完成通知只出自 `agent_end` 的 sendError/sendCompletion,event-controller.ts:2206-2273;notice 仅进程内展示,不受 `error.notify` 门控)。设置门控随之明确:系统错误通知由 `notifyOnError` 门控;toast 为应用内瞬时展示,不经系统通知设置门控。

实现位置:通知触发消费方注册为 omp-host 进程内事件订阅者(与 05 §5.1 处置表同源),只消费 ch05 注册表登记的 SDK 源类型与公开名(`agent_end` isTerminal 分支、`omp.notice.raised`、`omp.dialog.requested/settled`),**不新增事件名、不自建通道**(R1);`maybeSendPushForTrigger` 改接上述源(`omp.notice.raised` 除外 —— 它只进应用内 toast 分支,不进系统通知组装,见上权威规则),payload 组装/模板变量(`buildTemplateVariables`)保持。**顺序约束:必须先于 ch07 删除 permission/question 协议落地,否则审批/问询通知断供。**

**context 单源**(GAP-12):声明 `contextUsage` 选择器(现由消息 token 求和,`work-status/contextUsage.ts`)为唯一来源;ch05 事件化后该选择器内部换数据源(引擎 context 事件),Header(`Header.tsx:2097-2114`)与 WorkStatus session 节(`WorkStatusPrimaryGroup.tsx:192-232`)都只读它;`session.cost` 由 omp-host 投影继续填充(omp-host 投影层职责,D2)。逐turn usage row 归 ch05,不在面板重复。

### 5.7 GAP-10:队列终局设计

现状 delta 清单(OC client queue vs omp 引擎队列):

| 维度 | OC client queue | omp 引擎队列 |
|---|---|---|
| 存放 | 浏览器 persist store(messageQueueStore.ts:139-142) | 引擎进程内(agent-session.ts:5983-6022) |
| 写入时机 | busy 时本地持有,**idle 才发送**(useQueuedMessageAutoSend.ts:257-260) | busy 时即入队 `followUp`(delivery:'queue') |
| 排空 | 每次一条 + 退避重试(`:264,18-19`) | `followUpMode` one-at-a-time 默认,all 可选(settings-schema.ts:1660-1663) |
| 打断后 | 2s abort 保持窗(`:70-77`) | followUp-only 自动恢复抑制(agent-session.ts:6071-6075) |
| 编辑 | 逐 chip edit/拖排序/pop-to-input | 整批取回编辑器(input-controller.ts:1439-1454) |
| 多客户端 | 仅本客户端可见 | 引擎侧,全客户端一致(需事件/快照) |
| 持久 | 刷新存活;服务重启无关 | 服务重启丢失(会话消息持久,队列不持久) |

**SDK 队列面事实核(R2-评审 H6,已源码验证)—— v2 的写入协议在现有 SDK 上不可实现**:

- **入队不收 ID**:SDK 入队路径 `session.prompt(text, options)` 的 `PromptOptions` 无任何客户端 ID 字段(agent-session-types.ts:290-307),requestId 无处递交;
- **快照只有文本**:`getQueuedMessages()` 返回 `{steering: readonly string[], followUp: readonly string[]}` 纯文本数组(agent-session.ts:6425-6429);`clearQueue()` 以用户消息集合整体操作、整批取回(`:6398-6411`;`RestoredQueuedMessage = {text, images?}`,agent-session-types.ts:415)。host 侧"(sessionID, requestId) → 队列条目"关联表因此不可靠:重复文本无法区分、one-at-a-time 消费改变条目集、整批 clear 无法定位单条;且进程重启后 host 去重表消失,已执行未 ack 的 outbox 条目会被再次执行;
- **无版本化队列事件**:引擎不产生队列变更事件;host 自造 version 只能进程内计数、重启归零,version 对账失去意义。

**方案 A(现行,SDK 扩展合入前的唯一活跃路径)**:client queue 原样保留为执行面,**行为零改动** —— queue 行为仍为 busy 时本地持有 + idle 派发(messageQueueStore/useQueuedMessageAutoSend 现状,2.8),引擎 followUp 路径对 UI 保持不可达。允许的仅有展示/文案对齐:chip 标注"排队中(本地)";`followUpBehavior` 设置描述对齐 TUI Enter(steer)/和弦(followUp)语义。

**方案 B(终局,整体 gated 于上游 SDK 扩展)**:queue 模式改为立即 `sendMessage(..., delivery:'queue')`(ChatInput.tsx:1362-1366 分支改派),消息进引擎 followUp 队列。**上游前置(R2-H6 阻断项,三件套缺一即协议不成立)**:

1. **入队带稳定 ID**:SDK 入队路径接受客户端 requestId 并随队列条目保存;
2. **快照携带 ID**:`getQueuedMessages()` 等快照面返回条目级稳定 ID(文本-only 投影不足以支撑 ack 判定);
3. **带 version 的队列变更事件**:入队/排空/清除发出队列变更事件,version 单调且跨重启持久(不归零)。

跨重启幂等另要求 requestId 台账在**确认入队或执行之前**持久化、并可与 transcript 对账 —— 进程内有界 TTL Map 不够(R2-评审同款要求)。

前置合入后,写入协议按 R12 设计实现(**任何部分不得先于前置落地**):

- omp-parity 面:`GET /api/omp/sessions/{id}/queue` → `{ version, steering: [{requestId, text, images?}], followUp: [...] }`(omp-host 从扩展后的 SDK 快照投影);
- 变更事件 = **`omp.queue.changed {version}`**(ch05 注册表 G 行,05 §5.0.3;v1 冒号命名已按 05 更名表废弃),经 ch05 唯一通道,durable、session 作用域、version 单调,仅作 refetch 触发器;chips 改为渲染引擎快照;**SDK 扩展合入前该事件不产生**(host 不以进程内自造 version 冒充,ch05 侧标注见 OQ-1);
- **requestId 幂等 + server ack + outbox**(R12 设计保留,gated):客户端为每条入队消息生成 UUID `requestId` 随 `delivery:'queue'` 提交;入队落定以 `omp.queue.changed {version}` 前进 + 快照中出现该 requestId 条目为准(事件与快照即 ack 载体,不另设 ack 端点);persist store 降级为 outbox —— 条目仅在收到 ack 后出箱;网络失败/超时未 ack 的条目保留 outbox,按 `useQueuedMessageAutoSend` 退避策略重试(requestId 不变,幂等去重);用户手删/pop-to-input 显式出箱;服务重启后客户端按 version 对账(扩展保证 version 不归零)+ requestId 持久台账与 transcript 对账,outbox 重提交零重复执行;
- queue 对账纳入 ch05 §5.2.4 bootstrap/resync 矩阵(agents/jobs/queue 段):version 跳变/断流 → 权威 GET;
- chip 操作映射:pop-to-input/edit = `clearQueue` + 本地回填 + 重新提交(TUI 整批语义,input-controller.ts:1443-1454 的等价物);拖排序在引擎队列模式下退役(或届时扩引擎 per-item API —— 引擎 Agent 队列现无 per-item API,成本另议)。

**快照投影的诚实口径**:SDK 扩展合入前,本章**不注册** `GET /api/omp/sessions/{id}/queue` 端点 —— 今日唯一可投影的形状是 `{steering: [{text, images?}], followUp: [...]}` 文本-only(无条目 ID、无 version;getQueuedMessages/clearQueue 现状,agent-session.ts:6425-6429/6398-6411),支撑不了 chips 渲染引擎快照所需的对账语义;不发半吊子协议,也不以 host 侧文本匹配表伪装 ID 稳定性。

**取舍与切换门槛(R12 + R2-H6,REVISED)**:B 换来多客户端一致与服务端排空正确性(与 TUI 行为逐点对齐),代价是失去逐条编辑/排序(除非扩引擎 API)、以及**新增上游 SDK 扩展依赖**(落地时点不受本项目控制)。**`queue.v1` 在上游三件套 + 持久台账合入前恒为 false**(R2,服务端裁决;三矩阵回滚见 §6);启用需四条件齐备:①上游 SDK 队列扩展三件套合入;②写入协议实现并通过 §7 队列测试组(SDK 扩展后方可执行的那组);③queue 对账在 ch05 §5.2.4 矩阵(已纳入,05 修订版);④capabilities `queue.v1 = true`。此前一律维持方案 A;上游依赖处置与切换时机列入 §8-1(开放问题)。

### 5.8 所有权地图(原创面 vs ch02/04/05 计划面)

| 元素 | OC 原创面 | omp 计划面 | 裁决 |
|---|---|---|---|
| todo 列表 | StatusRow TodoItemRow、WorkStatus tasks 节、todosPersist | ch05 todo 事件 + todo_auto_clear | **数据/语义 = ch05;三处渲染 = OC 保留,单一来源 `state.todo` reducer** |
| usage | WorkStatus usage 节(配额) | ch05 usage row(逐turn);status line usage 段语义 | **逐turn row = ch05;配额面板 = OC;行模型对齐(GAP-06)** |
| context/cost | Header ContextUsageDisplay + WorkStatus session 节 | ch05 context 事件;TUI footer 同位 | **单一 selector,双渲染位保留(GAP-12)** |
| 子 agent 汇总 | WorkStatus subagents 节 | ch04 `/api/omp/agent-runs` roster | **数据端点 = ch04(R3);会话内子集视图 = OC 节消费同一 `OmpAgentRun` 快照,非重复面** |
| goal | WorkStatusGoalRow、scheduled goal、goal push | ch02 goal mode | **状态/端点/事件 = ch02(P1 投影,P2 续跑);呈现与定时集成 = OC** |
| plan | PlanView/plan rail(退役)、magic `/plan-feature`(留) | ch02 plan mode + review overlay | **模式 = ch02;提示模板 = OC Tier C** |
| MCP | WorkStatus MCP 节、McpDropdown | ch04 MCP 可执行端点(§5.7.5,未落地) | **端点定义权 = ch04;落地前 OC 面只读 + 开关禁用(R12);落地后呈现/操作 = OC** |
| 斜杠命令 | magic commands + composer 本地 | Tier A 内建(ch02/04 分批) | **§5.4 三层管线(发现 = `GET /api/omp/commands`,R12)** |
| 队列 | chips/autoSend | ch05 `omp.queue.changed` + §5.2.4 矩阵(如采方案 B) | **§5.7(R12 门槛 + R2-H6 上游 SDK 前置;`queue.v1` 前置合入前恒 false)** |
| 通知 | 设置层 + 交付通道 + tray 壳 | ch05 注册表事件(agent_end isTerminal/notice/dialog) | **触发 = ch05 通道事件(R1);设置/通道 = OC** |
| agents 定义编辑 | agents 设置节(AgentPermissionsEditor 随 ch07 删) | ch02 `/api/omp/agent-definitions` | **定义面 = ch02;OC 编辑器仅保留非权限字段或并入 ch02 面(归 ch02 裁决)** |

---

## 6. 迁移与兼容

阶段对齐 master D4:

- **P0(GAP-01/11)**:`lib/omp-defaults.ts` + 四表单默认值切换;`resolveDefaultAgentModelSelection` 删 build/big-pickle 分支;所有权地图(§5.8)并入各章评审。存量影响:无持久数据;行为差异 = 无 OC/OpenCode 设置默认时表单显示"跟随默认"而非硬选 big-pickle/build。
- **P1(GAP-02/03/04/05 投影/07 只读化/08/09)**:
  - 斜杠管线:先落 Tier C 收紧 + `/debug→/troubleshoot`、`/compact` omp 化(依赖 ch05 压缩端点时先保留 OC 动作、名字让位);发现面 `GET /api/omp/commands` 上线(capabilities `commands.v1`,R2);**wire `GET /command` 保持空返不改造**(R12;删除归 ch07 GAP-G12)。
  - 通知触发切换(§5.6 表,事件源 = ch05 注册表名,经 ch05 唯一通道),**先于 ch07 的 permission/question 删除**;并行期双订阅(wire 事件未删尽前新旧都听,以 requestKey/messageId 去重防双发)。
  - subagents 节数据源切换(ch04 `/api/omp/agent-runs` 可用后);ch03 对话框面可用前保留旧订阅并加 TODO 标注(唯一允许的过渡双轨,删除时点绑定 ch07)。
  - goal row 数据源切 ch02 投影(`omp.goal.updated` + `GET …/goal`);MCP 节只读化 + 开关禁用(随 07 GAP-G10 Step 1,capabilities 门控)。
  - scheduled execution 新字段:`goalEnabled` 读作 `goal.enabled`(读旧写新);**`permissionAutoAccept` 读取即忽略**(R10 fail-closed,UI 输入停止生效);runner 对 `modelRole` 省略 model;编辑器加 `approvalMode ≠ yolo` 警告。
- **P2(GAP-05 续跑/07 数据源/10/12/13)**:OC session-goal runtime 与 `sendGoalSettlePush` 旧路径退役(goal 输入迁移 + goal row 切换完成后);goal 自主续跑消费(待 02 GAP-B14,本域不实现);MCP 数据源切换(随 ch04 可执行端点);队列方案 B **整体后置 —— 上游 SDK 队列扩展三件套合入前不启动任何部分,`queue.v1` 恒 false**(门槛见 §5.7,R12+R2-H6);context selector 换 ch05 源;PlanView/rail/usePlanDetection 删除(flag 与合成文本检测一起,`usePlanDetection.ts:29-52` 的标记串不再产生——ch02 plan mode 不用合成文本协议)。
- **P3(GAP-06)**:usage 行模型对齐。
- **回滚与三矩阵(R2,REVISED)**:每个 GAP 独立提交;本章自有 feature 开关全部经 `GET /api/omp/capabilities` 承载(`queue.v1` 方案 B 写路径/快照 —— SDK 扩展前置合入前恒 false(R2-H6),P1/P2 可预见周期内"回退"即默认态;`commands.v1` 命令发现管线;MCP 只读态/dialogs/modes/agent-runs 等按所属章 key)—— 服务端摘 key 即回退,**不引入本地 feature flag**。三矩阵:新 UI + 旧 engine(key 缺失 → 自动回退 client queue 与现状两源命令解析);旧 UI + 新 engine(旧 UI 不读新端点/新通道,行为不变);relay 旧 bundle(等价旧 UI)。输入迁移类(GAP-01/02)回滚 = 恢复表单默认值读取 legacy 级联(分支保留一版);scheduled task 的 goal 字段读旧写旧可即时回退,`permissionAutoAccept` 旧值回滚**不**恢复自动放行(fail-closed 是 R10 语义,非 feature);通知触发回滚 = 服务端切回旧 wire 触发源(并行期双订阅已以 requestKey/messageId 去重,旧路径保留一个版本窗口后删除)。
- **并发会话**:multirun 批量建 session 不变;队列方案 B 上线时,已在 client queue 的存量消息在首启时一次性 flush(send 而非迁移,每条携带新生成 requestId,引擎侧自然排队;ack 后清空 persist store)。

---

## 7. 验证方案(设计,不执行)

单元/集成(bun:test,omp-host;server JS `node --check`):

- `omp-defaults` 解析器:role 缺失/部分缺失/thinkingLevel 缺失 → undefined 语义;六调用点(multirun/scheduled/github/newWorktree/multirun-agent-selector/configStore)默认值快照测试。
- scheduled runtime:旧字段任务读取兼容(`permissionAutoAccept` 读取即忽略、无审批改写;`goalEnabled` 读作 `goal.enabled`);`modelRole` 任务发出的 `prompt_async` body 无 model 字段;无人值守 fail-closed(`dialogs.v1` on + prompt 档工具 → 工具错误 + 任务诊断可见,全局审批设置未被改写);goal P1 无续跑(回合结束 goal 状态保持);loops 对账回归(既有 `scheduled-tasks/*.test.js` 扩展)。
- 通知触发:给定 `agent_end{stopReason:'error', isTerminal:true}` → error 通道一次;`isTerminal:false`(→ `omp.session.settled`)→ 零;`omp.dialog.requested`(kind=approval)→ 审批通道;新旧双订阅去重(同一 requestKey 只发一次);**双发抑制(§5.6 权威规则)**:`omp.notice.raised{level:'error'}` 单独到达 → 仅应用内 toast、零系统通知;notice 后紧跟 terminal `agent_end{stopReason:'error'}` → 系统错误通知恰一次(toast 与推送可并存)。
- 斜杠管线:名字解析优先级表驱动测试(Tier A > B > C;`/debug` → Tier A 动作;`/troubleshoot` → magic;`/summary` → magic);`GET /api/omp/commands` 形状;`commands.v1` 缺失 → 回退两源解析。
- 队列(现行,方案 A):`queue.v1` 缺失/false → client queue 行为不变(busy 本地入队、从不发 `delivery:'queue'`;useQueuedMessageAutoSend 派发/退避回归);引擎 `delivery:'queue'` → followUp 映射保持(既有 `omp-host.engine.test.js:71-91`)。
- 队列(方案 B 写入协议 —— **SDK 扩展前置合入后才可执行**,§5.7):`delivery:'queue'` 带 requestId 中turn提交 → `GET queue` 快照含该 ID 条目;同 requestId 重发 → 不重复入队(幂等);ack 前条目不出箱、5xx/超时条目保留 outbox 并退避重试;`clearQueue` 后快照空;模拟引擎重启(version 持久不归零 + requestId 台账持久)→ outbox 对账重提交且零重复执行;`followUpMode` 默认 one-at-a-time 排空断言(引擎侧已有 `omp-host.engine.test.js:86-91` 扩展)。
- WorkStatus 数据源:subagents 节对 `/api/omp/agent-runs` 快照(含 parked 行,复活动作走 `POST …/agent-runs/{sessionID}/{agentId}`)渲染;goal row 对 ch02 状态图标映射(P1 无续跑断言);MCP 节只读态断言(开关禁用、无 fake-success 调用路径)。

E2E(dev 栈 5180/3902,浏览器驱动):

- 新建 multirun:默认表单无预选模型,显式加 2 模型 → 2 worktree/2 session/2 条 prompt(网络面板断言 body 无 model 或含显式 pin)。
- 新建 scheduled task:默认"跟随默认 role",保存后重开编辑器回显;到点执行生成会话且模型 = 当前 `modelRoles.default`;遇 prompt 档工具时任务步失败且诊断可见(fail-closed)、全局审批设置未变;goal 任务回合结束即停(状态保持)。
- GitHub issue picker:发起消息的 wire body 不含 providerID/modelID(跟随默认)。
- `/debug` 输入 → omp 调试选择器(或其 OC 等价 Tier A 动作);`/troubleshoot <bug>` → 双提示对消息出现;`/compact` → ch05 压缩动作。
- 通知:后台标签页跑长任务 → 完成/错误/审批三类推送按设置触发;审批在 ch03 弹窗处理 → 推送不再发(requestKey 抑制);错误场景 notice 与 terminal `agent_end` 并发到达 → 系统推送恰一次、应用内 toast 可并存(§5.6 权威规则)。
- 队列(方案 B —— **SDK 扩展前置合入后才可执行**):双客户端(主窗口 + VS Code)同会话,A 排队消息,B 端 chips 同步可见;断流重连后按 05 §5.2.4 矩阵对账 queue(version 跳变 → GET)。

TUI 对照点:

- Enter=steer / 和弦=followUp 的行为对照(input-controller.ts:1367-1391);queue 显示与 `Alt+Up to edit` 提示对照(ui-helpers.ts:892-924)。
- 通知触发语义对照 `sendErrorNotification/sendCompletionNotification`(event-controller.ts:2206-2273):aborted 不发完成、error 不发完成、retry 中间态不发;系统错误/完成通知唯一出自 `agent_end`,notice 仅进程内展示 —— §5.6 权威规则的 TUI 依据。
- usage 段/逐turn row/workStatus 配额三呈现位各司其职(TUI footer vs usage-row vs status line usage)。

---

## 8. 开放问题

1. **队列方案 B 的上游 SDK 依赖与切换时机(§5.7;R2-评审 H6 改判)**:v2 将写入协议(requestId 幂等 + server ack + outbox)视为可实施是**错误判断** —— SDK 现状(入队不收 ID、快照文本-only、`clearQueue` 整批、无版本化队列事件;agent-session.ts:6425-6429/6398-6411、agent-session-types.ts:290-307)支撑不了该协议(§5.7 事实核)。本轮裁决:方案 B 与 `queue.v1` 整体 gated 于**上游 SDK 队列扩展三件套**(入队带稳定 ID、快照携带 ID、带 version 变更事件)+ 跨重启持久 requestId 台账;此前方案 A 长期现行。**需回写他章/上游的两项**:①ch05 注册表 `omp.queue.changed`(05 §5.0.3 G 行)在 SDK 扩展合入前**不产生**(host 不以自造 version 冒充),05 §5.2.4 矩阵 queue 段同随该前置标注;②SDK 扩展属上游依赖、时点不可控 —— 若上游长期不扩,终局即方案 A 长期化(接受多客户端队列不可见)。剩余用户决策 = 是否接受"整批取回→编辑→重排→重发"替代逐 chip 编辑/拖排序(SDK 仅有整批 `clearQueue`,input-controller.ts:1443)。**建议:上游三件套合入 → 写入协议实现并过 §7(SDK 扩展后)队列测试组 → 切 B;若产品认定逐条编辑不可弃或上游不扩,方案 A 长期化。**需用户拍板。
2. **通知设置键归属 vs ch06**:omp 有 `completion.notify/error.notify/ask.notify`(settings-schema.ts:1919-1971),OC 有自己的 `notifyOn*` + 模板 + 模式层(useUIStore.ts:722-731)。**建议:OC 键保留为 OC 层**(治理 OC 独有交付通道 web-push/APNs/desktop,omp 键只管 TUI 进程内 toast,语义不冲突);ch06 若做 omp 设置代理,通知组标注"OC 附加面"不代理。
3. **scheduled task 的 pin 默认值**:新任务默认 `modelRole:'default'` 还是默认显式 pin 当前 role 解析结果?**建议 role 引用**(跟随用户后续调整),编辑器提供"固定"开关;若用户依赖"当时模型"的可复现性反馈强烈再翻转默认。
4. **`/debug` 重命名**:OC 语义(调试魔咒)vs omp 语义(调试工具选择器)名字相同但完全不同。**建议 OC 让名改 `/troubleshoot`**;若认为 discoverability 受损,备选 = OC magic 名单整体加前缀(如 `/oc:summary`)——成本高、不建议。
5. ~~Agent Hub 数据口径(master 开放问题 4)~~ **已裁决(04 D04-3/D04-6,R3)**:subagents 节数据源 = ch04 `/api/omp/agent-runs`(AgentRunsAggregator 聚合,`OmpAgentRun.status ∈ running|idle|parked|aborted` 细分,双段 key `sessionID::agentId`,04 §5.5.1);§5.5 已按该形状消费,无待回填项。
6. **multirun 与 ch04 jobs 的关系**:multirun 是"多 worktree 多 session"产品,jobs 是单会话异步任务,不重叠;但未来"multirun 组"是否用 ch04 会话树/分组端点承载(替代标题字符串约定,useAgentGroupsStore.ts:53-77)**建议长期是**(字符串约定脆弱),列为 ch04 后续。
7. ~~`GET /command` 是否改为返回真实列表~~ **已裁决(R12/master D6 R12;07 GAP-G12)**:新命令语义(omp 内建/引擎展开命令的发现与元数据)唯一落点 = 本章 `GET /api/omp/commands`(§5.4);wire `/command` **保持空返**(endpoints.js:393 不动,D1:wire 不扩张)直至 ch07 GAP-G12 删除。v1 的"改造 wire 端点返回 customCommands/slashCommands 汇总"方案作废。

---

## 9. 依赖

前置(本章消费其契约):

- **ch01**(硬前置,GAP-01):`GET /api/omp/models` 角色快照形状与 thinkingLevel 表达;`routeMessage`/wire prompt body 的 model 参数可选化口径。
- **ch02**(GAP-02/03/05):persona 面 `/api/omp/personas` 与 worker 定义面 `/api/omp/agent-definitions`(R3 分型);goal mode 端点/事件/状态枚举(P1 投影,P2 续跑)。
- **ch03**(GAP-04/09):对话框桥(`omp.dialog.requested/settled` + `GET /api/omp/dialogs`;无人值守 fail-closed 语义与 `dialogs.v1` capability,R10)。
- **ch04**(GAP-04/07):`/api/omp/agent-runs` 聚合快照(含 parked,R3);MCP 可执行端点定义权(04 §5.7.5;落地前 UI 只读,R12)。
- **ch05**(GAP-09/12、§5.4 Tier B、§5.7 方案 B):唯一事件通道与注册表(05 §5.0.3:`agent_end` isTerminal 分支/`omp.notice.raised`/`omp.dialog.*`/`omp.queue.changed` —— 后者 SDK 队列扩展前置合入前不产生,R2-H6/OQ-1);bootstrap/resync 矩阵(05 §5.2.4,含 queue,同随该前置);压缩端点(`/compact` omp 化);capabilities 端点(R2,承载 `queue.v1`/`commands.v1`)。
- **上游 SDK(§5.7 方案 B 硬前置,R2-H6)**:AgentSession 队列面扩展 —— 入队接受稳定 requestId、快照携带条目 ID、带 version 的队列变更事件(version 跨重启持久);跨重启幂等需 requestId 持久台账并可与 transcript 对账。合入前 `queue.v1` 恒 false、方案 B 不启动(OQ-1)。
- **ch06**(软依赖):设置代理面确定 OC 通知键/`followUpBehavior` 的存放与展示层。

后置(消费本章产出):

- **ch07**:permission/question 删除的前置条件之一 = 本章 GAP-09 通知切换与 GAP-04 subagents 节切换完成(§6 顺序约束);wire `GET /command` 保持空返直至 ch07 GAP-G12 删除(R12 已裁决,§5.4/§8-7);MCP 只读化与 07 GAP-G10 Step 1 同列车。
