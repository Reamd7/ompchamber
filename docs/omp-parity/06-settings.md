# 06 设置体系(Settings):设置面代理 omp schema,清除平行宇宙

状态:设计阶段(2026-08-19 修订轮;2026-08-20 修订轮 2 落位 R2 评审 H3:§5.1 改为每目录 keyed Settings 实例注入,见 OQ-F1 修订登记;对齐 `00-MASTER.md` D6 冻结契约 R1/R2/R3/R4/R9/R12,冲突以总纲为准)
域代号:F
裁决原则引用:D1(omp 原生概念走 RuntimeAPIs `/api/omp/...`)、D2(投影与权威)、D3(默认模型 = `modelRoles.default`,存量只读检测 + 用户确认显式导入)、D4(P0 含"默认模型链改读 omp settings")、D5(TUI 源码为规格)、D6(R1 事件单通道、R2 capabilities、R3 路径复数、R4 进程归属、R6 Settings 多目录——R2 修订:会话消费改每目录 keyed 实例注入(§5.1 REVISED R2),总纲回写挂 §8 OQ-F1、R9 credential 脱敏、R12 杂项裁决)

证据缩写:
- `<s>` = `C:/Users/reamd/.bun/install/global/node_modules/@oh-my-pi/pi-coding-agent/src`(omp SDK 权威副本)
- `ui/` = `packages/ui/src`、`server/` = `packages/web/server`(OpenChamber 仓库相对路径)

---

## 1. 域概述与边界

### 1.1 本域管什么

1. **omp 设置的代理面**:OpenChamber 设置 UI 通过 RuntimeAPIs + `runtimeFetch` 的新端点组(`/api/omp/settings`)读取并写入 omp 的 `SETTINGS_SCHEMA`(`<s>`/config/settings-schema.ts:391,5,880 行、约 360 个键定义),落盘到 `~/.omp/agent/config.yml`(全局)与 `<cwd>/.omp/config.yml`(项目层,仅 `modelRoles` 子树权威)。OpenChamber **不复制 schema、不平行存储**,schema 即产品(`ui{tab,group,label,description}` 就是设置面板布局)。
2. **OpenChamber 平行设置宇宙的退役**:OC 自有 `~/.config/ompchamber/settings.json`(server/lib/opencode/proxy.js:696 证实路径)中与 omp 语义重叠的键(`defaultModel`/`defaultVariant`/`defaultAgent`/`permissionAutoAccept`/`planModeExperimentalEnabled` 等)的处置、迁移与退役。
3. **一致性与安全语义**:同一份 config.yml 被 TUI 与 web 并发编辑时的合并语义(omp 的按键合并 + 文件锁 + debounce)、变更通知(file watch → `omp.settings.updated`,经 05 章唯一通道)、credential 键的永不回显(write-only,R9 扩展到全部出口)。
4. **暴露范围策略**:omp 10 个设置 tab(`SETTING_TABS` `<s>`/config/settings-schema.ts:95-106)中哪些在 OpenChamber 首批露出、哪些永不露出(TUI 专属外观/终端渲染键)。

### 1.2 本域不管什么

| 相邻域 | 边界 |
|---|---|
| 01 模型选择与 roles | `modelRoles` 的**产品语义**(角色解析链、`@role` 别名、priority.json 兜底、会话头徽章)归 01;本章只管这些键的**编辑面**(settings 页如何读写 `modelRoles.default` 等键、`modelRoleStorage` 的 global/project 落盘) |
| 02 Agent 与模式 | `plan.enabled`/`goal.enabled` 的**运行时行为**(plan mode 会话态、read-only 门控)归 02;本章只管键的读写与 `planModeExperimentalEnabled` OC flag 的退役 |
| 03 审批与交互 | `tools.approvalMode`/`tools.approval`/`bash.patterns` 的**解析与弹窗桥**(tier 比较、tool_approval 桥)归 03;本章只管这些键的编辑面与 OC `permissionAutoAccept` 键删除 |
| 05 事件流 | **R1 事件单通道**:`omp.settings.updated` 的通道/envelope/事件 ID/重放/reducer 全部归 05 章唯一注册表与唯一 `OmpEventBus → /api/omp/events` SSE;本章只认领事件名、payload 形状与触发时机,**不自建通道/broadcaster** |
| 07 OpenCode 残留 | wire `/config` GET/PATCH(现只承载 custom agents,server/lib/omp-host/endpoints.js:347-356)的删除归 07;BehaviorPage 的挂载点迁移处置**本章已裁定**(§3.6/§5.8,07 章 GAP-G13 目标以此为准),旧路径清理动作在 07 清单 |
| 08 原创面 | OC 原创功能(small-model、walkthrough、通知系统)自身逻辑归 08;本章决定其设置键的存留 |

### 1.3 对外接口

- **端点**:`GET/PUT /api/omp/settings`(新,见 §5.2/§5.3;集合复数路径,R3)。**R4 进程归属**:两端点与 watcher 只注册/运行在 omp-host 进程内(Basic auth),web server 仅做既有 `/api` 透传代理,不自行接触 Settings 实例或 config.yml。
- **能力协商(R2)**:设置面以 `GET /api/omp/capabilities` 暴露的 capability 门控——代理端点组 = `settings.v1`;**每目录项目层语义(会话消费 + project-scope 写)= `settings.projectScopes.v1`**(修订轮 2 新增:02 章项目 agent overrides 等跨章消费方按此键门控,不得各自发明);**不使用**本地 env/runtime feature flag(`OMPCHAMBER_OMP_SETTINGS` 之类设计已废弃,§6.2)。capabilities 端点形状归总纲/05 章,本章只消费。
- **事件**:`omp.settings.updated`(经 05 章唯一 `OmpEventBus → /api/omp/events` SSE 通道下发,D1/R1;本章不进 OpenCode wire 生成类型、不另建通道)。
- **UI**:SettingsView 新增 omp 导航组;`DefaultsSettings` 页重构;`useOmpSettingsStore` 新 store。

---

## 2. 现状分析(OpenChamber 侧)

### 2.1 OC 宿主设置:文件、路由、写路径

- **存储**:`~/.config/ompchamber/settings.json`,JSON,原子写(临时文件 + rename,Windows EPERM 回退 copyFile)且 0600/0700 权限收紧——server/lib/opencode/settings-runtime.js:503-521(`writeSettingsToDisk`)、:474-501(`replaceFile`)。路径硬编码证据:proxy.js:696。
- **读路径**:`readSettingsFromDiskMigrated` 串 8 级迁移(lastDirectory→projects、theme 拆分、collapsedProjects、通知默认值、tunnel 键改名、路径规范化、确定性 projectId、删 approvedDirectories)——settings-runtime.js:806-820。
- **写路径**:`persistSettings(changes)` 经 `persistSettingsLock` 串行链:读盘 → `sanitizeSettingsUpdate` → `mergePersistedSettings` → 规范化/迁移 → 项目校验 → 原子写——settings-runtime.js:822-897。日志只打字段名防凭据泄漏(:824-826)。
- **路由**:`GET /api/config/settings` routes.js:140-148;`PUT /api/config/settings` routes.js:234-243。VS Code 运行时走 `getRegisteredRuntimeAPIs()?.settings.load/save`(ui/stores/useConfigStore.ts:85-88;注册形状 ui/contexts/runtimeAPIRegistry.ts:5-7)。
- **键面**(ui/lib/desktop.ts:47-226 `DesktopSettings`,节选与本章相关者):`defaultModel`/`defaultVariant`/`defaultAgent`(:125-127)、`smallModelUseDefault`/`smallModelOverride`/`walkthroughModelOverride`(:128-138)、`notifyOnCompletion/OnError/OnQuestion` + `notificationTemplates`(:79-92)、`permissionAutoAccept`(server 侧 sanitize:settings-helpers.js:216-231)、`globalBehaviorPrompt` + `responseStyle*`(:209-213)、theme/字体/终端/布局键(:48-188)。

### 2.2 默认模型/agent 级联(omp 语义的头号分歧点)

`resolveDefaultAgentModelSelection`(ui/stores/useConfigStore.ts:294-399,级联注释 :282-293):

```
Agent: settings.defaultAgent(OC)→ opencode default_agent → 硬编码 'build' → first primary → first
Model: project.defaultModel → settings.defaultModel(OC)→ agent 钉住的 model+variant
       → opencode config.model → 'opencode/big-pickle' 硬编码 → first provider model
```

- `settingsDefaultModel/defaultAgent` 来源:`fetchOpenChamberDefaults`(useConfigStore.ts:70-172)先试 VS Code RuntimeAPIs,再 GET `/api/config/settings`。
- UI 编辑面:`DefaultsSettings`(ui/components/sections/openchamber/DefaultsSettings.tsx):61-142 加载;`handleModelChange` :144-176 **双通道写**(本地 store + `updateDesktopSettings` + PUT `/api/config/settings`);`handleAgentChange` :203-220;small-model 组 :222-260。
- **问题**:该级联在 omp 引擎之上再造了一层"默认模型"。omp 真值是 `modelRoles.default`(无 defaultModel/defaultProvider 键,§3.2);engine 侧 `createAgentSession` 本已按 omp 语义解析默认角色(server/lib/omp-host/engine.js:443-447 注释:"createAgentSession resolves the settings default (defaultModel / defaultProvider) exactly like the TUI"),但 UI 级联会在每个 prompt 强制携带 model 参数盖掉它。

### 2.3 BehaviorPage:OpenCode 时代的全局 prompt(REVISED:死链结论撤回)

- `AGENTS_MD_PATH = '~/.config/opencode/AGENTS.md'`(ui/components/sections/behavior/BehaviorPage.tsx:33);读写经 `GET/PUT /api/behavior/agents-md`(server/lib/opencode/routes.js:624-643,服务端同一硬编码路径 routes.js:621)。
- i18n 文案明示 OpenCode 语义:"Changes made here update {path}. OpenCode also includes any project-level AGENTS.md rules"(ui/lib/i18n/messages/en.settings.ts:597)。
- `responseStyle*` 是 OC 原创:首消息注入风格指令,"sent with your first message and do not change your global AGENTS.md rules"(en.settings.ts:606)。
- **问题(2026-08-19 复核修正,原"死链"结论错误)**:omp SDK 的 OpenCode 兼容 provider **仍读**该文件——但仅**用户级**(`<s>`/discovery/opencode.ts:114-115 `getUserPath(ctx, "opencode", "AGENTS.md")`,provider 描述 :419-423),且优先级 55(:45)在用户级去重竞争中**垫底**:被 `~/.omp/agent/AGENTS.md`(100)、`~/.claude/CLAUDE.md`(80)、`~/.codex/AGENTS.md` 与 `~/.agent(s)/AGENTS.md`(70)依次遮蔽(完整发现顺序与去重机制见 §3.6)。即:文件不是死链,但对装了 Claude Code 等工具的用户会被静默遮蔽,且非 omp 原生挂载点。

> **2026-08-24 状态**:GAP-F4/G13 改指向已落地(服务端经 omp-host `GET /agent-dir` 每请求解析,profile-scoped),编辑器取值改**文件权威、副本仅种子**(`ui/lib/behaviorPrompt.ts`);`optimizeSystemPrompt` 死开关整链删除。增量与尾款记录见 07 §5.13。本节保留为历史取证。

### 2.4 两个待退役键的现状接线

- **permissionAutoAccept**:OC settings.json 键(server/lib/permission-auto-accept/runtime.js:1 `SETTINGS_KEY = 'permissionAutoAccept'`),按 session 布尔策略 + revision(settings-helpers.js:216-231 sanitize);scheduled tasks 携带同名字段(project-config.js:217、242)并在执行前 enroll(scheduled-tasks/runtime.js:561-563)。整条链挂在 OpenCode permission 协议上——omp-host 对 `/permission` 全部回答空(endpoints.js:335-344),该键只能空转。
- **planModeExperimentalEnabled**:env flag(`OPENCODE_EXPERIMENTAL_PLAN_MODE`/`OPENCODE_EXPERIMENTAL`,server/index.js:219-221)→ 系统状态 payload(index.js:1556)→ 三端读取(App.tsx:410-416、MobileApp.tsx:1076-1081、VSCodeApp.tsx:77-83)→ `useFeatureFlagsStore.setPlanModeEnabled`。OC 的 plan 面靠扫描合成文本标记识别(hooks/usePlanDetection),与 omp `plan.enabled`/plan mode 是两套体系(详见 02 章)。

### 2.5 设置页导航现状

`SettingsView` 页序(ui/components/views/SettingsView.tsx:89-118):general 组(general/appearance/chat/notifications/sessions/shortcuts/voice/integrations/usage/about)+ projects 组 + **opencode 组(providers/agents/behavior/commands/mcp/plugins)** + content 组。opencode 组的命名与页面内容(BehaviorPage、/config PATCH custom agents)是 OpenCode 残留语境,07 章清理命名,本章迁移 behavior/defaults 的内容归属。

### 2.6 omp-host 侧现状:Settings 实例是被动的

- engine.js 不直接初始化 Settings;`createAgentSession` 取 Settings 的顺序是 `options.settings ?? options.settingsManager ?? Settings.init({ cwd, agentDir })`(`<s>`/sdk.ts:1273-1275;注入口类型定义 `settings?: Settings` / `settingsManager?: Settings | Promise<Settings>` 在 sdk.ts:554-560)。omp-host 现两者都不传 → 走默认 init。
- **关键事实(R6 依据)**:`Settings.init` 是进程级单例(`globalInstancePromise` 守卫,`<s>`/config/settings.ts:404-425;**后续** `init({cwd: X})` 直接返回首个 promise,:405 —— 后到的 cwd 参数被静默忽略)——**首次** session 创建把整个 omp-host 进程的 Settings 绑定到那个 cwd;惰性代理 `settings` 永远转发到 globalInstance(settings.ts:2441-2460)。多目录并发时,后续目录的 `.omp/config.yml` 项目层对该进程内会话不生效。`reloadForCwd`(settings.ts:639-652)可原地重设作用域,但其文档注释自证危险:"mutates the live instance, so **every holder**(the settings proxy, the active session, controllers)observes the new project scope"(settings.ts:633-638)——前台切换调用它会重写**所有后台会话**的项目层,这正是总纲 R6 明令禁止的操作。
- sdk.ts:554-560 的 `options.settings` 注入口表明"每会话注入实例"在 SDK 层面已有缝;**修订轮 2(R2 评审 H3)裁决:该注入口从 OQ-F2 升格为 P0 采纳**——`createAgentSession` 取 Settings 的顺序即 `options.settings ?? options.settingsManager ?? Settings.init({ cwd, agentDir })`(sdk.ts:1273-1275);§5.1 REVISED R2 以「每目录 keyed Settings 实例注入」取代「单例消费 + loadReadOnly 旁路」拓扑。`loadIsolated` 是全量 `#load()`(agent.db/迁移/标记文件,settings.ts:439-442)、`cloneForCwd` 共享 storage/configPath 且按新 cwd 装载项目层(:607-625)——派生原语取舍与多实例残留风险显式登记于 §5.1.7 与 OQ-F2(上游确认项收窄,不再阻塞)。
- omp-host 现无任何设置读写端点;wire `GET /config` 返回 configPayload(default agent/model 等 OpenCode 形状),`PATCH /config` 只收 agents(endpoints.js:347-356)。

---

## 3. 目标语义(omp/TUI 侧)

### 3.1 一个 schema 就是整个设置产品

- `SETTINGS_SCHEMA`(`<s>`/config/settings-schema.ts:391):约 360 个键定义(grep `^\t\ttype:` 计 359),每个键 `type` + `default` + 可选 `ui{tab,group,label,description,options,condition,secret,ordered}`。334 个键带 `ui:`(进 TUI 设置面板),38 个带 `condition`,2 个 `options:"runtime"`,7 个 `credential:true`。
- 面板结构常量:`SETTING_TABS`(10 tab:appearance/model/interaction/context/memory/files/shell/tools/tasks/providers,:95-106)、`TAB_METADATA`(:109-120)、`TAB_GROUPS`(组序,:127-160)。
- TUI 入口:`/settings` → `SettingsSelectorComponent`(全屏 tab 编辑器,`<s>`/modes/controllers/selector-controller.ts:182-196);可见性条件函数表 `CONDITIONS`(`<s>`/modes/components/settings-defs.ts:96-147:hasImageProtocol/advisorEnabled/hindsightActive/mnemopiActive/autolearnActive/autoThinkingActive/usageAwareFallbackEnabled/planModeEnabled——**除 hasImageProtocol 外全部是纯 Settings 读**,web 侧可服务端求值)。
- TUI 写入 = `settings.set(path, value)` 同步生效(settings-selector.ts:904-910、1219-1247),无"保存按钮"。

### 3.2 存储与分层(权威语义)

- 配置根:`~/.omp`(`PI_CONFIG_DIR` 可改);主文件 `~/.omp/agent/config.yml`(`config.yaml` 只读兜底,`MAIN_CONFIG_FILENAMES`,pi-utils/dirs.ts:22、26)。YAML 是唯一活格式;legacy settings.json/agent.db 自动迁入并 `.bak`(`#migrateFromLegacy` settings.ts:1298-1333;约 60 个 schema 迁移 `#migrateRawSettings` :1336-1925)。
- 项目层:`<cwd>/.omp/config.yml`,**仅 `modelRoles` 子树是原生权威写面**(`#loadProjectSettings` :1232-1256:capability 发现的项目设置项参与读合并,:1236-1247;原生文件只取 modelRoles,:1248-1254;`#saveProjectNow` :2159-2192 只写 modelRoles 键)。`modelRoleStorage: enum global|project, default global`(settings-schema.ts:537-559)决定 /models 角色分配落到哪层。
- 合成优先级(`#rebuildMerged` :2213-2218):`schema defaults < global config.yml < 项目层 < --config overlay < 内存 overrides`。`get()` 同步返回合成值(:477-487,路径作用域数组额外按 cwd 解析);`isConfigured(path)` 区分"显式配置"与"默认值"(:493-495)。
- **没有 defaultModel/defaultProvider 键**:默认模型 = `modelRoles.default`(`modelRoles: {type:"record", default:{}}`,schema:561),由 `createAgentSession` 解析(sdk.ts:1275 → 默认角色 → catalog 兜底)。10 个角色与别名见 01 章。

### 3.3 写路径的持久化语义(并发设计的根据)

- `set()` → `#queueSave()` **100ms debounce** 链式后台保存(settings.ts:502-517、2027-2046);`flush()` 立即落盘(:584-587);`cancelPendingSaves()` 弃写(:572-575)。
- `#saveNow()`(:2049-2138)在**文件锁内重读 config.yml** 再只应用本次修改的键(:2061-2113)——即跨进程(或跨实例)**按键合并、不相干键互不覆盖**;modelRoles 按角色细粒度合并(:2076-2109)。写失败把修改路径放回重试队列(:2124-2134)。
- 原子写:`#writeYamlAtomically` 临时文件 + rename(:1961-1984);跨进程串行 `#withYamlWriteLock`(:1145-1148)。
- 损坏防护:启动/写锁内发现 invalid YAML → 隔离为 `<file>.broken-<ts>-<pid>-<uuid>` 并**拒绝覆盖**(`#quarantineInvalidYamlLocked` :1183-1200,`#loadYamlIfPresentForWriteLocked` :1165-1181)。
- modelRoles 专用 API:`setModelRole`(:892-920,写 global 层)、`setProjectModelRole`/`clearProjectModelRole`(:931-944,写 `.omp/config.yml`)、`getModelRole`/`getGlobalModelRole`/`getProjectModelRole`(:948-967)、`getModelRoleProvenance`/`getModelRoleSource`(:980-997)。

### 3.4 凭据与脱敏

- `credential: true` 顶层标记(schema:244-254,"lives at the top level rather than inside ui so it can also describe a setting the settings panel never shows");`isCredential(path)` 是唯一判定入口,**同时把 `ui.secret` 视为 credential**(schema:5628-5631)。
- 7 个 credential 键:`auth.broker.token`(schema:402)、`mnemopi.llmApiKey`(:2884)、`hindsight.apiToken`(:2927)、`searxng.token`(:5449)、`dev.autoqaPush.token`(:5533)等。API key 本体不在 settings——那是 auth storage 的事;schema 只嵌这少数几个服务凭据。
- TUI 面板对 credential/secret 键**掩码显示、可写不可读**。

### 3.5 SDK 对宿主的可用面(代理策略的可行性根据)

- `export { Settings, settings }`(`<s>`/index.ts:17);`settings` 是惰性代理到全局单例(:2441-2446)。
- 实例工厂:`Settings.init`(单例,:404-425)、`loadReadOnly`(不开 agent.db、不迁移、只读合成,:431-434)、`loadIsolated`(独立持久实例,:439-442)、`isolated`(纯内存,:448-456)、`cloneForCwd`/`reloadForCwd`(:607-652)。
- schema 工具全量再导出(settings.ts:56-57 `export * from "./settings-schema"`):`SETTINGS_SCHEMA`、`SETTING_TABS`、`TAB_GROUPS`、`getDefault`(:5613-5616)、`isCredential`(:5628-5631)、`getUi`(:5638-5641),以及 settings-defs.ts:14-27 所引 `getType/getEnumValues/getPathsForTab`。

> 结论:omp-host(已是 Bun 进程、已 import SDK,engine.js:14-20)**可以在进程内直接持有 Settings 实例**并复用 §3.3 的全部持久化语义,不需要自己碰 YAML。

### 3.6 上下文文件(AGENTS.md/CLAUDE.md)发现顺序与去重(R12 BehaviorPage 裁定的证据基线,2026-08-19 重新取证)

omp 的上下文文件不是单一文件而是 **provider 竞争 + 按 scope 去重**的体系(全部证据取自安装版 SDK,即 OpenChamber 实际嵌入的引擎):

**去重机制**:`contextFileCapability` 的 key 函数——用户级全部条目共享单一 key `"user"`,项目级按 `project:<depth>`(每个目录深度一个槽)(`<s>`/capability/context-file.ts:36,设计注释 :31-35 "one user-level file, and one project-level file per directory depth... higher-priority providers shadow lower-priority ones")。provider 按优先级降序参与,去重 first-wins(`<s>`/capability/index.ts:84-90 插入即按"highest first"、:183 "first wins = highest priority"、:430-431 排序)。**即:用户级全局槽只有一个文件能生效,项目层每深度一个。**

**用户级槽竞争表**(优先级高者胜):

| 优先级 | provider | 文件 | 证据 |
|---|---|---|---|
| 100 | omp 原生 builtin | `~/.omp/agent/AGENTS.md` | `<s>`/discovery/builtin.ts:910 `path.join(getAgentDir(), "AGENTS.md")`;`getAgentDir()` = `~/.omp/agent`(pi-utils `packages/utils/src/dirs.ts:495-498`);PRIORITY=100(builtin.ts:42) |
| 80 | Claude Code | `~/.claude/CLAUDE.md` | `<s>`/discovery/claude.ts:35、:127-153 |
| 70 | Agent Dirs | `~/.agent/AGENTS.md`、`~/.agents/AGENTS.md`(用户 home) | `<s>`/discovery/agents.ts:28、:290-315 |
| 70 | OpenAI Codex | `~/.codex/AGENTS.md` | `<s>`/discovery/codex.ts:44、:51-62 |
| 55 | **OpenCode 兼容** | `~/.config/opencode/AGENTS.md`(**仅用户级,无项目级**) | `<s>`/discovery/opencode.ts:45、:107-123、:419-423;路径表 helpers.ts:55-58(`userBase: ".config/opencode"`) |

**项目级槽(每深度)**:`<dir>/.omp/AGENTS.md`(100,builtin.ts:921-935 最近 `.omp` 配置目录)> `<dir>/.claude/CLAUDE.md`(80)> `<dir>/.agent(s)/AGENTS.md`(70)> **裸 `<dir>/AGENTS.md`(10)**。裸文件由独立 provider 从 cwd 向上走查发现(含 monorepo 祖先;跳过 home 目录自身的 AGENTS.md),`<s>`/discovery/agents-md.ts:42-71、:101-105——优先级最低,同深度存在任一配置目录内文件即被遮蔽。

**消费侧**:存活的 context files 由系统提示词模板注入(`prompts/system/project-prompt.md:27-29`:"Context files above auto-loaded. NEVER grep... relevant files already in context")。

> **对本章的推论(R12 BehaviorPage 裁定,§5.8 落地)**:①用户级全局指令的 omp 原生挂载点是 `~/.omp/agent/AGENTS.md`(优先级 100,写它即赢得唯一用户槽);`~/.config/opencode/AGENTS.md` 虽仍被兼容 provider 读取,但被所有更高优先级用户级文件静默遮蔽,不可作为产品面目标。②项目级写裸 `AGENTS.md` 可行且与其他工具互操作,但同深度存在 `.omp/AGENTS.md`/`.claude/CLAUDE.md`/`.agent/AGENTS.md` 时会被遮蔽——UI 必须做遮蔽检测并提示,而非静默写一个不生效的文件。③新建 `~/.omp/agent/AGENTS.md` 会**接管**用户槽:若用户已有 `~/.claude/CLAUDE.md` 等,此前生效的用户级内容将换成新文件——迁移必须检测并存用户级候选并让用户选择合并策略(§5.8)。

---

## 4. 差距清单

| 编号 | 差距 | 分类 | 优先级 | 风险 |
|---|---|---|---|---|
| GAP-F1 | 无 `/api/omp/settings` 代理端点:web 端无法读写 omp schema/config.yml,UI 只能编辑 OC 平行宇宙 | 建 | P0 | 中(端点形状需稳定;schema 漂移) |
| GAP-F2 | OC `defaultModel/defaultVariant/defaultAgent` 键与 UI 级联(useConfigStore.ts:282-396)覆盖 omp `modelRoles.default` 真值 | 删+改 | P0 | 高(牵动每 prompt 强制 model 参数、会话徽章;与 01 章联动) |
| GAP-F3 | `DefaultsSettings` 页是"默认模型/agent"双选择器,omp 语义应为"模型角色"编辑面 | 改 | P0 | 中 |
| GAP-F4 | BehaviorPage 写 `~/.config/opencode/AGENTS.md`(BehaviorPage.tsx:33)——兼容 provider 仅用户级且优先级垫底,被更高优先级用户级文件静默遮蔽,非 omp 原生挂载点(REVISED,§2.3/§3.6)。**2026-08-24:已落地**——改指向 omp 原生用户级文件(服务端每请求解析),取值文件权威;详见 07 §5.13 增量 | 删+改 | P1(已完成) | 中(用户存量内容迁移;用户槽接管检测) |
| GAP-F5 | 项目层写面缺失:`.omp/config.yml` 的 modelRoles 读写(`setProjectModelRole`/`modelRoleStorage`)无 web 入口 | 建 | P1 | 中(多目录语义) |
| GAP-F6 | 并发一致性缺失:TUI 与 web 同时编辑同一 config.yml,web 端无 file watch、无变更事件,页面会显示陈旧值。**扩围(2026-08-24 复核)**:`/api/behavior/agents-md` 编辑面同类缺口——页面打开期间 AGENTS.md 被 TUI/外部编辑器修改不热更(重开页面才见),双开页面文件级 LWW 后写胜;并入本 GAP 一并解决 | 建 | P1 | 中 |
| GAP-F7 | credential 键屏蔽:代理端点若照搬 `get()` 会把 7 个 credential/secret 值回显到浏览器 | 建 | P0(安全) | 高(凭据泄漏) |
| GAP-F8 | OC `permissionAutoAccept` 键整链(settings 键 + project-config 字段 + scheduled-tasks enroll)挂在已死的 permission 协议上。**清理分阶段(R12/R10)**:P0 审批桥原子落地为锚点 → P1 消费者切到 omp 审批(tools.approvalMode;无人值守任务 fail-closed,不改全局设置)→ P3 观察期后删键整链 | 删 | P0 锚点 / P1 消费者 / P3 删除 | 低 |
| GAP-F9 | `planModeExperimentalEnabled` env flag 与 omp `plan.enabled` 键双轨。**退役时点(R12)**:02 章模式端点上线即停产停读(flag 生产与消费同刻停止);env/i18n 文案清扫 P3 随 07 | 删 | 停用随 02 上线 / 清扫 P3 | 低 |
| GAP-F10 | omp-host Settings 进程单例被首会话 cwd 固化(sdk.ts:1273-1275 默认路径 + settings.ts:404-425),多目录会话不消费本目录项目层。**R2 修订(评审 H3)**:`createAgentSession` 已有 `options.settings` 注入口(sdk.ts:1273-1275)——改为每目录 keyed 实例注入(`#materialize` 注入 `#settingsFor(dir)`,`cloneForCwd` 派生,settings.ts:607-652),该目录会话消费「全局层 + 本目录项目层」;全局写仍唯一走 boot 实例;项目层可写仅 modelRoles 不变(§5.1 REVISED R2) | 改 | P0(代理面前置) | 中:实例数/内存、全局写跨实例传播缺口、hook 多实例语义显式登记(§5.1.7);上游确认 OQ-F2 |
| GAP-F11 | OC-native 键(theme/终端/通知/projects/tunnels/stt 等)需明确保留边界,防止"大扫除"误伤原创面 | 留 | P1 | 低 |
| GAP-F12 | omp tab 暴露范围未定:appearance/statusLine/tui.* 等终端渲染键对 web 无意义甚至误导 | 改(范围策略) | P1 | 低 |

---

## 5. 设计方案

### 5.0 决策一:代理 omp schema,还是保留 OC 平行存储?(推荐:代理)

**备选 A(推荐)——进程内 SDK 代理**:omp-host 在引擎进程内持有/派生 Settings 实例,新端点组把 schema + 当前值 + 写操作直通 SDK(`get/set/setModelRole/...`)。web UI 不存任何 omp 键副本。

- 依据:①§3.5 SDK 已导出全部所需面;②§3.3 的按键合并/文件锁/隔离/debounce 语义是 omp 产品行为的一部分,重实现必然走样(裁决原则:TUI 为准);③单一真相消灭"改了设置 TUI 不认"这类永久性 bug 类别;④schema 是活的(约 60 个迁移、每版新增键),平行存储会持续腐烂。
- 代价:端点形状受 schema 演进影响(用 §5.2 的"def 随响应下发"吸收——UI 不内置键清单);多目录实例拓扑(GAP-F10,§5.1 REVISED R2)取代旧"进程内单例约束"。

**备选 B——OC settings.json 镜像 + 导出同步**:OC 存自己的副本,定期/手动导出到 config.yml。否决:双向同步的冲突语义没有正确答案;违背 D2(状态权威);occam 层面等于两个产品。

**备选 C——起独立 `omp config` CLI 子进程读写**:否决:每次 spawn 进程做 YAML 读改写,绕过 SDK 的锁与合并;且 `config-cli.ts` 的交互面不适合服务化调用。

### 5.1 决策二:Settings 实例拓扑(REVISED R2:每目录 keyed 实例注入,取代「单例消费 + loadReadOnly 旁路」)

> **修订轮 2(R2 评审 H3,2026-08-20)**:R6 冻结裁决的前提是「SDK `Settings.init` 进程单例**不可注入前**」(00-MASTER D6-R6 原文)——该前提已被证伪:`createAgentSession` 取 Settings 的顺序为 `options.settings ?? options.settingsManager ?? Settings.init({ cwd, agentDir })`(**sdk.ts:1273-1275**;注入口类型 `settings?: Settings` / `settingsManager?: Settings | Promise<Settings>` 在 sdk.ts:554-560),每会话实例注入**无需上游改动即工作**。R2 评审 H3 指出旧拓扑(全部会话共享单例 + 项目层走 loadReadOnly 旁路展示)把非 boot 目录的项目层做成「UI 显示已生效、会话实际不消费」,直接违反 D2(权威状态)。本节改为**每目录 keyed Settings 实例注入**;R6 的四条实质全部保留——禁 reloadForCwd、全局写单一执行点、项目层可写仅 `modelRoles` 子树、前台切换不影响后台会话——被替换的只有「项目层只读用 loadReadOnly 旁路」一条(loadReadOnly 降级为无实例目录的只读兜底,§5.1.6)。总纲 D6-R6 回写登记于 §8 OQ-F1。原 reloadForCwd/active-project 设计与「短命写侧车」方案双双作废。

omp-host 进程内:

1. **boot 实例(进程单例,双重身份)**:engine boot 时显式 `await Settings.init({ cwd: <boot 目录>, agentDir })`(不再依赖首会话隐式初始化;boot 目录 = 进程首个上线的 directory,一经初始化终身不变——后续任何 `init` 被单例守卫忽略,settings.ts:404-425,本章不给任何代码路径机会去对抗它)。身份一:**全局层写的唯一执行点**(全部 `scope:"global"` 的 `set()`/`setModelRole()` 只经它 + `flush()`,杜绝全局 config.yml 第二条写路径);身份二:**boot 目录的 keyed 实例**(boot 目录会话直接复用它,不派生克隆)。
2. **每目录 keyed 实例(会话消费权威;R2 核心)**:engine 维护 `#settingsByDir: Map<规范化目录, Settings>`;`#settingsFor(dir)`:`dir === boot 目录` → boot 实例;否则(首次)`await boot.cloneForCwd(dir)` 派生并常驻 map(**settings.ts:607-625**:共享 agentDir/configPath/storage 句柄、按 dir 重载项目层 `#loadProjectSettings`(:616)、不跑 `#load` 故无迁移/标记文件副作用、不修改 boot 实例)。`#materialize`(engine.js:436;`createAgentSession` 调用 engine.js:454-473)把 `#settingsFor(directoryKey)` 作为 `options.settings` **注入**(sdk.ts:1273-1275)——**该目录的每个 AgentSession 消费「全局层 + 本目录 `.omp/config.yml` 项目层」的合成值,与 TUI 在该目录运行时的解析完全一致**。同目录全部会话共享同一实例对象:project-scope 角色写经该实例执行时,hook 在共享对象上即时触发,同目录 live 会话即刻可见(TUI 单例语义的每目录复刻)。派生备选:`Settings.loadIsolated({ cwd: dir })`(settings.ts:439-442)亦可构造独立持久实例,但其全量 `#load()`(agent.db 句柄、legacy 迁移、标记文件)在多目录场景副作用更大——v1 选 cloneForCwd,loadIsolated 留作上游确认后的备选(OQ-F2)。
3. **项目层写(任意目录;硬限制不变:仅 `modelRoles.*`)**:`scope:"project"` 的写路由到 X 的 keyed 实例(`#settingsFor(X)`,无实例则即时派生常驻——**取代旧「cloneForCwd 短命写侧车 + 用后即弃」**)执行 `setProjectModelRole`/`clearProjectModelRole`(settings.ts:931-943)+ `flush()`(:584-587)。落盘仍只 `<X>/.omp/config.yml` 的 modelRoles 子树(`#saveProjectNow` 只写 modelRoles 键,:2159-2193),经同一文件锁 + 锁内重读 + 按角色合并 + 原子写(§3.3)。
4. **全局层写(唯一执行点)**:`scope:"global"` 的任意键(含 `modelRoles.*` 的 global 落盘)一律 boot 实例执行 + `flush()`,与 `directory` 无关。守卫:非 boot 实例的代码路径禁止 `set(`/`setModelRole(` 调用(静态断言 + 测试,§7.1)。
5. **前台切换 = 无操作(不变,结构性保证更强)**:切换 active project 不触碰任何实例的 cwd;`reloadForCwd` 零引用(CI 静态守卫,§7.1)。目录间互不干扰由 map 结构保证——B 目录的项目写进 B 实例,A 实例的对象引用与内存均不受影响,不依赖约定。
6. **loadReadOnly 的降级角色**:`GET /api/omp/settings?directory=X` 优先读 X 的 keyed 实例(与该目录会话同源——"生效值"陈述为真);X 无实例(尚无会话)→ `Settings.loadReadOnly({ cwd: X })` 只读兜底(settings.ts:431-434;不触单例、不开 agent.db、不迁移),首次会话物化后切换到 keyed 实例。§5.2 的 condition 求值随所选实例。
7. **显式登记的残留 caveat(不以设计掩盖)**:
   a. **实例数与内存**:K 个目录 → K 个常驻实例(各持 merged 合成 + `structuredClone` 的全局层快照,settings.ts:615);内存 O(K × settings 规模)。目录会话全数回收后实例可驱逐(P1:map 驱逐策略,随最后会话 dispose 的显式 `#releaseSettings(dir)`)。
   b. **全局写跨实例传播缺口**:boot 实例上的全局 `set()` 只对 boot 目录 live 会话即时可见;非 boot 目录实例持有**派生时刻**的全局层快照(settings.ts:615),后续全局写不热传播到它们(与 TUI 单实例"set 即全员可见"有差异)。新派生会话取新值;live 会话的消除依赖上游 keyed instances(共享全局层,OQ-F2)。缓解:`omp.settings.updated` 照发全目录(呈现层不误导);跨目录运行时行为键(如 `tools.approvalMode`)的全局变更频率低,作为已知差异写入 §7.1.7 断言。
   c. **watcher 扇入**:§5.5 watcher 只驱动 UI revision/事件,不触碰任何实例内存(热重载不做);若未来要做实例刷新,per-instance watcher 扇入需重新设计——v1 明确不做。
   d. **hook 进程级副作用**:clone 派生与项目写会 `#fireAllHooks()`(settings.ts:623)——多实例并存时 SETTING_HOOKS 的进程级副作用(如 provider globals)为最后触发者胜,上游未担保多实例语义(OQ-F2)。
   e. **上游 keyed-instance 确认(OQ-F2 保留)**:注入工作在 SDK 既有缝上,但「多 Settings 实例并存」的支持等级、`Settings.forCwd(cwd)` 官方化、全局层共享语义需上游确认;确认前本拓扑按上述 caveat 运行,不再有「非 boot 目录会话读错项目层」类 D2 违例。

### 5.2 端点:`GET /api/omp/settings`(schema 驱动,D1 自有面)

```
GET /api/omp/settings?directory=<abs|default>&keys=<csv 可选>
```

响应(概念形状):

```jsonc
{
  "schemaVersion": "<SDK version>",          // UI 检测 schema 漂移
  "directory": "C:/proj",                    // 本响应的项目层作用域
  "agentDir": "~/.omp/agent",
  "globalConfigPath": "…/.omp/agent/config.yml",
  "projectConfigPath": "C:/proj/.omp/config.yml",
  "revision": 42,                            // 见 §5.5:watcher 维护的修订号
  "tabs": [                                   // SETTING_TABS + TAB_GROUPS + TAB_METADATA 的直通
    { "id": "model", "label": "Model", "groups": ["Thinking", "Sampling", "Prompt", …] },
    …
  ],
  "keys": {
    "compaction.strategy": {
      "type": "enum", "values": ["context-full","handoff","shake","snapcompact","off"],
      "default": "snapcompact",
      "value": "handoff",                     // get() 合成值
      "configured": true,                     // isConfigured()
      "scope": "global",                      // 可写层:global(默认);modelRoles.* 额外 "project"
      "ui": { "tab": "context", "group": "Compaction", "label": "…", "description": "…",
              "options": [{"value":"…","label":"…","description":"…"}] },
      "editable": true
    },
    "modelRoles": {                           // 特殊 record:per-role 视图
      "type": "record",
      "default": {},
      "value": { "default": "anthropic/claude-…", "smol": "…" },
      "roles": {
        "default":  { "value": "anthropic/claude-…", "source": "project", "editable": true },
        "smol":     { "value": null, "source": "default", "editable": true }
      },                                      // source = getModelRoleSource(): project|global|default
      "modelRoleStorage": "global"
    },
    "hindsight.apiToken": {
      "type": "string", "credential": true, "writeOnly": true,
      "value": null,                          // R9:credential 键(value 与 default)永不回显
      "configured": true,                     // isConfigured 仍暴露"已设置"
      "ui": { … }
    },
    "statusLine.preset": { …, "excluded": "terminal-only" }   // 见 §5.6 范围策略
  }
}
```

设计要点:

- **def 随响应下发**:`type/values/default/ui` 直接序列化自 `SETTINGS_SCHEMA` + `getUi/getType/getEnumValues`。UI 不 hardcode 键清单 → schema 加键零成本。
- **condition 服务端求值**:omp 的 38 个 `ui.condition` 是 TUI 本地函数(settings-defs.ts:96-147)。端点侧实现同名求值器:除 `hasImageProtocol`(终端能力,web 恒 `false` → 该键标记 `excluded:"terminal-capability"`)外,全部为 `Settings.instance.get(<键>)` 纯读,可在宿主内对该目录 keyed 实例求值(§5.1.6;无实例目录用 loadReadOnly 兜底)。求值失败 → 显示但不隐藏(保守)。
- **`options:"runtime"` 的 2 个键**(theme.dark/light 等):TUI 由 theme registry 填充。web 端 v1 标记 `options:"runtime-unresolved"` 并按文本输入渲染(或直接 excluded,见 §5.6——theme 本就属 TUI 专属面)。
- **provenance 仅在 SDK 暴露处提供**:通用键只有 value/configured/default(modelRoles 有 per-role source,§3.3)。不为通用键发明层探测(避免读多层 YAML 的第二实现)。
- **R9 脱敏是一切出口的统一规则**:判定入口唯一 = `isCredential(path)`(含 `ui.secret`,schema:5628-5631)。GET 的 `value:null` 只是其一;同一判定必须应用于**所有**出口——PUT 响应(§5.3 规则 4/5)、错误体(400 rejected 只含 key+reason;409/500 不含值)、omp-host 日志(只打键名,同 OC 既有纪律 settings-runtime.js:824-826)、`omp.settings.updated` payload(§5.4 只含键名)、bun:test 快照与 fixtures(§7.1 泄漏门禁)。任何出口出现 credential 键明文值即缺陷。
- **capabilities 门控(R2)**:本端点组在 `GET /api/omp/capabilities` 声明 `settings.v1`(端点组版本 + schema 版本);每目录项目层语义(会话消费 + project-scope 写)另声明 `settings.projectScopes.v1`(§1.3,跨章消费方按此门控)。UI 以 capabilities 决定是否露出 omp 设置页,不读本地 flag。降级矩阵见 §6.2。

### 5.3 端点:`PUT /api/omp/settings`(REVISED:R9 响应脱敏 + R2 写路由)

```
PUT /api/omp/settings
{
  "directory": "C:/proj",              // 项目层写时的目标目录;缺省 = boot 实例 cwd
  "changes": {
    "compaction.strategy": "handoff",
    "todo.reminders": true,
    "modelRoles.default": "anthropic/claude-sonnet-4",   // 特殊语法,见下
    "modelRoles.smol": null                               // null = 清除该角色分配
  },
  "scope": "global"                    // 'global'(默认)| 'project'
}
```

处理规则:

1. **键校验**:`changes` 的每个键必须存在于 `SETTINGS_SCHEMA`;类型按 def 校验(enum 值域/boolean/number/string/array/record)。未知键 → `400 { error, rejected:[{key, reason:"unknown"}] }`(防御 UI 与宿主 SDK 版本漂移;rejected 条目只含键名与原因,**不含提交值**)。
2. **写路由(对齐 §5.1 R2 拓扑)**:`scope:"global"` 的全部键(含 `modelRoles.*` 的 global 落盘)→ **boot 实例(全局写唯一执行点)** `set(key, value)` / `setModelRole(role, value ?? undefined)`(settings.ts:502-517、:892-920),无论 `directory` 为何。`scope:"project"` 时只接受 `modelRoles.*` 键(其余键 400,错误信息解释"omp 项目层只权威承载 modelRoles 子树"),一律路由到 **X 的 keyed 实例**(`#settingsFor(X)`,无实例则即时派生常驻,§5.1.2-3;X === boot 目录即 boot 实例)执行 `setProjectModelRole(role, value)` / null → `clearProjectModelRole(role)`(settings.ts:931-943)+ `flush()`——写入落在该目录会话共享的同一实例上,hook 即时触达 live 会话。
3. **通用键**:`set` 即时更新内存并触发 SETTING_HOOKS(settings.ts:512)——宿主内若 hook 有进程级副作用(如 provider globals)自动生效,与 TUI 一致。
4. **credential 键(R9)**:接受写入(write-only);`changes` 里的空串/null 语义照 TUI 文本编辑器——空串清除设置(settings-selector.ts:1142-1144 的既有约定)。**响应契约:credential 键在 `applied` 中固定为 `{ "configured": true|false }`,绝不回显写入值**(清除后为 `false`)。
5. **落盘确认**:handler `await instance.flush()`(settings.ts:584-587)把 debounce(100ms)收拢后再返回,响应:

```jsonc
// 普通 PUT
{ "revision": 43, "applied": { "compaction.strategy": "handoff" },
  "persisted": true, "quarantined": null }
// 含 credential 键的 PUT(R9:值不回流,只报 configured)
{ "revision": 44, "applied": { "hindsight.apiToken": { "configured": true } },
  "persisted": true, "quarantined": null }
```

6. **错误语义(R9 同样适用)**:YAML invalid 触发 SDK 隔离(§3.3)时,首次读会 throw "moved to <backupPath>"——端点转译为 `409 { error:"config-quarantined", quarantinedTo }`(仅路径,无值),UI 提示用户找回 `.broken-*` 文件。写失败(重试队列语义,§3.3)→ `500` + 建议重试(不含提交值);幂等性:重放同一 PUT 得相同终态(set 语义天然幂等)。
7. **并发防护**:目录级写互斥(端点 handler 内 per-directory promise 链,同 OC `persistSettingsLock` 的既有模式,settings-runtime.js:822-897)。v1 不做 revision CAS——omp 自身的按键合并已是并发模型,加 CAS 反而制造 TUI/web 不对称(开放问题 OQ-F4)。
8. **日志纪律(R9)**:PUT 的访问/审计日志只记录键名与 scope/directory,不记录值——credential 键连"值长度/前后缀"都不记。

### 5.4 事件:`omp.settings.updated`(REVISED:R1 单通道,仅认领名/payload/时机)

```jsonc
// payload
{ "directory": "C:/proj", "revision": 43, "keys": ["compaction.strategy"],
  "origin": "web" | "external" }
```

- **本章只认领**:事件名(`omp.settings.updated`,命名遵守 `omp.<域>.<事件>` 规约)、payload 形状、触发时机。**通道、envelope、事件 ID、durable/volatile、directory 作用域、`Last-Event-ID` 重放、reducer 一律以 05 章唯一事件注册表与唯一 `OmpEventBus → /api/omp/events` SSE 通道为准**(D6-R1);本章**不定义任何自有通道、broadcaster 或并行命名**,05 章注册表应含本事件行(SDK source:无——非 AgentSession 事件;producer:omp-host 设置端点/watcher;作用域:directory)。
- `origin:"web"`:本宿主 PUT 成功后发出(UI 乐观更新对账,见 §5.7)。
- `origin:"external"`:watcher 检测到外部修改(TUI/手编)。
- **R9**:payload 的 `keys` 只含键名,永不含值(含 credential 键时同样仅键名 + 它是否 configured 可由 GET 查询)。
- settings 域的引擎(AgentSession)事件数为 0——设置变更不是 AgentSession 事件,这本身即 D2 显式处置说明(已登记 05 章处置表)。

### 5.5 并发一致性语义(GAP-F6,必须显式)

**同一进程内**:所有 PUT 串行(per-directory 链)→ 无竞争。

**跨进程(TUI ⇄ web,或两个 web 实例)**:

1. **写-写**:双方都经 omp 的文件锁 + `#saveNow` 键级重读合并(§3.3)→ **不相干键互不丢失;同一键最后写者胜**。这是 omp 的既定语义(TUI 多实例同理),web 不加码。
2. **读-写一致性**:web 端显示可能陈旧,直到:
   - 自己 PUT(响应即新值),或
   - 收到 `omp.settings.updated`(external)。
3. **watcher 实现**:omp-host 进程内 `fs.watch` 两个路径(全局 config.yml + 每个已知目录的 `.omp/config.yml`;R4——watcher 与端点同进程,web server 不参与;Windows 用 `fs.watch` + debounce 250ms,或 polling 兜底——OC 的 terminal/notifications 模块已有同类实践)。变更判定 = mtime + 内容 hash(忽略纯属 mtime 抖动);命中 → `Settings.loadReadOnly({cwd})` 重读(只读快照,**不触碰任何 keyed 实例内存**,§5.1.7c)→ diff 出变化键 → 修订号 +1 → 发 external 事件。**自己触发的写**通过"写前登记 pending hash"抑制回环。**R9**:diff 结果、watcher 日志、事件 payload 一律只含键名——即使变更的是 credential 键,内容 hash 与 diff 也只产生键名,不落值。
4. **revision 语义**:宿主内单调计数,仅用于 UI 陈旧性检测与日志,**不用于 CAS**。
5. **UI 端**(sync-state-invariants 纪律):服务器响应/事件是权威;本地编辑态乐观显示;external 事件到达时**仅重写用户未在编辑的键**(正在聚焦的控件不打断,blur 时若权威值≠本地则标脏并提示)。

### 5.6 暴露范围策略(GAP-F12):首批 / 永不

**原则**:omp 键若驱动的是 omp 引擎行为(每个会话都消费)→ 应露出;若驱动的是 TUI 渲染(终端 chrome)→ 永不(OpenChamber 有自己的 web 外观体系,属 OC-native 键)。

**首批(P0/P1 落地)**:

| OC 设置页(新) | omp 键(示例,非穷举) | 备注 |
|---|---|---|
| 模型与思考 | `modelRoles.*`(10 角色编辑器,per-role source 徽章)、`modelRoleStorage`、`defaultThinkingLevel`、`thinkingBudgets.*`、`retry.enabled/maxRetries/modelFallback`、`cycleOrder` | 编辑器复用 01 章 roles hub 的模型列表数据;fallbackChains v2(编辑器复杂) |
| 上下文(Compaction) | `compaction.enabled/strategy/keepRecentTokens/autoContinue/idleEnabled/idleThresholdTokens/idleTimeoutSeconds`、`display.collapseCompacted` | 与 05 章 compaction divider 事件联动 |
| 任务与待办 | `todo.enabled/reminders/remindersMax/eager`、`tasks.todoClearDelay`、`plan.enabled`、`goal.enabled/goal.statusInFooter` | plan/goal 行为归 02,此处仅开关 |
| 审批(approvals 组) | `tools.approvalMode`、`tools.approval`(per-tool allow/prompt/deny 矩阵)、`ask.enabled/ask.timeout` | 语义消费归 03;`bash.patterns` v2 |
| 通知 | `completion.notify`、`error.notify`、`ask.notify`、`recap.enabled/idleSeconds` | 这些键在 omp 控制 TUI 端 OSC 通知;对 OC 是"引擎侧通知偏好",与 OC-native 通知系统并存(见 OQ-F5) |

**永不(标记 `excluded`,GET 仍返回但 `editable:false` 或直接过滤)**:`appearance` tab 全部(theme.dark/light、symbolPreset、statusLine.*、display.shimmer/smoothStreaming、tui.*、terminal.showImages、colorBlindMode、compactThinkingLevel 等)——TUI 渲染专属,web 改了无效果且误导;`power.sleepPrevention`(macOS caffeinate);`tui.titleState/hyperlinks/tight` 等终端能力键。实现:端点维护 `EXCLUDED_PREFIXES/TABS` 表(数据,不是散落 if),首版 = `["appearance"]` tab + `tui.*`/`terminal.*`/`statusLine.*`/`display.*`(除 `display.collapseCompacted`)前缀。

### 5.7 UI 改造

- **新 store** `ui/src/stores/useOmpSettingsStore.ts`:`load(directory)`(GET)、`setKey(key, value, scope)`(乐观置值 → PUT → 响应对账,失败回滚 + toast)、订阅 05 章唯一 `/api/omp/events` 通道上的 `omp.settings.updated`(R1:不另开连接、不直连 relay 旁路)。不进 `useConfigStore`(那是 OpenCode config 域,07 章拆)。
- **SettingsView**:nav 新增 `omp` 组(置于原 opencode 组位置,组名随 07 改名),页 = §5.6 首批表;渲染完全 schema 驱动(`tabs/keys` 响应 → 既有 `SettingsSection/SettingsFieldRow/SettingsCheckboxRow` 组件族,ui/components/sections/shared/SettingsSection.tsx)。i18n:`label/description` 来自 schema(英文原文);中文标签 v1 以 schema 英文呈现 + 逐步补 `settings.omp.<key>` 词条(locale-ui-patterns 管辖,不阻塞)。
- **DefaultsSettings 重构(GAP-F3)**:
  - "默认模型 + 默认 agent + thinking variant" 三行 → 指向"模型与思考"页的角色编辑器(主入口);页内保留 OC-native 三件:`showDeletionDialog`、small model 组、walkthrough model(§5.8 处置表)。
  - `handleModelChange` 的双通道写(DefaultSettings.tsx:144-176)删除——omp 键不落 OC settings.json。
- **useConfigStore 级联退役(GAP-F2,与 01 章共谋)**:`resolveDefaultAgentModelSelection` 的 model 分支改为读 `/api/omp/settings` 的 `modelRoles.default`(经 01 章的 role→model 解析端点/数据);`settingsDefaultModel/defaultVariant` 字段与 `fetchOpenCodeConfigDefaults` 中对应分支删除(useConfigStore.ts:282-396、2076-2279)。prompt 不再强制携带 model(总纲开放问题 3 的落点,节奏由 01 定)。
- **VS Code 端**:RuntimeAPIs 的 `settings.load/save`(OC desktop settings)不经手 omp 键;VS Code webview 调 `/api/omp/settings` 走同一 HTTP 代理(扩展宿主内的 omp-host)。`useIsVSCodeRuntime` 分支仅影响 fetch 传输,不影响形状。

### 5.8 OC 存量设置键处置表(GAP-F2/F4/F8/F9/F11)

| OC 键/链路 | 处置 | 依据与去向 |
|---|---|---|
| `defaultModel` + `defaultVariant` | **删 + 只读检测/确认导入(REVISED,R12/D3)** | **禁止任何自动写入**。只读检测(settings.json 旧值 + omp `getModelRoleSource('default')`)→ 设置页横幅"检测到旧默认模型,导入为 default 角色?"→ 用户显式确认后 `setModelRole('default', <model>)` 一次写入;`modelRoles.default` 已显式配置时**永不覆盖**,横幅仅并列展示两处来源;OC 原值原地保留(回滚锚点),导入动作写审计日志(时间/原值/目标 role;流程见 §6.2 阶段 2)。variant 无 omp 对应物,不迁移(01 章 GAP-11 同裁决),提示用户改设 `defaultThinkingLevel`。键从 DesktopSettings/sanitize/DefaultsSettings 移除(desktop.ts:125-126) |
| `defaultAgent` | **删,不迁移默认值** | build/plan 二分亡(07/02);custom-agent 场景 = 会话内 agent 选择(ch02);曾设值若对应 custom agent 名,迁移为"无全局默认"并在 UI 提示 |
| `smallModelUseDefault/smallModelOverride` | **留(OC-native)** | 服务 OC 原创面(summarization/walkthrough,server/lib/small-model/);不映射 `providers.tinyModel`(后者驱动 omp 引擎内标题/commit 生成,语义不同) |
| `walkthroughModelOverride` | **留(OC-native)** | OC walkthrough 专属(desktop.ts:135-138 注释) |
| `permissionAutoAccept`(全局键) | **删,分阶段(REVISED,R12:桥 P0 → 消费者 P1 → 删除 P3)** | P0 = 03 章审批桥原子落地(锚点,不动本键);P1 = 消费者切换(通知/待决状态/WorkStatus 改读 omp 审批面;sanitize 链 settings-helpers.js:216-231 与 permission-auto-accept/runtime.js:1 停止参与行为,键保留只读);P3 = 07 章观察期后随 permission 协议整链删除 + 清理器摘键 |
| project-config `permissionAutoAccept` 字段 + scheduled-tasks enroll | **删,同上分阶段** | project-config.js:217/242、scheduled-tasks/runtime.js:561-563;P1 起无人值守任务改走 omp 审批语义且 **fail-closed**(R10:桥不可用即任务失败,不改全局审批设置);替代 = omp `tools.approvalMode`(任务模板落 omp 配置或任务参数,03 章定);字段 P3 删除 |
| `planModeExperimentalEnabled`(env) | **删,停产与清扫分离(R12)** | 02 章模式端点上线**即刻**停产停读:env 生产(server/index.js:219-221)与三端消费(App.tsx:410-416、MobileApp.tsx:1076-1081、VSCodeApp.tsx:77-83)同刻停止,改读 omp `plan.enabled`;合成文本 plan 检测(hooks/usePlanDetection)同刻退役;env/i18n 文案清扫 P3 随 07 |
| `globalBehaviorPrompt`(BehaviorPage 主 textarea) | **改挂载点(REVISED,裁定见 §3.6;2026-08-24 已落地)** | 全局层目标 = omp 原生用户级 `~/.omp/agent/AGENTS.md`(优先级 100,赢得唯一用户槽;§3.6 证据链);迁移 = 只读检测 + 用户确认 + 永不覆盖既有文件(与 defaultModel 同纪律),并存用户级候选(`~/.claude/CLAUDE.md` 等)时提供合并/替换选择(新文件会接管用户槽,§3.6 推论③);旧路径 `~/.config/opencode/AGENTS.md` 原地保留(兼容 provider 仍读),仅停写。项目级规则不属于本键:项目 `AGENTS.md` 由项目侧裸文件承载,UI 做同深度遮蔽检测(§3.6 推论②)。**落地语义**:文件存在即权威(含空文件),本键降级为"文件从未创建时"的编辑器种子,保存仍双写(文件+副本);契约 `ui/lib/behaviorPrompt.ts` |
| `responseStyle*` 三键 | **留(OC-native)** | OC 原创、首消息注入实现,与引擎设置无关(BehaviorPage.tsx:44-79)。**2026-08-24 复核残留(minor,P3 文案级)**:enabled + custom 预设 + 空自定义文本时静默无效果(`buildResponseStyleInstruction` 返 null),UI 无提示 |
| `notifyOn*` + `notificationTemplates` | **留(OC-native)** | OC 通知系统是原创面(08);与 omp `completion.notify/error.notify/ask.notify` 的关系 = 两层(OC 键控 web/desktop 通知,omp 键控 TUI),UI 需文案区分(OQ-F5) |
| theme/字体/终端/布局/`messageStreamTransport`/`stt*`/projects/tunnels/git 凭据等其余 DesktopSettings 键 | **留(OC-native)** | OC 外观与基础设施;`/api/config/settings` 端点保留但**只**服务这些键 |

---

## 6. 迁移与兼容

### 6.1 omp 侧:零迁移

omp 拥有并持续维护自己的文件:legacy `settings.json`/agent.db → config.yml 自动迁移(settings.ts:1298-1333)、约 60 个 schema 迁移在每次加载时透明执行(:1336 起)。OpenChamber 不触碰这些文件的历史形态,不写 `.bak`,不复制。

### 6.2 OpenChamber 侧:字段退役计划(阶段化)

- **阶段 0(开关,REVISED R2)**:不再使用本地 env flag(原 `OMPCHAMBER_OMP_SETTINGS=proxy|legacy` 设计废弃)。门控 = `GET /api/omp/capabilities` 的 `settings.v1`:新 UI 见 capability 缺失(旧 engine)→ 整个 omp 设置页不露出、级联走旧路(降级不报错);旧 UI + 新 engine → 旧 UI 不调用新端点,零影响;relay 旧 bundle → 由 capabilities 声明的最低 UI 版本拦下或降级(R2 三矩阵,矩阵归总纲/05)。
- **阶段 1(P0)**:GAP-F1 端点 + F7 屏蔽 + F10 实例拓扑(§5.1 REVISED R2:每目录 keyed 实例注入)落地;`DefaultsSettings` 读侧切到 omp 面。
- **阶段 2(P0,与 01 同步,REVISED R12 —— 原自动写入设计废弃)**:defaultModel 存量只走"检测 + 确认导入":
  1. **只读检测**(无写入):读 OC settings.json 的 `defaultModel/defaultVariant` 与 omp 侧 `getModelRoleSource('default')`(经 §5.2 GET);
  2. **用户显式确认**:设置页横幅"检测到 OpenChamber 旧默认模型 X → 导入为 omp default 角色?";用户点击"导入"才执行 `setModelRole('default', X)` + `flush()`(boot 实例——全局写唯一执行点,§5.1 R2);**任何启动时/静默自动写入被禁止**(R12/D3:自动写会改变 omp TUI 行为与回滚基线);
  3. **永不覆盖**:`modelRoles.default` 已显式配置(source ∈ {global, project})时,横幅只并列展示两处来源与"当前生效值",不提供一键覆盖(用户仍可在角色编辑器手动改——那是显式编辑,不是迁移覆盖);
  4. **保留原值 + 审计**:OC settings.json 的 `defaultModel/defaultVariant` 原地保留不删(回滚锚点);导入成功写审计记录(时间、OC 原值、写入 role 与值——model id 非 credential,可记录;用户拒绝导入也记录一次,避免重复打扰);
  5. OC 键随后改为只读展示一个版本周期(标注"已由模型角色取代"),再从 DesktopSettings、`sanitizeSettingsUpdate`、`formatSettingsResponse`、i18n 与 DefaultsSettings 删除(clean cutover,不留 alias)。
- **阶段 3(P1/P3)**:P1 = BehaviorPage 切挂载点(§5.8 裁定)+ `permissionAutoAccept`/plan flag 消费者切换(GAP-F8/F9);P3 = 07 章大扫除时删 OC 键/字段/整链(permissionAutoAccept 随清理器主动摘除)。
- **回滚(REVISED)**:无 flag 可拨——回滚 = 发布维度(engine/UI 回退到旧版本)。过渡期物理隔离保证可回退:omp 键全部在 config.yml,OC 键原地保留;导入只在用户确认后发生且留有审计,回滚 = 级联回 OC 原值。阶段 2 第 5 步删键后回滚 = 重装前版本。
- **存量数据**:OC settings.json 中被删键不清盘删除(避免破坏用户备份预期),仅停止读写;`permissionAutoAccept` 例外——P3 随 03 的清理器主动摘除。

### 6.3 并发会话兼容

- 用户确认后的导入写入 `modelRoles.default` 走 boot 实例 `setModelRole('default', …)` + `flush()`(§5.1 R2:全局写唯一执行点),与任何正在运行的 TUI 按键合并(§5.5),不整文件覆盖;检测阶段零写入。
- 已开 会话的模型不追改(omp 会话模型在创建时解析,engine.js:443-450 的既有行为);新会话生效,与 TUI 改角色后开新会话的行为一致。

---

## 7. 验证方案(设计,非执行)

### 7.1 omp-host 单元/集成(bun:test,server JS `node --check`)

1. **GET 形状**:mock agentDir/cwd → 键数 ≥ schema 键数 − excluded;`tabs` 与 SETTING_TABS 同构;`configured` 对默认值键为 false。
2. **credential 屏蔽 + 全出口泄漏门禁(R9,安全门禁)**:预置 `hindsight.apiToken` 值 → GET 响应中该键 `value` 为 null、`configured:true`;随后对**每一类出口**断言明文不出现:GET/PUT 成功响应 JSON 串、PUT credential 键的 `applied` 值恰为 `{configured:true}`、400/409/500 错误体、omp-host 日志捕获、`omp.settings.updated` payload、bun:test 快照/fixtures——一律 grep 明文 token 归零。
3. **PUT 校验**:未知键 400(rejected 只含键名);enum 越值 400;`scope:"project"` 非 modelRoles 键 400(错误信息含"modelRoles"字样);`modelRoles.smol:null` + project → `clearProjectModelRole` 路径(断言 `.omp/config.yml` 中键消失)。
4. **写路由(R2)**:`scope:"global"` + `directory:B`(≠ boot 目录 A)→ 断言 **boot 实例**执行 set、写盘仍是 `~/.omp/agent/config.yml`(不因前台目录改道);`scope:"project"` + directory=B → B 的 keyed 实例路径(`#settingsFor(B)`,按需派生常驻),断言 `<B>/.omp/config.yml` 落盘且**只含 modelRoles 变更**;非 boot 实例代码路径 `set(`/`setModelRole(` 零调用(静态断言)。
5. **注入拓扑(R2 必测)**:`#materialize` 传给 `createAgentSession` 的 `options.settings` === `#settingsFor(directory)`(断言注入发生,SDK 不再走默认 `Settings.init`,sdk.ts:1273-1275);同目录两次物化共享**同一实例引用**;boot 目录会话拿到 boot 实例引用;非 boot 目录派生后 `#settingsByDir` 命中不再重复 cloneForCwd。
6. **静态守卫(R6/R2)**:omp-host 源码 `grep reloadForCwd` 零引用(CI 门禁;前台切换代码路径不存在);`Settings.init(` 仅出现在 boot 路径(单处)。
7. **A/B 双目录并发 + 会话级消费(R2 必测,取代旧 no-drift 口径)**:boot 实例 init 于 A;并发 PUT(A, project, modelRoles.default=X) 与 PUT(B, project, modelRoles.default=Y) → `A/.omp/config.yml` 与 `B/.omp/config.yml` 各自正确、互不污染;**随后在 A、B 各物化一个会话 → 断言各会话经注入实例解析的 default 角色 = 本目录项目值(session.model / `getModelRole('default')`),而非 boot 目录 A 的值——project 层被会话真实消费,不只 UI 展示**;GET(A) 与 GET(B) 的项目层视图不同且各自正确(keyed 实例;无会话目录走 loadReadOnly 兜底)。
8. **切前台零漂移(R6 必测)**:模拟 S_A 会话运行于 A(经注入的 A 实例读取 `tools.approvalMode`、`compaction.*`、`getModelRole('default')` 各记录一次基线)→ 切换前台目录到 B(触发设置 UI GET(B)+ 在 B 改 B 的项目角色)→ 再次读取 S_A 三类值 → 断言与基线完全一致(A 实例引用与内存未受触碰;B 的写只进 B 实例与 B 文件);**全局键跨实例传播缺口按 §5.1.7b 断言**:在 B 触发一次 global `set()` → A 的 live 会话不热见(行为与 caveat 文档一致)、A 新派生会话/新物化实例可见。
9. **flush 语义**:PUT 返回后立即直读 config.yml 文件,断言值已落盘(debounce 已被 flush 收拢)。
10. **quarantine**:预置损坏 YAML → GET/PUT 返回 409 config-quarantined,目录中出现 `.broken-` 文件,原文件未被覆盖(对照 settings.ts:1183-1200)。
11. **watcher**:tmp 目录写 config.yml → 期待 `omp.settings.updated(external)` 且 revision +1;自写(PUT)不触发 external(回环抑制);credential 键变更的 watcher 日志只含键名。
12. **导入纪律(R12)**:预置 OC `defaultModel` + omp 角色未配置 → 仅打开设置页(不点确认)→ 断言 `modelRoles.default` **未**被写入(零自动写入);点击导入 → 写入一次且审计记录存在;omp 角色已配置时 → 确认按钮不存在/不可达(永不覆盖)。

### 7.2 E2E(dev 栈 5180/3902,浏览器驱动)

1. 打开设置 → "模型与思考"页:把 `defaultThinkingLevel` 从 high 改 minimal → 响应 200 → `read ~/.omp/agent/config.yml` 出现 `defaultThinkingLevel: minimal`(TUI 对照:settings-selector 同操作同文件同结果)。
2. **双向并发**:浏览器改 `compaction.strategy`;同时 TUI 改 `todo.reminders`;断言:web 页面在外部事件后显示 reminders 新值,config.yml 两键并存(按键合并),无一侧丢失。
3. **同键竞争**:TUI 与 web 先后改同一键 → 后写者胜且另一侧在事件后收敛。
4. **DefaultsSettings 导入(REVISED,R12)**:预置 OC settings.json `defaultModel:"anthropic/claude-…"` + omp 角色未配置 → 打开新版设置页 → 出现导入横幅且 `modelRoles.default` **仍为空**(未自动写)→ 点击"导入" → 角色填充、来源徽章 global、审计可见;OC 旧键展示"已取代"且原值仍在 settings.json。
5. **project scope UI**:角色行切"存到项目"(`modelRoleStorage:project` 语义)→ `.omp/config.yml` 生成,`~/.omp/agent/config.yml` 不含该角色。
6. **credential**:apiToken 输入框写值 → 保存 → 刷新页面显示掩码/"已设置";DevTools 网络面板中 PUT 响应 `applied` 为 `{configured:true}`、GET 响应无明文。
7. **BehaviorPage 迁移与遮蔽检测(REVISED,§3.6)**:预置 `~/.config/opencode/AGENTS.md` 存量内容 + `~/.omp/agent/AGENTS.md` 不存在 → 打开 BehaviorPage → 提示迁移且未自动写 → 确认后 `~/.omp/agent/AGENTS.md` 生成、旧文件保留;再预置 `~/.claude/CLAUDE.md` 的环境 → 迁移 UI 必须呈现"用户槽将被接管"警告与合并选项;项目侧:项目根已有 `.omp/AGENTS.md` 时,项目 AGENTS.md 编辑器显示遮蔽警告。
8. **双目录前台切换 + project 生效(R2)**:dev 栈开目录 A 会话(触发一次需审批工具与一次 compaction)→ 切前台到 B,在 B 经 project scope 编辑 default 角色 → **B 新建会话的默认模型 = B 项目角色解析值(会话级消费,非仅 UI)**;回到 A:审批弹窗行为/compaction 行为/会话模型与切换前一致,A 会话读 A 实例,未受 B 写入影响(A 会话注入的实例引用未变)。

### 7.3 TUI 对照行为(规格说明引用)

- set 即生效、100ms debounce 持久化:settings-selector.ts:904-910 + settings.ts:2027-2046。
- 项目角色落盘:`modelRoleStorage:project` → `.omp/config.yml`(schema:537-559 选项描述;`#saveProjectNow` 只写 modelRoles,settings.ts:2159-2193)。
- 损坏隔离与拒覆盖:settings.ts:1183-1200。
- 单例与旁路工厂语义:settings.ts:404-425(init 单例)/431-434(loadReadOnly)/607-625(cloneForCwd)/627-652(reloadForCwd 的 every-holder 语义——本章禁用其依据)。
- AGENTS.md 用户级/项目级发现与优先级遮蔽:`<s>`/discovery/{builtin,opencode,claude,codex,agents,agents-md}.ts 与 capability/context-file.ts:31-36、capability/index.ts:183(§3.6 证据表)。

---

## 8. 开放问题

| # | 问题 | 建议 |
|---|---|---|
| OQ-F1 | ~~多目录权威口径~~ **裁决沿革 + R2 修订(需总纲回写)**:2026-08-19 总纲 D6-R6 冻结「全局实例唯一可写权威、禁 reloadForCwd、项目层读 loadReadOnly 旁路、写仅 modelRoles」;**2026-08-20 修订轮 2(R2 评审 H3)证实其前提失效**——`createAgentSession` 已有 `options.settings` 注入口(sdk.ts:1273-1275),§5.1 REVISED R2 改为**每目录 keyed Settings 实例注入**:会话消费本目录「全局层 + 项目层」,全局写仍唯一走 boot 实例,项目层可写仍仅 modelRoles,禁 reloadForCwd 不变 | **动作:00-MASTER D6-R6 需按本章 §5.1 修订表述**(保留四条实质,替换「项目层只读用 loadReadOnly 旁路」为「每目录 keyed 实例注入;loadReadOnly 为无实例目录只读兜底」);原「非 boot 目录运行中会话不消费本目录项目层」降级**已消除**,不再是开放问题 |
| OQ-F2 | **(REVISED R2)每目录 keyed 实例的上游确认**:`createAgentSession` 注入口已**采纳落地**(P0,§5.1;派生原语 `cloneForCwd`,settings.ts:607-625;`loadIsolated` :439-442 为备选)——不再是"是否可行"问题 | 收窄为上游确认项:①多 Settings 实例并存的支持等级(非官方 API 依赖度);②`Settings.forCwd(cwd)` keyed instances 官方化(共享全局层,消除 §5.1.7b 全局写跨实例传播缺口);③多实例下 SETTING_HOOKS/`#fireAllHooks`(:623)进程级副作用语义;④storage 句柄共享的并发担保。确认前按 §5.1.7 caveat 运行(实例数/内存、传播缺口、watcher 扇入均有显式登记与测试);确认后可切换共享全局层的 keyed instances 并重估 loadIsolated |
| OQ-F3 | ~~BehaviorPage 目标文件~~ **已裁决(本章 §3.6,2026-08-19 重新取证)**:全局层 = `~/.omp/agent/AGENTS.md`(omp 原生,优先级 100);项目层 = 项目根裸 `AGENTS.md`(互操作默认,带同深度遮蔽检测) | 裁定回填:07 章 GAP-G13 目标与本裁定一致(其开放问题 6 关闭)。**需 07 侧更正**:其 §3.3/§5.13 引用的文件路径前缀 `config/opencode.ts`、`config/builtin.ts`、`config/agents.ts` 实为 `discovery/` 目录(行号内容一致),标签需按本章 §3.6 证据表更正 |
| OQ-F4 | PUT 是否需要 `If-Match: revision` CAS | 建议不做:omp 语义本就是按键 LWW + 合并,CAS 只在 web 侧制造 TUI 没有的失败模式。若后续用户报告"编辑被覆盖"体验问题,再加可选弱校验(仅同 origin 冲突时提示) |
| OQ-F5 | OC 通知键与 omp `*.notify` 键双层并存是否合并 | 建议保留双层并加文案区分("引擎/TUI 通知" vs "OpenChamber 通知");合并方案(OC 通知系统订阅 omp 键)留 08 章原创面适配评估 |
| OQ-F6 | schema 英文 label/description 的本地化策略 | v1 原文直出;v2 以 `settings.omp.<key>.label/description` 词条覆盖,缺词条回退英文(locale-ui-patterns 的既有回退模式) |
| OQ-F7 | `options:"runtime"` 键(theme.dark/light 等 2 处)在 web 的取值来源 | 首批这些键全在 excluded 的 appearance tab 内,不构成阻塞;若未来露出,由端点调用 theme registry 序列化选项 |
| OQ-F8 | **跨章 R6 旧表述漂移清单(R2 修订后需各章对齐;本章不代改,列此供修订轮收口)**:①`00-MASTER.md` D6-R6 原文(「项目层只读用 loadReadOnly 旁路…上游注入项挂 OQ」)——按 OQ-F1 回写;②`02-agents-and-modes.md` §5.2(task.* overrides「一律写全局实例」)与 §8 开放问题 6(R6 表述)——ch02 修订轮 2 已在落地 M4 对齐(项目 overrides 门控 `settings.projectScopes.v1`,project 层 task.* 写为上游扩展);③`04-protocols-and-entities.md` 四处(§1.2 边界表「R6:全局实例唯一真值」、§5.5.1 park 的 `task.idleParkMs` 读取「全局实例…只读旁路」、§5.7.4「读取发生在 SDK 全局 settings 实例上」、§9 前置「一切设置读取遵循 06 全局实例裁决(只读旁路)」)——应改为「读取走会话注入的每目录 keyed 实例(06 §5.1 R2)」;④`03-approvals-and-dialogs.md` OQ-3「SDK 现为进程级单例,需上游评估」——注入口已存在,但 per-session(非 per-directory)overlay 仍无,表述可更新为引用 06 §5.1 | 按①→③顺序收口;④为措辞级更新。所有对齐以本章 §5.1(REVISED R2:每目录 keyed Settings 实例注入)为唯一表述源 |

---

## 9. 依赖

**前置**:
- 01(模型选择与 roles):`modelRoles` 语义、role→model 解析数据、prompt 去 model 强制——DefaultsSettings 重构与级联退役与其同 PR 线;defaultModel 导入纪律(§6.2 阶段 2)与其 GAP-11 同裁决对齐。
- 03(审批):`permissionAutoAccept` 清理的**阶段锚点**——P0 桥原子落地、P1 消费者切换、P3 删键(GAP-F8);无人值守任务 fail-closed 语义(R10)在 P1 生效。
- 05(事件流):`omp.settings.updated` 在其唯一注册表登记(envelope/ID/重放/reducer 归 05,R1);capabilities 端点(`settings.v1` + `settings.projectScopes.v1`)的形状与三矩阵归总纲/05(R2)。

**后置**:
- 07(OpenCode 残留):wire `/config` GET/PATCH、BehaviorPage 旧路径停写、`/api/behavior/agents-md` 路由的删除清单以本章处置表为输入;GAP-G13 目标已由本章 §3.6/§5.8 确认(用户级 `~/.omp/agent/AGENTS.md`),其证据路径标签更正见 OQ-F3。
- 08(原创面):small-model/walkthrough/通知系统键的最终归属说明引用本章 §5.8。

**可独立先行**:GAP-F1 端点 + GAP-F7 屏蔽 + GAP-F10 实例拓扑(§5.1 REVISED R2:每目录 keyed 实例注入)+ GAP-F6 watcher(不依赖任何其他章,且是其余项的地基)。
