# 02 · 域 B:Agent 与模式

状态:设计稿修订轮 2(R2 REVISED 2026-08-20,落地 R2 评审 M3/M4;落 D6 冻结契约 R1-R12;遵循 `00-MASTER.md` 总纲;裁决原则:D1 双轨契约 / D2 投影权威 / D3 概念映射)
日期基线:2026-08-19(SDK `@oh-my-pi/pi-coding-agent@17.3.7`,TUI 源 = `oh-my-pi/packages/coding-agent/src`,两者同源)
证据缩写:`<s>` = SDK 安装副本 `C:/Users/reamd/.bun/install/global/node_modules/@oh-my-pi/pi-coding-agent/src`;`<tui>` = `C:/Users/reamd/Documents/experiment_area/oh-my-pi/packages/coding-agent/src`(与 `<s>` 同构,行号通用);`ui/` = `packages/ui/src`;`host/` = `packages/web/server/lib/omp-host`。
> **R2 修订摘要**(本轮落地项,评审源 `.jspace/review-output-r2.md`):**M3** persona 生命周期收敛——P0 仅**会话级显式切换**(变更即重建,与今日 agent-switch 同路径),`@persona` 单条消息路由承诺**删除**(per-turn 化身需 SDK 原生 per-turn system/tool overlay → 开放问题 7,本章不设计)(§5.1);**M4** agent `task.*` 覆盖作用域分层——项目定义的 overrides 写**项目层**(前提 = 01/06 章 R2 落地的 per-directory Settings 注入;注入落地前该 capability 关闭),全局覆盖为独立显式动作,上轮"一律写全局实例"表述作废(§5.2,开放问题 6)。

---

## 1. 域概述与边界

本域管:

1. **custom agents 体系**:agent 定义的发现链(project/user/bundled)、frontmatter 契约、per-agent 覆盖(model patterns / prewalk / advisor / disabled)、**定义 CRUD 端点与 UI**(`/api/omp/agent-definitions`,TUI `/agents` hub 的数据面)。live 运行实例面(revive/kill/chat/transcript)归 04 章 `/api/omp/agent-runs`(R3 资源拆分,见 5.2)。
2. **build/plan 二分的清除**:manufactured `/agent` 面、engine 的 planYolo 映射、UI 的 `build` 硬回退与 primary/subagent 过滤、存量 `meta.agent` 会话的迁移;顶层输入由 **persona**(OpenChamber 原创可选层,独立资源,默认无)承接(D-B2/5.2a)。
3. **会话模式(session modes)产品化**:plan / goal / vibe / loop 四个模式的生命周期状态机、模式进入/退出的 RuntimeAPIs、模式状态投影(事件 + 冷启动恢复)、plan review 流(与 OpenChamber 实验 PlanView 的合并裁决)。
4. **周边模式的裁决**:prewalk / advisor(会话级 + per-agent chips)、btw / tan(表面草图 + 优先级裁决)。

本域不管:

- model roles 的解析链(`modelRoles.default`、`cycleOrder`、priority 链)→ **01 章**;本章只消费"plan role / advisor role / smol role"的解析结果。
- 审批/ask 对话框桥、`xd://propose` 工具的审批 tier → **03 章**(plan review 不是审批:它不走 approvalMode,是独立的 overlay 流程)。
- `local://` 协议解析、Agent Hub live roster/parked agents(`/api/omp/agent-runs`)、async jobs(tan 的底层)、`agent://` artifact → **04 章**;本章只定义 plan 文件路径(`local://PLAN.md`)与 tan/btw 对 04 章实体的依赖关系。
- 事件通道与事件注册表 → **05 章**(R1 单通道:唯一 `OmpEventBus → /api/omp/events`);本章只给出 `omp.mode.changed` / `omp.goal.updated` / `omp.plan.*` 的**名称与 payload 草案**及 engine 侧 producer 职责;envelope/事件 ID/durable/重放/reducer 汇总一律引用 05 章注册表,本章不自建通道。
- `plan.enabled` / `goal.enabled` / `task.disabledAgents` 等设置键的读写通道 → **06 章**;本章定义这些键的产品语义与 UI 入口。
- OpenCode 残留(permission 字段、`/agent` 旧端点)的删除顺序与回滚 → **07 章**。
- multirun / AgentManager / scheduled tasks 等原创面的 agent 输入迁移 → **08 章**。

---

## 2. 现状分析(OpenChamber 侧)

### 2.1 build/plan 二分的制造链

- `host/endpoints.js:361-376`:`GET /agent` 返回**硬编码**的 `build`(primary)+ `plan`(primary)两个 builtin,再把 `engine.customAgents` 全部强制 `mode:'subagent'` 拼在后面。omp 中不存在任何 primary agent 概念(见 3.1)。
- `host/engine.js:464`:`meta.agent === 'plan'` 时向 `createAgentSession` 传 `planYolo: { autoApproveOnResolve: true }`。
- **该 planYolo 形状与 SDK 契约不符**。SDK `PlanYolo` 定义为 `{ target: Model; thinkingLevel?: ConfiguredThinkingLevel }`(`<s>/session/agent-session-types.ts:89-92`),全 SDK 无 `autoApproveOnResolve` 字段。后果链:
  1. `{ autoApproveOnResolve: true }` 是 truthy → `armPlanYoloIfNeeded()` 在首个 prompt 构建时照常武装(`<s>/session/agent-session.ts:5567` → `<s>/session/prewalk.ts:247-259`):激活 read-only plan mode 状态(`setPlanModeState({enabled:true, planFilePath:'local://PLAN.md', workflow:'parallel'})`)并挂 proposal handler,**UI 全程无感知**(omp-host 不投影 plan mode 状态)。
  2. 模型调用 `xd://propose` 提交计划时,`#finalizePlanYoloProposal` 解引用 `planYolo.target.provider` / `setModelTemporary(planYolo.target, …)`(`<s>/session/prewalk.ts:300`),`target` 为 `undefined` → 工具结果抛 TypeError。
  3. 即:**选择 "plan" agent 的会话今天处于"隐形只读模式 + 审批即崩溃"状态**。这是 P0 级缺陷证据,不是可保留的兼容行为。
- `host/engine.js:639`:`agentName = agent ?? meta?.agent ?? 'build'`——'build' 是 prompt 分发的硬默认;`host/engine.js:483` `currentAgent: meta?.agent ?? 'build'`。
- `host/engine.js:451-453`:`meta.agent` 为 'build'/'plan' 时 `customAgent = null`(不解析定义),其余名字查 `this.customAgents`(sidecar,见 2.2)。

### 2.2 双轨 agent 存储(split-brain)

OpenChamber 当前有**两套互不相通的 agent 存储**:

1. **omp-host sidecar**:`host/engine.js:89-120`——custom agents 存 `<registryRoot>/openchamber-agents.json`(字段 name/description/prompt/tools);唯一写入口是 `PATCH /config`(`host/endpoints.js:348-356`)。UI 侧无调用方(`ui/src/lib/opencode/client.ts:1454-1478` 的 `updateConfig` 存在于 SDK client,但 agent CRUD 走的是下面第 2 套)。
2. **OpenChamber host 的 OpenCode 配置实体路由**:`packages/web/server/lib/opencode/config-entity-routes.js:80-147` + `lib/opencode/agents.js:321-361`——`POST/PATCH/DELETE /api/config/agents/:name` 写 **OpenCode 格式**的 `.md` 文件(用户级 `~/.config/opencode/agent/`、项目级 `<dir>/.opencode/agent/`,agents.js:325-352),frontmatter 为 OpenCode 契约(`mode`/`model`/`variant`/`temperature`/`top_p`/`permission`,ui/src/stores/useAgentsStore.ts:402-414)。

omp-host **从不读取**第 2 套文件;`GET /agent`(UI `listAgents` 的数据源,ui/src/stores/useAgentsStore.ts:327)只合并制造面 + sidecar。因此 UI 上"创建 agent"后经 `refreshAfterOpenCodeRestart` 重新拉取,**新建 agent 永远不会出现**——除非名字恰与 sidecar 重复。agent CRUD 在 omp 引擎下已是死功能。

### 2.3 engine 的 agent-switch 重建路径

`host/engine.js:640-649`:`prompt()` 时 `agentName !== hostSession.currentAgent` → `#disposeSession`(拆掉 AgentSession 的事件订阅与 projector)→ `registry.update({agent})` → `#materialize` 重建 AgentSession(同 transcript 文件)→ 重入 `prompt()`。理由(注释自述):agent 定义在**构造时**决定 system prompt 与 toolset(`engine.js:465-472`:`systemPrompt: customAgent.prompt`、`toolNames: customAgent.tools`),所以切 agent 必须重建。

该路径与 omp 语义的冲突:omp 的 AgentSession 暴露**运行时**切换面——`setActiveToolsByName()`(`<s>/session/agent-session.ts:4586-4588`)、`setModelTemporary()`、plan mode 的进入/退出本身就是"改 toolset + 改 model + 注入 context"(3.4)。重建路径是为 OpenCode agent 模型设计的;本章 5.4 将其收缩为"化身定义变更"专用。

### 2.4 UI 侧 agent 选择与默认级联

- Composer 底部 agent 下拉:`ui/src/components/chat/ModelControls.tsx:2655-2860`(`renderAgentSelector` 紧邻 `renderModelSelector`);列表 = `agents.filter(isPrimaryMode)`(ModelControls.tsx:503-505)——由于 omp-host 只制造两个 primary,用户实际只能在 build/plan 间选择,sidecar custom agents(全部 subagent)永不可选。
- 'build' 硬回退三处:默认 agent 名(ModelControls.tsx:518-526)、`fallbackAgent`(971)、label 回退 'Build'(1291-1295);另有 mobileControlsUtils.ts:34-37、GitHubIssuePickerDialog.tsx:249、NewWorktreeDialog.tsx:439、agentColors.ts:20。
- 默认级联:`settings.defaultAgent`(OC host)→ OpenCode `default_agent` → build → first primary(`ui/src/stores/useConfigStore.ts:282-293` 注释 + 实现);发送时 `agent` 参数随每条消息(`ui/src/sync/session-ui-store.ts:214`,`sessionAgentSelection`/`configAgentName` 级联 session-ui-store.ts:1437-1441)。
- 每 agent 的 permission 摘要图标(tooltip,ModelControls.tsx:1438-1447)与 `AgentPermissionsEditor` 挂在 OpenCode permission 协议上(03/07 章删除)。
- agent CRUD UI 写 OpenCode schema:temperature/top_p/permission 字段(useAgentsStore.ts:409-412)——omp 完全忽略(无对应 frontmatter 字段,3.2)。

### 2.5 实验性 PlanView(合成文本协议)

- 旗标:`planModeExperimentalEnabled` 由 UI 启动时 `GET /health` 读取并写入 `useFeatureFlagsStore.planModeEnabled`(`ui/src/App.tsx:404-424`;store:ui/src/stores/useFeatureFlagsStore.ts:8-10,默认 false)。
- 检测:`usePlanDetection` 扫描 assistant 文本 part 中的合成标记 `'The plan at '` / `'User has requested to enter plan mode'`(ui/src/hooks/usePlanDetection.ts:42-44),命中则 `markSessionPlanAvailable`,Header 出现 Plan tab。
- 表面:plan rail surface 被 `planModeEnabled` 旗标门控(`ui/src/lib/surfaces/registry.ts:203-205`);Header 的 plan tab 门控在 ui/src/components/layout/Header.tsx:1338-1355 / 1700-1702。
- 该协议是**纯 UI 启发式**:无引擎侧 plan 状态、无 review/approve 流、无 plan 文件生命周期;omcode 时代遗物(WireOpenCodeResidue 报告 A11 项:"OpenChamber PlanView ≠ omp plan mode")。

### 2.6 缺失面清点

OpenChamber 完全没有:goal / vibe / loop / prewalk / advisor / btw / tan 的任何 UI、端点与事件;`goal_updated` 事件在 engine drop-switch 中被丢弃(`host/engine.js:611-613` default 分支);mode_change session entry 无投影;status-line mode segment 无对应物。

---

## 3. 目标语义(omp / TUI 侧)

### 3.1 agent = worker 分类学(无 primary)

omp 中 **agent 一律是 subagent(worker)**:由 `task` 工具派发、运行在子会话、`yield` 提交结果。顶层会话没有 agent 概念——顶层差异只有 model roles(`<s>/config/model-roles.ts:31-51`,10 个 role)与 thinking level。没有 build/plan agent 对(OpenCode 残留报告交叉确认:OmpSettings 报告种子核对 2)。bundled 分类学(`<s>/task/agents.ts:45-76`):

| agent | 定位 | 绑定 |
|---|---|---|
| `task` | 通用多步 worker | `model: ["@task"]`,`thinkingLevel: AUTO`,`spawns: "*"`(prewalk 由 `task.prewalk` 设置武装) |
| `sonic` | 低推理机械更新/数据收集 | `model: ["@smol"]`,`thinkingLevel: Medium` |
| `scout` | 只读侦察 | — |
| `reviewer` | 代码评审 | — |
| `security-reviewer` | 安全评审 | — |
| `librarian` | 库源码考证 | — |
| `designer` | 设计实现 | — |

("plan" 在 omp 中出现两次、都不是 agent:① model role `plan`(tag PLAN,名 Architect,model-roles.ts:42);② plan **模式**,见 3.4。)

### 3.2 定义发现链与 frontmatter

发现(`<s>/task/discovery.ts:70-138`):优先级 **project `<cwd>/.omp/agents` > user `~/.omp/agents` > OMP 扩展包 agents > Claude 插件 agents(project 先)> bundled**,按 name 去重(先见者胜)。`AgentSource = "bundled" | "user" | "project"`(`<s>/task/types.ts:9`)。

`AgentDefinition` 契约(`<s>/task/types.ts:359-378`,frontmatter 字段在 task/agents.ts:21-31):

```ts
{
  name: string;
  description: string;
  systemPrompt: string;          // md body
  tools?: string[];              // 工具集(缺省=全量)
  spawns?: string[] | "*";       // 可派发的下级 agent
  model?: string[];              // 模型 pattern(如 ["@smol"]、"provider/*")
  thinkingLevel?: string;
  output?: unknown;              // 结构化输出 schema
  blocking?: boolean;
  autoloadSkills?: string[];
  readSummarize?: boolean;
  prewalk?: boolean | string;    // true=默认 prewalk 目标;string=自定义目标 pattern
  advisor?: boolean | string;    // true=advisor role;string=模型 pattern(可选 :level 后缀)
  source: "project" | "user" | "bundled";
  filePath?: string;
}
```

OpenCode 的 `permission` / `temperature` / `top_p` / `mode` / `variant` 字段**无对应物**——omp 审批走 tier 体系(03 章),采样参数走全局设置(06 章)。

### 3.3 per-agent 覆盖(agents hub chips)

`/agents` hub(`<s>/modes/components/agents-hub.ts:287-313`)对每个 agent 叠加四类**设置级**覆盖(不改定义文件):

- `task.disabledAgents: []` — 停用(`<s>/config/settings-schema.ts:4723-4726`)
- `task.agentModelOverrides: {}` — 模型 pattern 覆盖(schema:4728-4731)
- `task.agentPrewalk: {}` — prewalk 覆盖(schema:4732-4735)
- `task.agentAdvisor: {}` — advisor 覆盖(schema:4736-4739)

hub 还提供 "+ New agent"(LLM 生成 spec)与 per-agent ModelBrowser(TUI 侧交互;数据面 = 上述设置键 + 定义文件)。

### 3.4 plan mode(会话模式)

TUI 侧全流程(规格说明):

- **进入** `/plan [prompt]`(builtin-modes.ts:198-202)或 `alt+shift+p`(`<s>/config/keybindings.ts:222-224`);`#enterPlanMode`(`<s>/modes/interactive-mode.ts:2688-2753`):
  1. 与 goal/vibe 互斥(2696-2703);
  2. 捕获当前 toolset,补 `write`(plan 文件草拟必需,2719-2723);
  3. `session.setPlanModeState({enabled:true, planFilePath, workflow:'parallel', reentry})`(PlanModeState:`<s>/plan-mode/state.ts:1-6`);
  4. `session.setPlanProposalHandler(title => session.preparePlanForReview(title))`(2739;AgentSession API:`<s>/session/agent-session.ts:933-938, 1733-1735`);
  5. 流式中则 `session.sendPlanModeContext({deliverAs:'steer'})`(2740-2742,agent-session.ts:4992-4996);
  6. 切到 **plan role** 模型(`#applyPlanModeModel`,2461-2467 捕获 pre-plan 模型);
  7. `sessionManager.appendModeChange("plan", {planFilePath})`(2751)——持久化 `mode_change` entry(`<s>/session/session-entries.ts:234-240`,mode + payload);
  8. status:"Plan mode enabled. Plan file: `<path>`"。
- **plan 文件**:默认 `local://PLAN.md`(`#getPlanFilePath`,interactive-mode.ts:2307-2309)。
- **三态切换**:`/plan` 无参 toggle → 激活→paused(`#exitPlanMode({paused:true})`,有草稿先确认,3386-3396)→ paused→off(第三次 toggle,3398-3409);`plan.enabled=false` 时拒绝(3411-3413)。
- **review overlay**:模型 resolve(`xd://propose`)→ `handlePlanApproval`(3909-4088)全屏 overlay(`<s>/modes/components/plan-review-overlay.ts`):分节 markdown + Contents 侧栏、节删除+undo、行/节 annotation → refine 回路、model-tier slider(cycleOrder 角色环)、外部编辑器;**选项固定四个**(3979-3982):`Approve and execute` / `Approve and compact context` / `Approve and keep context (~X / Y)` / `Refine plan`。
- **approve**(`#approvePlan`,3205-3364):exit(silent)→
  - execute:清空 transcript(`handleClearCommand`)+ 迁移 `local://` 工件到新 root + 把 plan 写入新 root(3234-3244)——**同一 session 的 fresh start**,不是新 session;
  - compact:先压缩(plan 蒸馏 prompt 走 internalGuidance,3245-3273);
  - 恢复执行 toolset(强制含 `read`,3283-3286)、`setPlanReferencePath`(3287)、应用 slider 选择的角色模型(3297-3301);
  - 从 plan 标题播种会话名(3316-3324);
  - 派发**合成** plan-approved prompt(`session.prompt(planModePrompt, {synthetic:true})`,流式中改 followUp,3353-3361)。
- **headless 变体 planYolo**:CLI `--plan-yolo [--plan-yolo-into @smol]`(`<s>/main.ts:1077-1092`);`PrewalkCoordinator.armPlanYoloIfNeeded`(prewalk.ts:247-318)武装 plan 阶段,resolve 时自动批准 + `setModelTemporary(planYolo.target)` + steer 合成 handoff(customType `plan-yolo-handoff`)。
- **会话恢复**:`#reconcileModeFromSession`(interactive-mode.ts:2626-2686)按 mode_change entries 恢复 plan/plan_paused/goal 状态。

### 3.5 goal mode(会话模式)

- 状态机(`Goal` / `GoalModeState`,`<s>/goals/state.ts:3-21`):`goal.status ∈ active | paused | budget-limited | complete | dropped`;`state = {enabled, mode: active|exiting, reason?, goal}`,含 `tokenBudget?` / `tokensUsed` / `timeUsedSeconds`。
- 运行时 `<s>/goals/runtime.ts:117-496`:`createGoal`(objective+tokenBudget)、replaceGoal、`resumeGoal`、`pauseGoal`、`dropGoal`、`completeGoalFromTool`(goal 工具提交完成)、budget 逼近 steer(`#sendBudgetLimitSteer`)、用量记账(`onTurnStart` / `onAgentEnd`,agent-session.ts:2611, 2845)。
- 事件:`goal_updated {goal, state?}` 从 GoalRuntime → AgentSession session event(`<s>/session/agent-session-events.ts:64`)→ TUI `#handleGoalSessionEvent`(interactive-mode.ts:2427-2437 维护 enabled/paused 旗标)。
- 命令 `/goal set|drop|pause|resume|budget|status` + `/guided-goal` 面试(builtin-modes.ts:246-280);目标也可由模型经 `goal` 工具创建/完成。
- **自主续跑**:TUI 输入循环驱动——空闲时(`getUserInput`)`#scheduleGoalContinuation`(interactive-mode.ts:1466-1507):`goal.continuationModes` 含 `"interactive"`(schema:4429-4432,默认 `["interactive"]`)且 goal active、编辑器空、loop mode 关 → 800ms 后以**隐藏合成消息**重提交续跑 prompt(`customType:"goal-continuation", display:false`,1498-1505)。budget 触顶 → `goal-budget-limit` 隐藏消息 + steer。
- mode_change:进入 `appendModeChange("goal"...)`,退出 `"none"`;完成时另有 `goal-completed` custom entry(2905-2909)。

### 3.6 vibe / loop / prewalk / advisor / btw / tan

- **vibe**:`/vibe`(builtin-modes.ts:228-231)→ `#enterVibeMode`:登记临时 vibe 工具、toolset 收缩为 `read`+可选父 `todo`+vibe 工具、注入 director context;`appendModeChange("vibe", {previousTools})`(interactive-mode.ts:3524);退出杀掉全部 worker 会话(3441-3445 docstring)。无设置键(VibeSessionRegistry 内存态)。
- **loop**:`/loop [count|duration] [prompt]`(builtin-modes.ts:282-285);`loopModeEnabled`/`loopPrompt`;空闲后 800ms 自动重提交(`#scheduleLoopAutoSubmit`/`#deferLoopAutoSubmit`,interactive-mode.ts:1440-1457),Esc 取消当次迭代;状态段 `Loop <state> [remaining]`(segments.ts:258-266)。设置键 `loop.mode`(prompt)。
- **prewalk**:`prewalk.enabled` 默认 false(schema:461-464);`/prewalk` 命令随时武装(builtin-modes.ts:521-525);机制 = 起步强模型,todo 出现且首次 edit/write 后切到 smol 类目标(`<s>/session/prewalk.ts:207-243`)。
- **advisor**:`advisor.enabled` 默认 false(schema:450-453);`/advisor on|off|status|dump|configure`(builtin-collaboration.ts:57-61);第二个模型(advisor role,tier.advisor none)每回合旁评,卡片渲染(`advisor` customType);model segment 加 `++` 徽章。
- **btw**:`/btw <question>`(builtin-lifecycle.ts:237-243)——用当前会话上下文的临时旁问,bordered 面板流式渲染,可 branch 成正式聊天。
- **tan**:`/tan <work>`(builtin-lifecycle.ts:248-252)——全后台 agent(async job),转录 pill `◌ Tangent dispatched [task] <jobId>` + `✓ Background job completed` 行;`/jobs` 查看。

### 3.7 状态投影面(TUI 如何呈现模式)

- status-line mode segment(`segments.ts:228-270`):优先级 plan(paused 变体)> prewalk > goal(status 图标 active/paused/complete/budget-limited/dropped + used/budget)> vibe > loop(state + remaining);`goal.statusInFooter` 设置(schema)。
- `mode_change` session entry(`session-entries.ts:234-240`)持久化进会话日志,树选择器显示 `[mode: plan]`。
- `model_change` entry 携带 role 字段(01 章);`thinking_level_changed`、`ttsr_injection` 等见 05 章。

**但自主续跑驱动器(goal/loop)在 TUI 输入循环里**——嵌入式宿主必须自己实现等价驱动(5.6;R12 裁决降 P2,预算/幂等/abort/重启恢复测试先行)。

---

## 4. 差距清单

| # | 差距 | 分类 | 优先级 | 风险 | 摘要 |
|---|---|---|---|---|---|
| GAP-B01 | `/agent` 制造面 + engine planYolo 映射(shape 无效,隐形 plan mode + 审批崩溃) | 删 | P0 | 高 | endpoints.js:361-376、engine.js:451-464/639;存量会话迁移(6.1) |
| GAP-B02 | UI build 硬回退、primary 过滤、每消息 agent 默认链 | 删+改 | P0 | 中 | ModelControls.tsx:503-526/971/1291-1295 等 7 处 + useConfigStore 级联 |
| GAP-B03 | 无真实 agent 定义数据面(discoverAgents 未接) | 建 | P0 | 高 | `/api/omp/agent-definitions` 组(5.2;R3:live 实例面归 04 章 `/api/omp/agent-runs`) |
| GAP-B04 | agent CRUD split-brain(OpenCode .md vs sidecar JSON,互不相通,CRUD 已死) | 建+删 | P0 | 高 | 统一写 `.omp/agents/*.md`;sidecar/OpenCode 存量迁移(6.2/6.3) |
| GAP-B05 | agent 定义字段错位(permission/temperature/top_p/mode/variant 无 omp 对应;缺 model patterns/thinkingLevel/prewalk/advisor/spawns) | 改 | P0 | 中 | 契约替换(5.2/5.3);per-agent chips → task.* 设置键,**覆盖作用域分层 + `settings.projectScopes.v1` 门控(R2-M4,5.2)** |
| GAP-B06 | 无会话模式状态机与 mode 端点 | 建 | P1 | 高 | `/api/omp/sessions/:id/mode` + 状态机(5.4);capabilities `modes.v1` 门控(R2) |
| GAP-B07 | plan review 流缺失;实验 PlanView 合成文本协议需合并 | 建+改 | P1 | 高 | **替换**合成协议,复用 PlanView UI 资产(5.5);合成协议停产 = 模式端点上线即执行,flag/i18n 清扫 P3(6.4,R12) |
| GAP-B08 | goal 状态投影 + 显式用户操作面 + `goal_updated` 桥(REVISED:自旧 B08 拆出续跑项) | 建 | P1 | 高 | `/api/omp/sessions/:id/goal`(5.6;master D4"仅状态投影 + 显式用户操作") |
| GAP-B14 | goal 自主续跑驱动器 host 侧 auto-loop(REVISED:自 B08 拆出,R12 降级;loop 同骨架) | 建 | P2 | 高 | 预算/幂等/abort/重启恢复测试**先行**(5.6,D-B5) |
| GAP-B09 | vibe mode 无对应物 | 裁决 | P2 | 中 | 依赖 04 章 worker 会话/parked;建议 defer(5.7) |
| GAP-B10 | loop mode 无对应物 | 裁决 | P2 | 低 | 与 B14 同一驱动器机制(D-B5);P2 实现(5.7) |
| GAP-B11 | prewalk / advisor 会话级开关与 per-agent chips 无对应物 | 建 | P2 | 中 | chips 随 B03/B05 落;会话级开关走设置代理(5.7) |
| GAP-B12 | btw / tan 无对应物 | 裁决 | P2(defer) | 中 | 依赖 04 章 async jobs / agent://;表面草图(5.7) |
| GAP-B13 | engine drop-switch 丢弃 `goal_updated`(D2 违例,05 章总表) | 建 | P1 | 低 | engine.js:611-613 增 case → `omp.goal.updated`(05 章注册表登记;5.8) |

优先级对照总纲 D4(REVISED):P0 = 概念迁移(roles 取代 build/plan);P1 = 可见性桥(事件/对话框/模式生命周期;goal 仅状态投影 + 显式用户操作);P2 = 实体面(goal 自主续跑/vibe/Agent Hub live 面);P3 = 大扫除(合成 plan 协议遗留清扫随 07 章)。

---

## 5. 设计方案

### 5.1 替换数据模型:`/agent` 面的终结

**决策 D-B1:删除 manufactured `/agent`;OpenCode wire 的 Agent 列表端点保留但返回空壳。**

- wire `GET /agent`(vendored 契约)在过渡期返回 `[]`(07 章最终删除);agent 定义数据一律走 **RuntimeAPIs** `/api/omp/agent-definitions`(D1:omp 原生概念走自有面;R3 路径裁决)。
- 备选:让 `GET /agent` 直接返回 omp agents 投影。**不取**——wire Agent 型含 `permission: PermissionRuleset`、`topP`、`temperature` 等字段(types.gen.d.ts:1939-1959),塞 omp 语义要么撒谎(permission 全空)要么手改 vendored gen(违反 D1)。UI 的 `useAgentsStore.loadAgents` 从 `opencodeClient.listAgents` 切到 `runtimeFetch('/api/omp/agent-definitions')`。

**决策 D-B2:顶层"化身(persona)"保留为 OpenChamber 原创可选层——独立类型与资源,默认无,不复用 worker agent id(REVISED,对齐 master D3 行 / D6 R12)。**

omp 顶层无 agent;但"顶层会话换 system prompt/toolset"是 OpenChamber 现有用户可见能力(master §2.3 原创面保留)。**persona ≠ worker agent**:worker(custom agents)由 `task` 工具派发、运行在子会话、`yield` 提交;persona 是顶层会话的 host 级 systemPrompt/toolset 覆盖。两者生命周期与权限完全不同,同名双入口即概念混用(评审 M-14),故 persona 用**独立类型 `OmpPersona` 与独立资源 `/api/omp/personas`**(5.2a)承载:

- `registry` 的 session meta 增 `persona?: string`(替代 `agent`);**缺省 undefined = 标准会话**(无 systemPrompt 覆盖、全量工具)。'build'/'plan' 语义上等于 undefined。
- persona 解析:`#materialize` 时 `meta.persona` → personas 存储(5.2a)按名查找;找不到 → 降级为标准会话 + `notice` 警告(原 sidecar 行为是静默 null,engine.js:451-453)。**不再查 `discoverAgents`/worker 定义**。
- Composer agent 选择器改为 **persona 选择器**:默认"标准",可选项 = `/api/omp/personas`(worker 定义不进 composer——worker 经 task 工具/04 章 Hub 派发)。**persona 是会话级状态,只有"显式切换"一种生命周期(R2-M3)**:选择器变更 = 对当前会话 persona 的显式切换(经 prompt 携带 persona 参数触发,持久化进 registry meta,对后续所有 turn 生效,直到再次显式变更——会话级语义与今日 agent 参数一致,engine.js:639-649);生命周期细则(时序/回滚/并发)见 D-B3。**`@persona` 单条消息路由不提供(R2-M3 revoked)**:"本条消息用 persona X、下条恢复"需要 per-turn system/tool overlay,而 SDK 仅支持构造时注入 `systemPrompt`/`toolNames`(engine.js:465-472 的 host 用法),运行时最近的 `setActiveToolsByName()`(agent-session.ts:4586-4588)只改 toolset、无 system prompt 替换、无 per-turn 作用域 → 挂开放问题 7(上游),本章不设计、不预留语法/端点。
- 备选 A:完全删除顶层 persona 选择。**不取**:删除用户可见能力且 multirun(08 章)依赖多化身输入。
- 备选 B(修订前旧案):persona 复用 worker 定义面(`discoverAgents` 按名查找、选择器列定义全量)。**不取**:违反 D3"不复用 worker agent id"——定义面与顶层化身同名同源后,改 worker 定义会隐式改顶层会话行为。

**决策 D-B3:agent-switch 重建路径收缩,不删除。**

plan/goal 等 mode 切换走 AgentSession 运行时 API(不重建);但 persona 定义(system prompt + toolNames)仍构造时注入(engine.js:465-472),所以 persona **变更**仍需重建。改造:

- `prompt()` 中重建条件从 `agentName !== currentAgent` 改为 `personaKey(meta.persona) !== personaKey(hostSession.currentPersona)`,其中 `personaKey(undefined) = 'standard'`——即 build→standard、plan→standard 的迁移值**不再触发重建**(6.1)。
- 重建前若 AgentSession 处于非 none 模式,先走模式退出(5.4),避免把 plan mode toolset 快照带进新化身。

persona 切换生命周期(R2-M3 显式定义,关闭评审"无生命周期"项;唯一入口 = prompt 携带 persona 参数的显式切换,D-B2):

- **持久性**:persona 是会话级持久状态(registry meta);变更后对后续所有 turn 生效,**无单 turn 自动恢复**。唯一改写方式 = 再次显式切换或切回"标准"——不存在"@mention 一条、下条还原"的路径(该承诺已删除)。
- **busy 时序**:切换判定位于 `prompt()` 分发之前(与今日 agent-switch 同位置,engine.js:640-649);流式中到达的切换同样先 dispose→重建,触发消息以 steer 进入重建后的会话(继承现状语义,`delivery` 语义不变,engine.js:701-702)。不为 persona 新增任何 per-message 时序分支。
- **失败回滚**:切换前先解析目标 persona(5.2a 存储);**解析成功才写 registry 并重建**——修复现状"`registry.update` 在 `#materialize` 之前的写-建窗口"(engine.js:644-647,失败会留下指向不存在定义的 meta)。显式切换到不存在的名字 → 404 拒绝该请求:会话保持原 persona 与原实例,消息不派发,UI 选择器回滚。区别于物化期降级(持久化 meta 指向已删除 persona → D-B2:降级标准会话 + notice,不阻塞会话打开)。
- **并发**:切换判定 + registry 写 + 重建在同一 per-session 串行锁内完成(与 06 章目录级写互斥同纪律);并发到达的多次切换按到达顺序收敛、末次生效;重建期间到达的 prompt 排队至重建完成后按新 persona 判定。

### 5.2 RuntimeAPIs:`/api/omp/agent-definitions` 组(worker 定义面;REVISED,R3)

> 命名裁决(R3):评审前草案以 `/api/omp/agents` 同时指本章定义 CRUD 与 04 章 Hub live 实例,资源冲突(评审 H-3)。现拆分为 **`/api/omp/agent-definitions`**(本章)与 **`/api/omp/agent-runs`**(04 章:live/parked 实例的 revive/kill/chat/transcript);`/api/omp/agents` 路径自此**废弃,不得再注册**。类型固定:定义 = `AgentDefinition` 投影(本章 `OmpAgent`),实例 = 04 章 `AgentRun`;配置选择器只消费前者,Hub/WorkStatus 只消费后者。

新端点组(经 `runtimeFetch`,ui-api-decoupling skill 管辖;注册于 omp-host、Basic auth,R4;端点组版本进 `GET /api/omp/capabilities`,R2):

```
GET /api/omp/agent-definitions?directory=<dir>
→ { agents: OmpAgent[], projectAgentsDir: string | null }

OmpAgent = {
  name: string;
  description: string;
  source: "project" | "user" | "bundled";
  filePath?: string;
  // 定义(frontmatter)字段
  model?: string[];              // pattern 数组,如 ["@smol"]
  thinkingLevel?: string;
  tools?: string[];
  spawns?: string[] | "*";
  prewalk?: boolean | string;
  advisor?: boolean | string;
  readSummarize?: boolean;
  // 设置级覆盖(3.3 的 task.* 键投影)
  disabled: boolean;             // task.disabledAgents
  modelOverride?: string;        // task.agentModelOverrides[name]
  prewalkOverride?: string;      // task.agentPrewalk[name]
  advisorOverride?: string;      // task.agentAdvisor[name]
  systemPrompt: string;          // md body(列表可截断,详情端点全量)
}

GET    /api/omp/agent-definitions/:name?directory=<dir>  → OmpAgent(全量 systemPrompt)
POST   /api/omp/agent-definitions?directory=<dir>        → 201 OmpAgent
       body: { scope: "user" | "project", definition: { name, description, systemPrompt,
              model?, thinkingLevel?, tools?, spawns?, prewalk?, advisor? } }
PUT    /api/omp/agent-definitions/:name?directory=<dir>  → OmpAgent
       body: { definition?: Partial<…同上>,
               overrides?: { disabled?, modelOverride?, prewalkOverride?, advisorOverride?,
                              scope?: "layer" | "global" },  // R2-M4:默认 "layer"=写定义所在层;
                                                              // "global"=显式跨项目覆盖(独立动作)
               renameTo?: string }
DELETE /api/omp/agent-definitions/:name?directory=<dir>&scope=user|project → 204
POST   /api/omp/agent-definitions/refresh?directory=<dir> → 204  // 失效运行中会话的 task 工具缓存
```

实现要点:

- 读取:omp-host 调 `discoverAgents(cwd)`——SDK 包根未导出该函数,但 exports map 含 `./task` 与 `./task/*`(package.json exports),可 `import { discoverAgents } from '@oh-my-pi/pi-coding-agent/task'`(task/index.ts:104 `export { discoverAgents, getAgent }`);结果按 directory 缓存(TTL 或文件 mtime 失效),`refresh` 端点调 `refreshAgentDiscovery`(task 模块导出,agents-hub.ts:45 同源用法)。
- 覆盖字段合并逻辑照抄 agents-hub.ts:292-313(disabled set / overrides record join)。
- 写入:
  - `definition` → `<dir>/.omp/agents/<name>.md`(scope=project)或 `~/.omp/agents/<name>.md`(scope=user),YAML frontmatter + body;**bundled 不可写**(409 + 指引复制到 user scope 覆盖——同名 user 定义天然遮蔽 bundled,discovery.ts:120-133 先见者胜)。
  - `overrides` 作用域分层(R2-M4,**取代上轮"一律写全局实例"表述**;写入一律经 06 章设置代理,键 = `task.disabledAgents` / `task.agentModelOverrides` / `task.agentPrewalk` / `task.agentAdvisor`):
    - `scope:"layer"`(默认)= 写**定义所在层**:`source:"project"` 定义的 overrides 写该 directory 项目层(`<dir>/.omp/config.yml` 的 `task.*` 子树),只影响以该目录为 cwd 的会话——两个项目的同名 agent **互不串扰**(上轮全局写法的跨项目互踩缺陷即 R2-M4);`source:"user"` / `"bundled"` 定义的 overrides 写用户层(全局实例)。项目层读写的前提 = 06 §5.1(REVISED R2:每目录 keyed Settings 实例注入)——`createAgentSession({ settings })` 注入口已存在(sdk.ts:1273-1275),实例经 `cloneForCwd` 家族派生(settings.ts:607-652),每会话注入后项目层对该目录会话即成为读取权威。
    - `scope:"global"` = 显式全局覆盖(跨项目生效):UI 独立入口("设为全局覆盖"),**不是项目编辑动作的默认落点**;仅对 project 定义有意义(user/bundled 的 layer 本就是全局层)。
    - **能力门控**:project 定义的 overrides 写入门控在 capabilities **`settings.projectScopes.v1`**(06 章 R2 定义:每目录注入拓扑激活时由 engine 声明,与 `settings.v1` 并列登记;**不是** `settings.v1`——那只门控设置代理 UI)。该 key 未亮时,PUT 对 `source:"project"` 定义的 `overrides` 段一律 409 `{error:"project-overrides-unavailable"}` + 指引(可临时改用显式全局覆盖或等注入);user/bundled 的全局覆盖不受门控。注意注入只解决**读权威**;项目层 `task.*` 的**落盘**仍需 SDK 项目保存路径扩展(现 `#saveProjectNow` 仅持久化 `modelRoles` 子树,settings.ts:2159-2193)→ 该扩展是开门的前置之一,挂开放问题 6。
    - 读投影:`OmpAgent.disabled` / `*Override` 字段 = 该 directory 会话 Settings 合并视图的**生效值**(项目层按键遮蔽用户层,合并语义随 06 章 R2 注入模型);两层来源的分层视图经 `GET /api/omp/settings?directory=`(06 章项目层读)供 UI 分层展示,本端点不重复建模分层。
- 命名冲突:project 已有同名 → POST 拒绝(镜像 agents.js:328-340 的存在性检查,但只查 omp 目录)。

### 5.2a RuntimeAPIs:`/api/omp/personas` 组(persona 定义,OpenChamber 原创层;REVISED,D3/R12)

persona 与 worker 分型(D-B2):worker 定义是 omp 原生(`AgentDefinition`,task 工具派发);persona 是 OpenChamber 原创可选层(顶层会话覆盖),独立类型与资源:

```
GET    /api/omp/personas            → { personas: OmpPersona[] }
GET    /api/omp/personas/:name      → OmpPersona
POST   /api/omp/personas            → 201 OmpPersona   body: { persona: OmpPersona }
PUT    /api/omp/personas/:name      → OmpPersona       body: { persona: OmpPersona }
DELETE /api/omp/personas/:name      → 204

OmpPersona = { name: string; description?: string; systemPrompt?: string; tools?: string[] }
// tools 缺省 = 全量;无 model/thinkingLevel——顶层模型选择归 01 章 model roles,persona 不绑模型
```

- 存储:`<registryRoot>/personas.json`(OpenChamber host 拥有);**不写 `~/.omp`**——persona 非 omp 概念,不得进 omp 发现链(discovery.ts 只扫 `.omp/agents`,天然隔离)。
- 会话绑定:`registry` session meta `persona?: string`;缺省 undefined = 标准会话(D-B2)。
- 注册与鉴权同 R4(omp-host,web server 仅代理);端点组版本进 capabilities(R2)。

### 5.3 契约对齐:删什么、补什么

| OpenCode 字段(types.gen.d.ts:1939-1959;useAgentsStore.ts:402-414) | 处置 | 理由 |
|---|---|---|
| `mode: primary/subagent/all` | **删** | omp 无 primary;顶层化身由 D-B2 承担 |
| `permission: PermissionRuleset` | **删** | omp 审批 = tier + tools.approval(03 章);AgentPermissionsEditor、tooltip permission 图标(ModelControls.tsx:1438-1447/2449-2458)随 07 章删 |
| `temperature` / `topP` | **删** | omp 采样参数是全局设置(06 章),无 per-agent |
| `model: {providerID, modelID}` + `variant` | **替换**为 `model: string[]`(patterns) | omp 是 pattern 数组,支持 `@role`/通配/`:level` 后缀 |
| `options` / `steps` / `native` / `hidden` / `color` | 删(UI 本地色板保留在 agentColors,按 name hash) | 无 omp 对应 |
| (无) | **补** `thinkingLevel` / `spawns` / `prewalk` / `advisor` / `readSummarize` / `source` | 3.2 契约 |
| OC 原创的 `scope` / `group` | `scope`→`source`(语义变化:定义文件真实来源);`group` 由 filePath 子目录派生,保留 UI 分组 | 现状 useAgentsStore.ts:355-357 已按 path 派生 |

UI CRUD 表单(settings/sections/agents)字段重排:Prompt(body)、Description、Model patterns(逗号分隔 chip 输入,占位 `@smol, anthropic/*:high`)、Thinking level、Tools(白名单 tag)、Spawns、Prewalk chip、Advisor chip;per-agent 覆盖区(enabled toggle / model override / prewalk override / advisor override)——覆盖区按**两层来源**呈现(读自 06 章分层视图;生效值 = 合并视图),写入默认落定义所在层,"设为全局覆盖"为显式独立动作(R2-M4,5.2);project 定义的覆盖写入在 `settings.projectScopes.v1` 未亮时置灰 + 指引。

### 5.4 会话模式状态机与 `/api/omp/sessions/:id/mode`

**模式状态机**(engine 侧权威,每 hostSession 一份;互斥规则照抄 TUI 3378-3385 / 3455-3462 / 2696-2703):

```
                ┌──────────────────────────── none ────────────────────────────┐
                │            enter(plan)         enter(goal)      enter(vibe)  │
                ▼                ▼                    ▼                ▼        │
        ┌─────────────┐   ┌──────────┐         ┌──────────┐    ┌──────────┐     │
  exit ◀│ plan        │──▶│plan_paused│ exit(3rd toggle / approve)              │
        │ (read-only, │   └──────────┘                                            │
        │  plan role) │                                                           │
        └─────────────┘                                                           │
        ┌─────────────┐  pause   ┌─────────────┐   resume                         │
  drop ◀│ goal active │◀────────▶│ goal_paused │───┐                              │
        └──────┬──────┘          └─────────────┘   │                              │
               │ complete / budget-limit(→budget-limited)                         │
               ▼                                                                  │
        ┌─────────────┐                                                          │
        │ goal done   │──────────────────────────────────────────────────────────┘
        └─────────────┘
  loop: none ⇄ active(remaining: n | deadline),与 plan 互斥、抑制 goal 续跑(TUI 1468/1471)
```

约束:

- plan / goal / vibe 两两互斥(进入前检查,冲突返回 409 `{conflict: "goal"}` 并附 TUI 同文案:"Exit goal mode first.");
- goal 的 paused 与 plan 的 paused 是不同子状态;`mode` 投影值集合:`none | plan | plan_paused | goal | goal_paused | vibe | loop`;
- mode_change entry 由 omp-host 写(`sessionManager.appendModeChange(mode, payload)`,SDK SessionManager API)——与 TUI 写同一种 entry,保证 omp 会话文件双向兼容。

**端点**(全部 directory-scoped):

```
GET  /api/omp/sessions/:id/mode?directory=<dir>
→ { mode, persona?,
    plan?:  { planFilePath, paused, hasDraftContent, review?: PlanReviewDetails },
    goal?:  Goal & { state },            // goals/state.ts 形状直通
    loop?:  { state: "running"|"paused", remaining?, limit? } }

POST /api/omp/sessions/:id/mode?directory=<dir>
     body: { mode: "plan"|"goal"|"vibe"|"none", action?: "enter"|"exit"|"pause"|"resume",
             planFilePath?, initialPrompt? }
→ { mode, ...同上 }                       // 409 on 互斥;400 on plan.enabled=false
```

能力门控(R2,REVISED):本端点组在 `GET /api/omp/capabilities` 暴露 feature **`modes.v1`**;服务端按 omp-host 实际能力裁决,UI 一律以 capabilities 判断模式面可用性并降级,**不引入 OpenChamber 本地 feature flag**(R2:本地 flag 改由 capabilities 承载)。`plan.enabled` / `goal.enabled` 是产品设置键(06 章),与 capabilities 正交:capabilities 答"端点在不在",设置答"产品让不让"。

engine 侧进入/退出实现 = TUI 逻辑移植(`#enterPlanMode` 2688-2753 / `#exitPlanMode` 2789-2860 / goal 进入 2882-2886 / vibe 3447-3524 的 host 等价),全部走 AgentSession API:toolset 捕获/恢复、`setPlanModeState`、`setPlanProposalHandler`、plan role 模型(经 01 章 role 解析器)、`appendModeChange`。

**模式状态投影事件(REVISED,R1——本章只定名与 payload 草案,通道归 05)**:下表事件经 05 章唯一 `OmpEventBus → /api/omp/events` 通道下发;envelope、事件 ID、directory 作用域、durable/volatile、`Last-Event-ID` 重放与 schema 版本以 05 章事件注册表为唯一权威,本章不自建通道:

| 公开名(`omp.<域>.<事件>`)| payload 草案 | 触发(producer = omp-host)|
|---|---|---|
| `omp.mode.changed` | `{ sessionID, mode, data? }` | enter/exit/pause/resume 后(mode 投影值集合见上)|
| `omp.goal.updated` | `{ sessionID, goal, state? }` | engine `goal_updated` 直通(D2 处置;GAP-B13)|
| `omp.plan.review_requested` | `{ sessionID, details }` | `xd://propose` resolve(`preparePlanForReview` 结果)|
| `omp.plan.updated` | `{ sessionID, planFilePath }` | plan 文件写入/编辑 |

冷启动恢复:`#materialize` 读会话 mode_change entries,等价 `#reconcileModeFromSession`(2626-2686)恢复 plan/plan_paused/goal 状态并投影一次 `omp.mode.changed`。UI 重连对账按 sync-state-invariants:以 `GET /api/omp/sessions/:id/mode` 为权威快照(纳入 05 章 bootstrap/resync 矩阵的 modes 段)。

### 5.5 plan review 流(裁决:**替换**合成文本协议)

**决策 D-B4:omp plan mode 全量落地;OpenChamber 合成文本 PlanView 被**替换**,UI 资产保留复用。**

理由:合成标记检测(usePlanDetection.ts:42-44)无法承载 review/approve/tier/refine/plan 文件生命周期,且 omp plan mode 是引擎级模式(toolset/model/只读保证),两套并存 = 两个"plan"概念(master D3 明示合并)。保留的资产:Plan tab/rail 容器、markdown 渲染;数据源与触发全换。

**流(时序)**:

1. `POST mode {mode:"plan", initialPrompt?}` → engine 进 plan mode,emit `omp.mode.changed {mode:"plan", data:{planFilePath}}`;UI status 徽章 + composer 提示"Plan mode enabled. Plan file: local://PLAN.md"(TUI 文案)。
2. 模型起草 plan(`write` 到 `local://PLAN.md`)→ 每次落盘 emit `omp.plan.updated`。
3. 模型 resolve(`xd://propose`)→ host 的 proposal handler 调 `session.preparePlanForReview(title)`(agent-session.ts:933-938)得 `PlanApprovalDetails` → emit `omp.plan.review_requested {details}` 并把工具结果挂起(pending-promise;UI 未决议前该 turn 不结束,镜像 TUI overlay 阻塞语义)。
4. UI 打开 **PlanReviewOverlay**(新 React 组件,复刻 plan-review-overlay.ts):分节 markdown + Contents、节删除(undo)、行/节 annotation → 反馈框、tier slider(cycleOrder 角色环,01 章模型数据)、外部编辑按钮(打开 04 章文件面板,`local://` 根)。
5. 用户四选一(`POST /api/omp/sessions/:id/plan/review`):

```
POST /api/omp/sessions/:id/plan/review?directory=<dir>
     body: { choice: "approve-execute" | "approve-compact" | "approve-keep" | "refine",
             editedContent?,        // in-overlay 编辑(节删除)后的全文
             feedback?,             // refine 回路:annotation 反馈文本
             executionRole? }       // tier slider 选择(role id)
→ { dispatched: boolean, mode }     // refine → { dispatched:false } 且 engine 以 feedback 重新提示
```

6. engine 执行 `#approvePlan` 等价(3205-3364):
   - `approve-execute`:退模式 → **清 transcript + 迁移 local 工件 + plan 写新 root + 合成 plan-approved prompt**。host 需新增 `engine.clear(sessionID)`(SessionManager 层 reset,同会话 fresh start;备选:fork 新会话——**不取**,断会话树且丢失 plan-mode 上下文链接,与 TUI 行为不一致);清空后照常 emit `session.updated` + 全量 message 重放(UI 现有 sync 对账处理)。
   - `approve-compact` / `approve-keep`:压缩(compaction 事件归 05 章)或保上下文;
   - 全部路径:恢复执行 toolset(强制 `read`)、`setPlanReferencePath`、应用 `executionRole`、从 plan 标题播种会话名、派发 synthetic prompt(3316-3361 逐条对应)。
   - `refine`:resolve 挂起以"继续细化"结果放行,engine 以 `feedback` 作为下一 turn 用户输入(TUI 4066-4086:feedback 非空才提交,空则仅状态提示)。
7. `/plan-review` 重开:`GET /api/omp/sessions/:id/plan?directory=` 返回 `{planFilePath, content, review?}`,UI 可随时重开 overlay(TUI `/plan-review` 行为)。

`planModeExperimentalEnabled` 退役为两段式(6.4,REVISED):模式端点上线即**停产停用**合成文本协议,P3 物理清扫;Plan tab 门控从 feature flag 改为 `mode === plan* || plan review 存在`。

### 5.6 goal 模式:P1 状态投影 + 显式操作面;P2 自主续跑驱动器(REVISED,R12/D4)

**P1(GAP-B08):状态投影 + 显式用户操作**。端点:

```
GET  /api/omp/sessions/:id/goal?directory=<dir>  → { goal, state } | { goal: null }
POST /api/omp/sessions/:id/goal?directory=<dir>
     body: { op: "set" | "drop" | "pause" | "resume" | "budget" | "complete",
             objective?, tokenBudget? }
→ { goal, state }          // op=set 覆盖已有 active goal 需先 drop(TUI 3610-3650 语义)
```

engine 直通 `session.goalRuntime.*`(runtime.ts:384-496:createGoal/replaceGoal/resumeGoal/pauseGoal/dropGoal/completeGoalFromTool;budget 经 tokenBudget 校验与 budget steer 路径)。每次状态变化 engine `goal_updated` → `omp.goal.updated`(GAP-B13;经 05 章通道)。**P1 不含任何自动续跑**:回合结束后 goal 停在当前状态等待显式操作——master D4"goal 仅状态投影 + 显式用户操作"。

**P2(GAP-B14,REVISED 降级):自主续跑驱动器**。master D4/D6 R12:host 侧 auto-loop 涉及预算触顶、幂等、abort、进程重启恢复,风险高于可见性桥,降为 P2;以下测试**先行**(作为实现合入的前置验收):

- 预算:tokenBudget 触顶 steer → `budget-limited` 状态机在 host 驱动下收敛;
- 幂等:agent_end 重复触发/定时器竞态不产生双份续跑 prompt;
- abort:用户抢先输入、drop/pause、显式取消逐条走查(TUI 1484-1497 守卫);
- 重启恢复:host 重启后从 goal 状态恢复驱动,续跑不重不漏。

**决策 D-B5:驱动器放 omp-host(engine 侧),不放 UI(设计保留,阶段降 P2)。**

- TUI 等价物在输入循环(interactive-mode.ts:1466-1507);web 客户端没有常驻输入循环,且 goal mode 的产品价值 = 关掉浏览器也在跑(host 进程常驻)。
- 实现:`#handleEngineEvent` 的 `agent_end` 分支(engine.js:582-596)后追加——`goalModeState.enabled && goal.status==='active' && !loopMode && settings goal.continuationModes 含 "interactive"` → 800ms timer → `session.prompt(continuationPrompt, {customType:'goal-continuation', display:false})`。omp-host 的 `session.prompt` 调用需透传 customType/display(SDK `sendCustomMessage`/`promptCustomMessage` 通道,agent-session.ts:4992-4996 的同族 API);timer 在 abort/drop/pause/用户抢先输入时取消(TUI 1484-1497 的守卫逐条对应)。
- 备选(UI 驱动:`omp.goal.updated` + idle → UI 定时发隐藏消息):**不取**——浏览器关闭即断跑;且隐藏合成消息经 wire prompt 通道会渲染成用户气泡。
- budget 触顶:`goal-budget-limit` 隐藏消息 + `#sendBudgetLimitSteer` 均为 SDK 内部行为,host 只需投影 `omp.goal.updated {goal:{status:'budget-limited'}}` 并在 UI 徽章显示(5.9)。

loop mode(5.7)复用同一驱动器骨架(agent_end → remaining>0 → 重提交),同为 P2。

### 5.7 vibe / loop / prewalk / advisor / btw / tan 裁决

| 模式 | 裁决 | 优先级 | 依据与表面草图 |
|---|---|---|---|
| **loop** | 实现 | P2 | 驱动器 = D-B5 同机制(GAP-B14 同列车);端点 `POST /api/omp/sessions/:id/mode {mode:"loop", count?|durationMs?, prompt?}`;投影 `omp.mode.changed {mode:"loop", data:{state, remaining, limit}}`;UI:mode 徽章 `Loop running (n left)` + Esc/按钮取消当次迭代。低风险:纯 host 重提交。 |
| **vibe** | defer(倾向实现) | P2 后段 | 依赖 04 章 worker 会话面(live roster/parked/IRC)才有产品意义——vibe = 持久 worker 池 + read-only 主会话。先决条件落地后:host 维护 worker session set(进入注册 vibe 工具集、退出全部 abort),`omp.mode.changed {mode:"vibe"}` + worker 列表并入 Agent Hub 面。若 04 章砍 worker UI,则 vibe 一并砍(开放问题 2)。 |
| **prewalk** | 实现(chips 先行) | P2 | per-agent chips 随 B03/B05 免费获得(`task.agentPrewalk`);会话级 `/prewalk` 等价 = `POST /api/omp/sessions/:id/prewalk {target?}` → host 调 PrewalkCoordinator arm;投影 `omp.mode.changed {mode:"prewalk", data:{target}}`(status segment 中 prewalk 位)。引擎行为(首次 edit/write 切 smol)SDK 内置,host 无需驱动。 |
| **advisor** | 实现(chips 先行) | P2 | `advisor.enabled` 经 06 章设置面;per-agent chips = `task.agentAdvisor`;UI:advisor 卡片(customType `advisor`,05 章流内元素)+ model 徽章 `++`。 |
| **btw** | defer | P2+ | 依赖 04 章(branch 到新会话 + agent:// artifact)。草图:composer 旁问按钮 → 侧面板流式渲染(running/complete/branching/aborted/error)→ "转入正式会话" = 04 章会话树 branch。 |
| **tan** | defer | P2+ | 依赖 04 章 async jobs + agent://。草图:`/tan <work>` → job 派发 pill(`◌ Tangent dispatched <jobId>`)+ 完成行(`✓ Background job completed`);`/jobs` 面板。 |

### 5.8 engine.js 改造点汇总

| 位置 | 改造 |
|---|---|
| endpoints.js:361-376 | `GET /agent` → 返回 `[]`(07 章删除);新增 `/api/omp/agent-definitions*` 与 `/api/omp/personas*` 路由组(live 实例面归 04 章 `agent-runs`) |
| endpoints.js:348-356 | `PATCH /config` agents 分支删除(sidecar 亡) |
| engine.js:451-472 | `customAgents.get` → `discoverAgents` persona 解析;`planYolo` 行删除;`meta.agent` 读迁移值(6.1) |
| engine.js:639-649 | `agentName` 默认链删除(默认 = persona 未设);重建条件改 personaKey 比较(D-B3);重建前模式退出;**切换写-建顺序修复**:persona 解析成功才 `registry.update` + 重建,失败 404 不写(R2-M3,修复现状 engine.js:644-647 先写后建窗口);切换判定入 per-session 串行锁(D-B3 并发) |
| engine.js:89-120 | sidecar load/save/upsert/delete 删除(迁移后,6.2) |
| engine.js:582-596(`agent_end`) | 后置 goal/loop 续跑驱动器(5.6;P2,GAP-B14 前置测试先行);`isTerminal` 处置归 05 章统一 |
| engine.js:611-613(default) | 增 `case 'goal_updated'` → `omp.goal.updated`(经 05 章 `OmpEventBus` 通道,注册表登记);其余 drop 处置归 05 章 |
| engine.js 新增 | `enterMode/exitMode/setGoal/planReview/clear` 方法;`#materialize` 模式恢复;proposal handler Promise 桥 |

### 5.9 UI 组件与 store 改造

- `useAgentsStore`:数据源切 `/api/omp/agent-definitions`(`listAgents` 弃用);CRUD 改投新端点(POST 建到集合、PUT 更新);`AgentConfig` 类型重写(5.3 契约);删除 temperature/top_p/permission 表单字段。新增 personas slice(或独立 `usePersonasStore`):数据源 `/api/omp/personas`,供 persona 选择器(会话级显式切换,R2-M3;**无 @mention 解析**——单条消息路由已删除,5.1)。
- 新 `useSessionModeStore`(zustand):`modeBySession`、`goalBySession`、`planBySession`,reducer 消费**经 05 章 `/api/omp/events` 通道下发**的 `omp.mode.changed` / `omp.goal.updated` / `omp.plan.*`(R1:不自建订阅),重连对账拉 `GET /api/omp/sessions/:id/mode`(sync-state-invariants:权威 = server,乐观 = 本地立即切徽章、409 回滚);模式面可用性按 capabilities `modes.v1` 判断(R2),不用本地 flag。
- `ModelControls`:persona 选择器(默认"标准",数据源 = `/api/omp/personas`;worker 定义不进 composer——worker 经 task 工具/04 章 Hub 派发);删除 build 回退三处 + mobileControlsUtils/GitHubIssuePickerDialog/NewWorktreeDialog/agentColors 的 'build' 特判(改 name-hash 色板);删除 permission 摘要图标。
- 新 `ModeBadge`(composer/Header):plan(paused 变体)/goal(status 图标 + used/budget)/prewalk/vibe/loop,文案对齐 segments.ts:228-270;`SessionNodeItem` 的 goal status icon 已有雏形(SessionNodeItem.tsx:458-463)接同一 store。
- 新 `PlanReviewOverlay`(5.5)+ `GoalPanel`(set/drop/pause/resume/budget/status 表单,objective + tokenBudget 输入);`/plan` `/goal` `/loop` 斜杠命令注册进现有 command palette(与 magic prompts 并存,路由到 mode 端点)。
- `usePlanDetection` / `planModeExperimentalEnabled` 读取(App.tsx:404-424)删除;Plan tab 数据源切 `useSessionModeStore`。

---

## 6. 迁移与兼容

### 6.1 存量会话 `meta.agent`

- `registry`(SessionMetaRegistry)读路径迁移:`'build'` 与 `'plan'` → `persona: undefined`(标准会话);其它名字 → `persona` 保留。写一次性 migration(读取时惰性归一 + 保存),或一次性脚本(推荐:`migrate-agents.js` 遍历 registry 文件)。
- 'plan' 会话的**行为变化**必须公告:现状是"隐形只读 + 审批崩溃"(2.1),迁移后回到标准会话(全工具、正常审批)——是缺陷修复,不是行为回退;CHANGELOG 明示。
- 兼容窗口:engine 同时接受 `agent`/`persona` 字段一个版本(`agent` 优先级低且仅用于归一),之后删 `agent`。

### 6.2 omp-host sidecar(`openchamber-agents.json`)→ `.omp/agents/*.md`

一次性 import:每个 sidecar agent 写 `~/.omp/agents/<name>.md`(worker 面;frontmatter:description/tools,body:prompt),并**镜像一条 `OmpPersona`** 进 `<registryRoot>/personas.json`(顶层化身面:存量会话 `meta.agent` 指向 sidecar 名的,迁移后 `meta.persona` 仍可解析,不断裂——D-B2 分型后两面各自存续);成功后 sidecar 改名 `.migrated-<ts>`(不直接删,可回滚)。失败(文件系统错误)保留原文件并在日志 + `/health` 标注迁移挂起。并发:迁移在 engine boot 早期、请求面打开前执行,无并发问题。

### 6.3 OpenCode agent 存量(`~/.config/opencode/agent/` 与 `<dir>/.opencode/agent/`)

**不自动迁移**(字段语义冲突:permission/temperature/top_p 无处安放;mode 含义相反)。提供显式 import:`POST /api/omp/agent-definitions/import-opencode?directory=` —— 逐个转写(description/prompt/model→单元素 pattern 数组,variant→`:level` 后缀尽力而为),跳过冲突字段并在响应中列明 `droppedFields`。旧文件保留原地(OpenCode 已死路径,07 章连同 `/api/config/agents` 路由删除)。

### 6.4 `planModeExperimentalEnabled` 退役:两段式(REVISED,R12)

**第一段:停产停用——模式端点上线即执行(随 P0 概念迁移/P1 模式端点列车),不等 P3**:

- 停止生产:server `/health` 停发 `planModeExperimentalEnabled`(可保留一版返回 false + deprecation log);
- 停止消费:App.tsx:404-424 读取、`useFeatureFlagsStore.planModeEnabled`、registry.ts:203-205 门控、`usePlanDetection` 扫描全部停用;Plan tab 改 `useSessionModeStore` 驱动(门控 = `mode === plan* || plan review 存在`)。
- 无存量数据风险:合成标记是运行时产物不持久化;旧会话残留的标记文本在新协议下只是普通 assistant 文本,无副作用。

**第二段:遗留清扫(P3,与 07 章大扫除同列车)**:上述停用代码、flag 定义、i18n 文案、`usePlanDetection` 文件物理删除。

**阶段门控(R2,REVISED)**:模式端点组经 `GET /api/omp/capabilities` 的 feature `modes.v1` 门控,服务端裁决;**不引入新的 OpenChamber 本地 feature flag**(R2:本地 flag 改由 capabilities 承载)。`plan.enabled` / `goal.enabled` 是产品设置键(settings-schema.ts:4384-4410 默认均 true),与 capabilities 正交。

### 6.5 回滚策略

- `/api/omp/*` 端点组独立于 wire 面;回滚开关经 capabilities 裁决(R2):omp-host 撤下 `modes.v1` / agent-definitions 组版本,UI 按 capabilities 自动降级回 wire 面,不依赖本地 flag(但 `/agent` 制造面已删,降级即无 agent 数据——因此 B01 与 07 章的 `/agent` 删除**必须同列车发布**)。
- sidecar 迁移:改名保留 + `POST /api/omp/agent-definitions/rollback-sidecar`(仅迁移未消费前);`.omp/agents/*.md` 是用户可见文件,删除即回滚。
- 模式状态:mode_change entries 与 TUI 双向兼容,回滚(禁用模式端点)后 omp 会话文件里的 mode entries 对 TUI 仍可恢复(`#reconcileModeFromSession`)。

---

## 7. 验证方案(设计;未执行)

**单元/集成(omp-host,bun:test;server JS `node --check`)**

1. agents 投影:临时 `.omp/agents` + `~/.omp/agents` 目录 → `GET /api/omp/agent-definitions` 断言 source 优先级、同名遮蔽、bundled 合并(discovery.ts:70-138 对照)、overrides 合并(agents-hub.ts:292-313 对照)。
2. CRUD:POST→文件落盘 frontmatter/body;PUT overrides 作用域(R2-M4):project 定义默认(`scope:"layer"`)→ 该 directory 项目层 `task.*` 变化、显式 `scope:"global"` → 用户层变化、`settings.projectScopes.v1` 未亮时 project overrides → 409;**双项目同名 agent 隔离**(A 目录 override 不改变 B 目录会话的生效值);bundled 写 409;DELETE 按 scope。
3. persona:meta.agent='build'/'plan' 归一为 standard 且**不触发重建**(6.1);未知 persona 名降级 + notice;`/api/omp/personas` CRUD 往返与默认无 persona 的标准会话行为;**切换生命周期(R2-M3,5.1 D-B3)**:显式切换 → registry 持久化 + 重建后新 persona 生效且后续 turn 保持(无自动恢复);切到不存在名字 → 404、会话保持原 persona 原实例且消息不派发;流式中切换 → 重建后触发消息 steer 进新会话;并发切换按序收敛末次生效。
4. 模式状态机:互斥矩阵(plan×goal×vibe 全组合断言 409)、pause/resume 三态 toggle(3398-3409 对照)、`plan.enabled=false` → 400;capabilities `modes.v1=false` → 模式端点拒绝且 UI 降级不崩(R2 矩阵:新 UI+旧 engine)。
5. plan review:fake session 的 proposal handler → `omp.plan.review_requested`(经 05 章通道);四 choice 的 approve 路径断言(toolset 恢复含 read、planReferencePath、synthetic prompt 派发、execute 路径 transcript 清空 + local 工件迁移)。
6. goal:op 全集直通 runtime;`goal_updated` → `omp.goal.updated` 事件形状(经 05 章通道)——P1;续跑驱动器 fake timers(agent_end → 800ms → 隐藏重提交;abort/drop 取消)+ 预算触顶收敛 + 幂等(重复 agent_end 不双发)+ host 重启恢复——P2(GAP-B14 前置测试)。
7. 迁移:sidecar import md 内容逐字段;OpenCode import 的 droppedFields 清单。

**E2E(dev 栈 5180 UI / 3902 server,浏览器驱动)**

1. 定义与 persona 全流程:worker 定义建 → 出现在定义管理面与 per-agent chips → 改 model pattern → 删;persona 建 → 出现在 persona 选择器 → 选 persona 开新会话 systemPrompt/toolset 生效(与"标准"会话对照)→ 会话中途**显式切换** persona(变更即重建、后续 turn 保持、切回"标准";composer 无 @mention 入口)。
2. plan 全流程:进模式(徽章 + 只读提示)→ 起草(plan 文件面)→ review overlay(节删除/annotation/tier slider)→ approve-execute(transcript 清空 + plan-approved 回合开跑)→ refine 回路。
3. goal(P1):set → 徽章 used/budget → pause/resume → budget 触顶徽章变化 → drop;P1 验证停在"回合结束 goal 保持 active 等显式操作"。关浏览器标签 30s 后回合继续(host 续跑)——P2 场景(GAP-B14)。
4. 旧旗标路径:模式端点上线后 Plan tab 由 mode store 驱动,`/health` 旗标读取已停产(6.4 第一段);P3 物理删除后不复发。

**TUI 对照行为(D5)**

- `/plan` 三态 toggle 文案与顺序;"Plan mode enabled. Plan file: …"文案;四选项逐字(3979-3982);approve-execute 后会话名 = plan 标题(3316-3324)。
- goal segment 图标语义(active/paused/complete/budget-limited/dropped,segments.ts:228-270);`/goal` 子命令集合(builtin-modes.ts:248-256)。
- agents hub:source 分组计数、per-agent chips 生效(新会话派发时 model pattern 命中)。

---

## 8. 开放问题

1. **(已裁决,REVISED)顶层化身(persona)口径**。master D3/D6 R12 已裁:persona = OpenChamber 原创可选层,**独立类型与资源,默认无,不复用 worker agent id**——本章 5.2a `OmpPersona` + `/api/omp/personas` 即落地;旧案"persona 解析复用 discoverAgents/worker 定义"作废。残余:① `/api/omp/personas` 尚未列入 master R3 路径清单(D3 已裁"独立资源",建议总纲下次修订把该路径补进 R3 清单,避免被当违规新增);② multirun 化身输入迁移的对接细节归 08 章定案。
2. **vibe mode 是否进 P2**。依赖 04 章 worker 会话/parked/IRC 面;若 04 章裁剪 worker UI,vibe 应同步砍掉而非做半套。建议:04 章定案前 vibe 保持 defer,本章不预留半成品端点。
3. **goal 续跑驱动器的 `continuationModes` 语义**。omp 键默认 `["interactive"]` 指 TUI 交互宿主;host 驱动是否等价于 "interactive"?建议:host 实现映射为 interactive(当前唯一模式),设置面文案按 06 章代理时改写为宿主中立描述,不新造模式值。
4. **`engine.clear(sessionID)` 的会话清空语义**(5.5 approve-execute)。`handleClearCommand` 在 TUI 还承担 UI 状态清理;host 侧只需 transcript/local-root 语义,需在实现时确认 SessionManager 的 reset/branch API 边界(与 04 章会话树协调,避免与 fork/branch 语义打架)。
5. **OpenCode agent 存量是否值得提供 import**(6.3)。若用户群几乎无存量,可降级为文档说明;建议先统计 `/api/config/agents` 的历史写入量再定。
6. **(R2-M4 已改判)端点鉴权/overrides 写层与项目作用域**。R4 不变:本章全部路由注册在 omp-host(Basic auth),web server 仅 `/api` 代理。上轮"R6 → task.* 一律写全局实例"的表述**作废**:R6 的前提是"`Settings.init` 进程单例**不可注入前**"(master D6-R6),06 章 R2 修订落地**每目录 keyed Settings 实例注入**(06 §5.1 REVISED R2;`createAgentSession({ settings })` sdk.ts:1273-1275,`cloneForCwd` 家族 settings.ts:607-652)后前提翻转——project 定义的 overrides 改写项目层、全局覆盖为显式独立动作、`settings.projectScopes.v1` 门控(5.2)。**残余(需 06 章/上游对齐)**:① 项目层 `task.*` 的**落盘通道**:注入只交付读权威,SDK 原生项目保存路径仍仅持久化 `modelRoles` 子树(`#saveProjectNow`,settings.ts:2159-2193)——项目层承载 `task.*` 需 06 章定义宿主侧写路径或上游 save-path 扩展(上游 issue);② `settings.projectScopes.v1` 的声明时机与翻转列车随 06 章 R2 定稿,本章只消费不定义。
7. **(R2-M3 新登记)per-turn persona 上游依赖**。单条消息 persona 路由("`@persona` mention:本条用 X、下条恢复")已从 P0 承诺中删除(5.1);若产品将来要 per-turn 化身,需 SDK 原生 per-turn system/tool overlay——现 AgentSession 仅构造时接受 `systemPrompt`/`toolNames`(engine.js:465-472 host 用法),运行时最近 API `setActiveToolsByName()`(agent-session.ts:4586-4588)仅 toolset,无 system prompt 替换、无 per-turn 作用域;用"销毁重建会话"模拟 per-turn 即评审 M3 指出的无生命周期方案,不再考虑。动作:上游 issue;本章不设计、不预留语法/端点/字段。

---

## 9. 依赖

**前置**:

- 01 章:model roles 解析器(plan role、cycleOrder、`@role` pattern 展开)——plan mode 模型切换与 review tier slider 直接消费。
- 06 章:设置代理面(`plan.enabled`/`goal.*`/`task.*`/`advisor.*`/`prewalk.*` 读写)——overrides chips 与模式门控;**每目录 keyed Settings 实例注入(06 §5.1 REVISED R2,R2-M4 前置)**:project 定义 overrides 的项目层写入、生效值合并视图与 `settings.projectScopes.v1` 能力位均依赖该模型——注入(及项目层 `task.*` 落盘扩展,开放问题 6①)落地前,project-agent overrides capability 保持关闭(5.2)。
- 05 章:唯一事件通道(`OmpEventBus → /api/omp/events`)与事件注册表——本章 `omp.mode.changed` / `omp.goal.updated` / `omp.plan.review_requested` / `omp.plan.updated` 只登记不自建(R1);capabilities 协商端点(R2)。

**后置**:

- 04 章:`local://` 根解析(plan 文件)、Agent Hub/parked(`/api/omp/agent-runs`,vibe 前置)、async jobs + `agent://`(btw/tan 前置)、会话树(approve-execute 清空与 branch 语义)。
- 07 章:`/agent` 制造面、`/api/config/agents` 路由、permission 字段全家、合成 plan 协议遗留清扫(P3;停产已随模式端点列车先行,6.4)。
- 08 章:multirun/AgentManager/scheduled tasks 的 agent 输入迁移到 persona/worker 语义。
