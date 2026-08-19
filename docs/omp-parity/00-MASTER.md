# OpenChamber ↔ omp 产品对齐总纲领(Master Spec)

状态:调研完成,设计阶段
日期基线:2026-08-19(omp SDK = `@oh-my-pi/pi-coding-agent` 当前安装版;TUI 源 = `C:/Users/reamd/Documents/experiment_area/oh-my-pi`)
裁决原则:**凡 OpenChamber 与 omp TUI 产品逻辑分歧,以 TUI 为准。OpenCode 概念仅在与 omp 语义重合时保留。**

---

## 1. 背景:目标转变

OpenChamber 的 omp-host(`packages/web/server/lib/omp-host/`)最初以"兼容 OpenCode wire 契约、尽快跑起来"为目的构建:引擎是 omp 的,但产品骨架(会话/agent/permission/config 等概念模型)沿袭 OpenCode。本纲领确立新目标:

> **OpenChamber 的产品逻辑全面跟随 omp(TUI),OpenCode 残留概念清除,OpenChamber 原有增值面保留并适配。**

本文件是总纲;每个域的详细设计 spec 见各章(`01`–`08`),章节必须遵守本文件的架构总决策与模板。

## 2. 现状总判断

OpenChamber 现状 = **OpenCode 产品骨架 + omp 引擎**,概念层三类错位:

1. **OpenCode 残留**(应删):permission/question 协议全链、share/upgrade/shell、tui-bridge 事件、session.next.* durable stream、build/plan agent 二分、agent permission 编辑器、MCP 空面等(retry status 形状除外——见 §7.6,复用承载 auto_retry)。
2. **omp 缺失**(应建):model roles 体系、plan/goal/vibe 模式、15 个 URI scheme、Agent Hub/parked agents、会话树、async jobs、IRC、drafts、12 类被 engine drop 的 SDK 事件、usage row/cache-miss/compaction divider/45+ customTypes 流内元素、5,800 行 settings schema 承载的产品行为。
3. **OpenChamber 原创**(保留):WorkStatusPanel、multirun/AgentManager、scheduled tasks、projects 体系、GitHub 集成、files/terminal/browser 面板、magic slash commands、通知系统——但其"默认模型/agent"输入须迁移到 omp role 体系。

## 3. 架构总决策(所有章节必须遵守)

### D1 双轨契约策略
- **OpenCode wire 契约**(`packages/ui/src/lib/opencode/wire/`,vendored 生成代码)**不再扩张**。omp-host 继续用它承载与 OpenCode 语义重合的部分(session/message/todo/SSE 基础面),残留面按第 07 章清单停用。唯一例外:`message.part.removed` 由 05 章首次投产(retry 超越撤回),07 章删除守卫不含它;`message.removed` 仍按残留清理。
- **omp 原生概念**(model roles、会话树、Agent Hub、URI schemes、模式、jobs、审批)一律走 **OpenChamber 自有面**:`RuntimeAPIs` + `runtimeFetch`(`packages/ui/src/lib/api/`)新增 omp-parity 端点组(前缀 `/api/omp/...`),**事件一律走 05 章定义的唯一 `OmpEventBus → /api/omp/events` SSE 通道**(envelope/事件 ID/directory 作用域/`Last-Event-ID` 重放/schema 版本以 05 章为唯一权威),不硬塞进 OpenCode wire 生成类型。事件命名统一 `omp.<域>.<事件>`(如 `omp.model.changed`、`omp.dialog.requested`、`omp.mode.changed`);**禁止** `openchamber:omp-*` 等并行命名。
- 理由:wire gen 是 vendored 生成物,手改不可维护;RuntimeAPIs 是 OpenChamber 已有的自有 API 通道(ui-api-decoupling skill 管辖)。

### D2 投影与状态权威
- 引擎事件 → wire/RuntimeAPIs 事件的映射集中在 omp-host;UI 端 reducer 遵循 sync-state-invariants(权威状态、乐观更新、重连对账)。
- **处置与渲染分离**:P1 要求 SDK `AgentSessionEvent` union 的每个事件类型都有显式 case 或带理由的 intentional-ignore(以锁定 SDK 版本生成覆盖清单,新增事件未登记时 CI 必须失败);完整 UI 渲染可延至 P2。禁止无注释的 `default:` 静默丢弃。
- 断流不是空状态:任何事件 gap 必须触发相应权威 GET 对账;05 章定义唯一 bootstrap 顺序(capabilities → session snapshot → modes/model → dialogs → agents/jobs/queue → transcript 增量)。

### D3 概念映射基线(各章细化)
| OpenCode 概念(现状) | omp 真实概念(目标) |
|---|---|
| agent build/plan 二分 | 删除;模型选择 = model roles(default/smol/slow…);worker = custom agents(定义面);persona 为 OpenChamber 原创可选层(独立类型与资源,默认无,不复用 worker agent id) |
| 默认模型(defaultModel 设置项) | `modelRoles.default`(~/.omp/agent/config.yml),UI 不再每 prompt 强制显式 model;存量 defaultModel 只读检测 + 用户确认显式导入,永不覆盖已有 role |
| permission 协议/图标 | 删除;omp 审批 = approvalMode/tier + tool_approval 弹窗桥 + ask 对话框(C3+C4+C5 原子交付,见 D6) |
| question 协议 | 删除;由 ask 工具对话框桥取代 |
| share(cloud) | 删除;omp 自有 /share(加密快照)另议(低优) |
| plan(实验合成文本协议) | 与 omp plan mode(会话模式 + review overlay)合并设计;新模式端点上线即停产停用合成文本协议,遗留清扫 P3 |
| todo(OpenCode 事件型) | 保留事件通道,补 todo_auto_clear;渲染对齐 TUI 语义 |

### D4 阶段规划
- **P0 概念迁移**(改骨架):roles 取代 build/plan;默认模型链改读 omp settings;prompt 可省略 model;`/config` 默认指针修正;**审批桥 C3+C4+C5 原子交付**(桥未整备不开 `hasUI`);**`GET /api/omp/capabilities`** 版本协商上线。
- **P1 可见性桥**:全部 SDK 事件类型显式处置(D2)+ 优先渲染(auto_compaction/auto_retry+fallback/model_changed/todo_auto_clear/isTerminal);`local://` URI 读写(限同 directory/session);`/api/omp/events` 通道投产;goal 仅状态投影 + 显式用户操作。
- **P2 实体面**:Agent Hub、会话树、async jobs(capability 门控)、IRC 卡、goal 自主续跑、vibe、drafts、其余 URI schemes(ssh/vault/xd 写能力须先过威胁评审)。
- **P3 大扫除**:07 章残留全删(permission 旧链在审批桥 + 消费者切换的观察期之后)。

### D5 验证纪律
- 每章的验证方案必须含:单元/集成测试点(omp-host 用 bun:test;server JS 需 `node --check`)、E2E 场景(dev 栈 5180/3902,浏览器驱动)、与 TUI 行为的对照点。
- 行为对齐以 TUI 源码为规格说明(章节必须引用 TUI file:line)。

### D6 修订轮冻结契约(评审裁决 R1-R15;二轮修订 2026-08-20)
> gpt-5.6-sol 评审(12 高危/15 中危)裁决;各章修订必须逐条落位,冲突以本节为准。

- **R1 事件单通道**:全部 omp 原生事件经 05 章 `OmpEventBus → /api/omp/events`(唯一 SSE);01/02/03/04/06 删除各自通道设计,只引用 05 的事件注册表(SDK source → 公开名 → payload → producer → durable/volatile → 作用域 → 快照端点 → reducer)。
- **R2 capabilities 协商**:`GET /api/omp/capabilities`(端点组版本/事件 schema 版本/feature 状态/最低 UI 版本);服务端裁决开关;覆盖 新UI+旧engine、旧UI+新engine、relay 旧 bundle 三矩阵。各章本地 feature flag(如 ompModelRoles)改由 capabilities 承载。
- **R3 路径规约**:集合一律复数——`/api/omp/sessions/{id}`、`/api/omp/agent-definitions`(02 章 CRUD)、`/api/omp/agent-runs`(04 章 Hub 运行实例:revive/kill/chat/transcript)、`/api/omp/personas`(02 章,OC 原创可选 persona 层)、`/api/omp/models`、`/api/omp/commands`、`/api/omp/dialogs`、`/api/omp/jobs`、`/api/omp/settings`、`/api/omp/uri/*`。禁止组件自行拼 URL,统一经 RuntimeAPIs。
- **R4 进程归属与认证**:一切触碰 AgentSession/registry/settings/URI 的路由只注册在 omp-host(Basic auth);web server 仅做既有 `/api` 透传代理。补直连/桌面/VS Code/relay 的认证头转换表;SSE 与 `Last-Event-ID` 必须穿透验证。
- **R5 message.part.removed**:复用为有效 wire 契约(05 章 retry 撤回),07 章守卫与 DAG 移除该键;`message.removed` 照删。
- **R6 Settings 多目录(修订二轮,前提更新)**:~~进程单例不可注入~~ 已证伪——`createAgentSession` 支持 `options.settings/settingsManager` 注入(sdk.ts:1273-1275)。裁决 = **每目录 keyed Settings 实例**:omp-host `#settingsFor(dir)` 经 `boot.cloneForCwd` 派生、`#materialize` 注入,每个会话消费自己目录的 global+project 层叠(06 §5.1 REVISED R2 为准);写路径不变(全局 config.yml + 项目 modelRoles 子树);跨实例全局写传播缺口为已登记行为;reloadForCwd 仍禁止。验证含 A/B 双目录的**会话级消费**断言。
- **R7 URI 目录隔离(修订二轮收紧)**:~~进程级 artifactsDirs 并集/交换~~ 废弃(并发竞态泄露面)。P1 宿主解析仅 `local://`(会话钉扎 localProtocolOptions,零全局变异);`agent:// history:// artifact://` 宿主解析 capability 关闭,待上游 per-resolve `ResolveContext.artifactsDirs`(OQ);响应不回显绝对 `sourcePath`,以受鉴权 token 替代。
- **R8 URI 写边界**:P1 仅 `local://` 写(限同 directory/session);ssh/vault/xd 写延后 P2+ 且须威胁评审;**ssh/vault/security/mcp 的读能力同须威胁评审**(scheme×read/write capability 矩阵)。
- **R9 credential 脱敏**:设置 PUT 对 `credential:true` 键仅返回 `{configured:true}`;响应/错误/日志/事件/测试快照按 isCredential 统一清洗。
- **R10 审批原子性**:C3+C4+C5 为单一 P0 原子能力,全部落地方可启用;无人值守任务 fail-closed,不改全局审批设置;按钮集 = Approve/Deny(TUI parity),"始终允许"为弹窗外高级操作(先写设置成功再批准的事务)。
- **R11 pending dialog 生命周期**:shutdown/dispose 原子 settle 为 aborted 并写 transcript 诊断;重启不伪恢复;ID 不可猜;respond 以 registry 绑定作用域为准;四组测试(竞答/错目录/abort/热重启)。
- **R12 杂项裁决**:defaultModel 迁移=只读检测+确认导入;jobs 未获上游注入前 capability=false 且响应携带 ownerSessionID;wire `/command` 保持空返直到 07 删除;BehaviorPage AGENTS.md 已重新取证(~/.omp/agent/AGENTS.md 用户级 + 项目根裸 AGENTS.md,07/06 已对齐);goal 自主续跑留 P2;permission 清理顺序=P0 原子桥→P1 消费者切换→P3 删除;persona 独立类型默认无、仅会话级切换(单条消息 @persona 路由已删除)。
- **R13 capability ≠ UI 在场(二轮新增)**:`dialogs.v1` 只声明服务端桥就绪,绝不驱动会话 `hasUI`;`hasUI` 唯一权威 = per-session UI attachment 租约(认证+dialogs-capable 客户端,心跳/TTL);无租约 = hasUI:false = SDK 精确 fail-closed;对话框双 TTL(presented-ack 起算 T_answer + 注册期 T_present 保护 + 租约丢失 orphan 结算)保证永不悬挂(03 §5.0/§5.1)。
- **R14 queue 与 retry 撤回的上游前置(二轮新增)**:queue.v1=false 直到 SDK 扩展(入队稳定 ID + 快照携带 ID + 版本化变更事件 + 持久 requestId ledger);retry 破坏性撤回(message.part.removed 首产)为 P2 门控——P1 仅 status/overlay,门 = 服务端 synthetic/未 commit 标记 + 壳保留不变式 + 三类测试(05 §5.3);`session.error` 终裁零生产(错误经 terminal agent_end→session.idle + omp.notice.raised 沉降,05 §5.11)。
- **R15 MCP 脱期(二轮新增)**:本轮不建设可执行 MCP 端点(定义权移出 04 章,专项轮次承载);只读 + 开关禁用为长期稳态(04 §5.7.5/07 G10/08 §5.5)。


## 4. 章节索引

| 章 | 域 | 文件 | 规模 | 一句话 |
|---|---|---|---|---|
| 01 | A 模型选择 | `01-model-selection-and-roles.md` | 466 行/GAP×11 | model roles 取代默认模型链 + build/plan;thinking/fallback/enabledModels |
| 02 | B Agent 与模式 | `02-agents-and-modes.md` | 529 行/GAP×13 | custom agents 体系、plan/goal/vibe/loop 模式产品化;发现 planYolo 形状错误(P0 缺陷) |
| 03 | C 审批与交互 | `03-approvals-and-dialogs.md` | 420 行/GAP×11 | 删 permission/question 协议;建 omp 审批弹窗 + ask 对话框桥;发现非 yolo 用户工具必抛错(P0 缺陷) |
| 04 | D 协议与实体 | `04-protocols-and-entities.md` | 567 行/GAP×16 | 15 URI schemes、会话树、Agent Hub/parked、jobs、IRC、drafts、artifacts |
| 05 | E 事件流与流内元素 | `05-event-stream-and-transcript.md` | 405 行/GAP×15 | 12 类 drop 事件处置表;customTypes 四级渲染;usage row/cache-miss |
| 06 | F 设置体系 | `06-settings.md` | 414 行/GAP×10+ | 设置面代理 omp schema(~/.omp/agent/config.yml),清平行宇宙 |
| 07 | G OpenCode 残留清除 | `07-opencode-residue-removal.md` | 449 行/GAP×13 | 13 项残留的删除清单、顺序 DAG、回滚 |
| 08 | H 原创面保留与适配 | `08-chamber-original-surfaces.md` | 423 行/GAP×13 | WorkStatusPanel/multirun/projects 等的输入迁移与所有权地图 |

各章完成于 2026-08-19,全部经源码 spot-verify(合计 750+ 处 file:line 证据)。写作中的新发现已回写本文件第 7 节。

## 5. 章节模板(强制)

每章必须包含以下节,证据一律 file:line:

1. **域概述与边界**(本域管什么、不管什么;与其他章的接口)
2. **现状分析**(OpenChamber 侧:代码路径、数据流、行为,证据)
3. **目标语义**(omp/TUI 侧:API、设置键、TUI 行为,证据)
4. **差距清单**(编号 GAP-xx,每条标注 分类[删/建/改/留]、优先级[P0-P3]、风险)
5. **设计方案**(每个 GAP 或 GAP 组:数据模型、端点/事件形状、engine 改造、UI 改造、交互流;关键决策给出备选方案与取舍理由;遵守 D1 双轨)
6. **迁移与兼容**(存量数据、并发会话、回滚策略;阶段性开关)
7. **验证方案**(测试点 + E2E 场景 + TUI 对照行为)
8. **开放问题**(需用户/上游决策的点,附建议)
9. **依赖**(前置章/后置章)

## 6. 证据来源

调研底稿(5 份 scout 报告,含完整 file:line 证据,写作时直接读取):
- omp 产品面/extension API/TUI chrome:`agent://OmpProductSurface`
- omp 设置 schema 与产品行为:`agent://OmpSettingsProduct`
- omp 协议与实体(URI/树/hub/jobs/IRC):`agent://OmpProtocolsEntities`
- OpenChamber UI 产品面:`agent://ChamberProductSurface`
- wire 契约与 engine 残留:`agent://WireOpenCodeResidue`

关键代码入口:
- omp-host:`packages/web/server/lib/omp-host/{engine,endpoints,projection,events,registry}.js`
- UI 契约:`packages/ui/src/lib/opencode/{client.ts,wire/gen/}`
- UI 同步:`packages/ui/src/sync/`(受 sync-state-invariants skill 管辖)
- omp TUI:`C:/Users/reamd/Documents/experiment_area/oh-my-pi/packages/coding-agent/src/`(modes/、internal-urls/、session/、config/settings-schema.ts)
- SDK 安装副本:`C:/Users/reamd/.bun/install/global/node_modules/@oh-my-pi/pi-coding-agent/src/`

## 7. 总纲级别开放问题(各章细化)

1. ~~RuntimeAPIs omp 端点组的鉴权/多目录语义~~ **已裁决(04 章 D04-1/D04-2)**:/api/omp/* 实现在 omp-host 进程内,经既有 /api 代理直达;作用域沿用 directory 语义,不开 agentDir 全局面。
2. ~~vendored wire 的残留类型是否裁剪~~ **已裁决(07 章)**:vendored 生成物不裁剪,停止消费 + 契约守卫防回归。
3. UI 每提示强制 model 的移除节奏 —— 01 章(修订版)改为 capabilities 门控(modelRoles.v1)三矩阵分阶段,待用户批准执行。
4. ~~私有 AgentRegistry 对外暴露口径~~ **已裁决(04 章 D04-3/D04-6,修订轮更新)**:保持每会话私有 registry;URI 解析走 directory 允许集台账(R7,不再进程级 registerArtifactsDir 并集);Agent Hub 用 AgentRunsAggregator 聚合投影(`sessionID::agentId` 双段 key),面为 `/api/omp/agent-runs`。

### 写作期新发现(需要用户/上游决策)
5. **P0 缺陷两枚**(02/03 章):a) engine.js planYolo 传 `{autoApproveOnResolve}` 而 SDK 期望 `{target,thinkingLevel}` → 选 plan agent 即隐形只读模式 + xd://propose 审批时 TypeError;b) engine 创建会话无 hasUI → 非 yolo 用户任何需审批工具直接抛 "no interactive UI available",ask 工具未注册。修复属 P0。
6. **retry status 形状复用**(05/07 章):总纲原列其为残留;现裁决为复用现有 wire SessionStatus retry 形状承载 auto_retry(符合 D1,不新增面)。本文件 §2 残留清单相应修正。
7. **队列终局方案**(08 章 §8.1):UI 队列从不发送 delivery:'queue',引擎队列路径不可达;方案 B(立即发 queue + /api/omp/sessions/{id}/queue 快照 + clearQueue 整批取回)为目标,需用户拍板切换时机。
8. **persona 保留 vs D3 措辞**(02 章开放问题):OpenChamber persona(OC 原创)建议保留为可选标签、默认无;与 D3 "删除 build/plan" 不冲突但需确认。
9. **AsyncJobManager 多会话缺陷**(04 章 C2,上游):SDK 仅首个 top-level 会话获得 job manager,后续会话无 /jobs —— 需上游暴露注入项。
10. **上游增强候选**(03 章 OQ-6):select() 审批回调携带 toolCallId 等 9 条;定时任务无人值守审批需会话级 settings overlay。
