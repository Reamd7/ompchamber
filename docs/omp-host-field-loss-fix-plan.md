# omp-host 数据变形丢失问题：问题点、根因与实施计划

> 状态：**全部批次已完成**（批次 1：P1 `e043fa4c` / P2 `4c5f0b03` / P4 `390cf005` / P5 `ae766150`；批次 2：P6 `0accee54` / P7 `27996b7e` / P8 `8b1b9475` / P9 `7c5feeb0` / P10 `f214897b`；批次 3：P11 `b71e38e2` / P13 `02244427` / P14 `db5f18b2` / P15 `7d1a7b3a`；P12 按 TUI 裁决降级为可选未实施，P16 仅观察。门禁全绿：bun 321/0、双包 tsc 0、oxlint 新增类 0、check:events OK、打包重编+verify 19 features）；已完成 TUI 参照重审（见每条"TUI 裁决"）

## 零、四类根因

| 根因类 | 机制 | 为什么现有门禁抓不到 | 涉及问题 |
|---|---|---|---|
| A. 投影覆盖缺口 | 投影函数只构造"已知必要字段"，wire 契约的可选字段从不发射 | 输出侧类型全 optional，tsc 对"漏发"零感知；测试只断言已发射字段 | P2/P3/P7/P10/P11-P13 |
| B. zod 整帧丢弃 | UI 事件 reducer 的 schema 校验失败即丢弃整个 envelope，字段级可空被放大成事件级丢失 | schema 测试用合法帧，非法形状无负例 | P5/P8 |
| C. 序列化白名单 vs 解析全量 | 写路径只序列化表单已知键，SDK 解析的其余键在 GUI 保存时被抹掉 | 往返测试只覆盖表单自己的键 | P1/P9/P15 |
| D. ID 体系错配 | 事件载荷携带 SDK 内部 ID，UI 按另一套 wire ID join，恒 miss | 两套 ID 各自类型正确，tsc 无法跨系统对齐 | P4 |

## 一、问题明细（含 TUI 裁决）

### 高危

**P1 引擎技能永远进不了 UI**（根因 C+D）｜TUI 裁决：CONFIRMED（对齐基准是 OpenCode-wire 而非 TUI）
- 问题点：技能页/斜杠面板只显示本地扫描结果，引擎技能（bundled、`~/.omp/agent/skills`、项目 `.omp/skills`）全部缺失，无任何报错。
- 原因三层叠加：① `GET /skill` 输出 `{name, description, path}`，wire 契约字段名是 `location`（endpoints.ts:594-606 vs types.gen.d.ts:7147-7153）；② 输入侧读 `skill.path`，SDK 字段是 `filePath`，恒空串；③ 代理层 skill-routes.js:151-156 按 `location` 过滤逐行丢弃；本地扫描根不含 `.omp` 目录。
- TUI 证据：TUI 斜杠行只读 `skill.name + description`，调用按 `filePath/baseDir` 重读文件（interactive-mode.ts:1424-1430、skills.ts:491-497）；SDK 展示层把 `content` 填为**去 frontmatter 的正文**（discovery/helpers.ts:396/409，inspector-panel.ts:346-371 展示）。`location` 先例出自 OpenCode wire（SkillV2Info 同样 location+content）。
- 修正方案：输出 `{name, description, location: filePath, content: 去frontmatter正文}`（对齐 wire 必填契约与 SDK 展示层语义）；代理层补 `.omp/skills` scope 推断。

**P2 `AssistantMessage.finish` 从不发射**（根因 A）｜TUI 裁决：CONFIRMED（含映射口径）
- 问题点：完成的 turn 在 UI 里永远"没完成"。冷热两路都不写 `finish`（projection.ts:883-904/1027-1040）；连锁：流式 hold 永真（MessageBody:1416）、turn 摘要跳过所有消息、推送正文落空。
- TUI 证据：终结由 **message_end/agent_end 生命周期**驱动（event-controller.ts:1217/1319、1762-1821），`stopReason` 只选择**终态呈现**：`aborted` → 中止行、`error` → 错误横幅+内联块（1241/1268/1356-1370），`stop`/`toolUse` 为常规（tree-selector.ts:279）；流式中消息恒带 `stopReason:"stop"`（assistant-message.ts:298-299），通知取 `agent_end.messages.findLast(assistant).stopReason`（:2264-2299）。
- 修正方案：wire `finish` 直接映射 `stopReason`（'stop'/'toolUse'/'aborted'/'error' 等原值），未知值省略字段；UI 的"完成"判定回归 finish==='stop'||'toolUse' 语义时需对照 TUI 口径复核（turn 真正终结 = agent_end 等价物，即 message_end 后无后续）。

**P3 工具结果图片全部丢失**（根因 A）｜TUI 裁决：CONFIRMED（附提取配方）
- 问题点：截图/浏览器类工具的图片永不显示。`textOfContent` 只拼 text（projection.ts:866；engine.ts:1342 热路径同），wire `attachments` 槽位从未填。
- TUI 证据：提取 = 过滤 `content.type==="image" && typeof data==="string" && typeof mimeType==="string"`（event-controller.ts:1574-1582/441-456）；read 工具行内联于 assistant 尾部（assistant-message.ts:505-581），其余工具渲染在卡片内（tool-execution.ts:706-713 = content 图片 + details 图片 + details.xdev.inner）；纯文本 join 仅用于 text 块。
- 修正方案：冷热两路把 image 块投到 `ToolStateCompleted.attachments`（base64 data + mime），details 内图片一并纳入。

**P4 retry 恢复笔记永不渲染**（根因 D）｜TUI 裁决：CONFIRMED，修复键修正为 persistenceKey
- 问题点："限流重试""换凭据重试"笔记从不出现。`omp.retry.ended` 载荷 messageID 发 SDK persistenceKey/entryId（engine.ts:1469），UI 按 wire id join 恒 miss。
- TUI 证据：TUI 在 auto_retry_start 记住最后一个 assistant 组件，auto_retry_end 用 `retryError.persistenceKey` 解析到组件（event-controller.ts:1986/2043-2054，FIFO 兜底 720-740），就地重渲染恢复注记（assistant-message.ts:430-434、transcript-render-helpers.ts:254-277：recovered → 暗 note，superseded → 隐藏）；**从不读 entryId**。persistenceKey 形如 `assistant:{timestamp}:{provider}:{model}:{responseId}:{stopReason}`（assistant-message.ts:436-446）。
- 修正方案：engine 侧解析 persistenceKey 提取 timestamp → 复用 wire id 推导（`wireMessageId('assistant', timestamp, seed)` / `#wireIdResolver`）得到 wire id 再发载荷；entryId 仅持久层用途。不做"独立通知"形态（TUI 无此形态）。

**P5 goal 模式事件整帧被丢**（根因 B）｜TUI 裁决：CONFIRMED（对象形状为正，含 goal 字段）
- 问题点：goal 进度/状态/退出永不实时更新。SDK `state` 是对象，UI zod 声明 `z.string()` → 整帧丢弃。
- TUI 证据：`GoalModeState = {enabled, mode:'active'|'exiting', reason?, goal: Goal}`（goals/state.ts:16-21）；消费：`state?.goal?.status==='dropped'`→退出、`enabled`→启用、`!enabled && paused`→暂停（interactive-mode.ts:2760-2773）驱动状态行（2695-2702）。
- 修正方案：UI schema 改 `z.object({enabled: z.boolean(), mode: z.enum(['active','exiting']), reason: z.string().optional(), goal: GoalSchema.nullable()}).optional()`，消费逻辑按上述分支对齐。

### 中危

**P6 `commands.v1` 能力键从未发射**｜TUI 裁决：CONFIRMED（上游无能力门控，翻开关与 TUI 语义一致）
- domain-commands 已实现+文档标注 landed，ompFeatures() 无此键 → 服务端恒 501、UI 门控不 fetch；测试注入自己的 features 表所以 CI 绿。
- TUI 证据：命令枚举无门控，仅设置项门控（available-commands.ts:31-96：builtin→skill→extension→custom/mcp_prompt→file 首胜去重；skill 命令受 `enableSkillCommands` 门控——注意 SDK 自身两消费者不对称：RPC/ACP 按 truthy、TUI 按 `!==false` 默认开）。
- 修正方案：omp-parity.ts 加 `'commands.v1': true`；域内枚举已按上述次序实现则不动。

**P7 `tokens.total` 从不发射**｜TUI 裁决：ADJUSTED（口径辨析后保留，但依据改为 OpenCode-wire 先例）
- SDK `Usage.totalTokens`（权威最终窗口数）被 projectUsage 丢弃，UI 退化为累加，多轮 turn 上下文占比虚高（tokenUtils 注释记录 330% 案例）。
- TUI 证据：**TUI 的上下文占比不来自逐消息 Usage.totalTokens**——来自会话级 `getContextUsage()`，锚定最后一条 assistant 的 correctedPromptTokens vs contextWindow（session-stats.ts:168-261、component.ts:1608-1690）；`totalTokens` 仅用于 memo 指纹与 ACP usage_update。逐 turn 行显示 input+cacheWrite / output / cacheRead（usage-row.ts:19-42）。
- 修正方案（保留但改口径）：按 **OpenCode-wire 先例**（opencode 1.18.18 服务器确实发 `tokens.total`=最终往返窗口，UI 注释自证）发射 `total: usage.totalTokens`；不引入 TUI 的会话级 context 统计算法（web UI 架构按消息 tokens 读）。文档标注这是 wire 兼容行为而非 TUI 语义。

**P8 thinking/model 事件发 null 致整帧丢弃**｜TUI 裁决：CONFIRMED（undefined 合法；model_changed 上游本就无载荷）
- engine.ts:1520/1501 用 `?? null`，zod optional 不收 null → 清空 thinking、切模型、session model 未设时整帧被丢。
- TUI 证据：`thinkingLevel: ThinkingLevel|undefined` 是显式契约（agent-session-events.ts:56-63），TUI 处理器不读事件字段、状态回退 `state.thinkingLevel ?? Off`（footer.ts:207-209）；`model_changed` 是**无载荷事件**（agent-session-events.ts:50），TUI 只 invalidate + 从 session.model 重读（event-controller.ts:256-259）。
- 修正方案：服务器侧条件展开省略字段（不发 null）；UI 对 model_changed 的语义对齐"invalidate + 从 session 重读"。

**P9 agent .md 编辑往返丢手写配置**｜TUI 裁决：ADJUSTED（改为 raw frontmatter 保真，白名单补键方案作废）
- 问题点：SDK 解析 12 键，`serializeAgentMarkdown` 只写 9 键，update 全量重序列化（domain-modes.ts:779）→ `autoloadSkills`/`blocking`/`output` 及任何未知键在 GUI 保存后静默消失。
- TUI 证据：SDK **不存在**任何往返序列化器——仅有的两个写入器都是 create-only 且模块私有（CLI unpack 写 8 键 agents-cli.ts:55-97；TUI Agent Hub 只写 name+description 且拒绝覆盖 agents-hub.ts:798-808）。读侧 `parseAgentFields` 静默丢未知键且**读时突变**（tools 自动追加 'yield' helpers.ts:268-270；tools 含 task 时推断 spawns:'*' :287-290）——经它往返必不保真。
- 修正方案：保留 host 自有序列化器，但改为**原始 frontmatter 记录保真合并**——解析时保留 raw 记录，update 只改请求键、整体重序列化（未知键一并存活）；写形状对齐 SDK：`---\n{YAML 2空格}\n---\n\n{body}\n`。禁止经 ParsedAgentFields 形状往返。

**P10 assistant 自带图片不进 transcript**｜TUI 裁决：CONFIRMED
- content 循环只处理 text/thinking/toolCall（projection.ts:794-877），ImageContent 跳过。
- TUI 证据：image 块按内容顺序与 text/thinking 交错渲染（assistant-message.ts:815-818/550-573），无图片协议时退化为 `[Image: <mimeType>]` 文本（:571）。
- 修正方案：加 image 分支 → wire file part（data URL），无渲染能力时 UI 至少给 mime 占位。

### 低危

**P11 synthetic 不投影**｜CONFIRMED——TUI 对 synthetic 暗色渲染、冷重建折叠为一行（ui-helpers.ts:270-294、chat-transcript-builder.ts:252-272）；修 projectUserMessage 补 `synthetic`。
**P12 流式工具行标题用原始工具名**｜**ADJUSTED：当前行为与 TUI 一致，降级为可选增强**——TUI 行标题 = `tool.label ?? toolName`（tool-execution.ts:453），`intent` 只进 working/status 消息位（event-controller.ts:1377-1380），**不是**行标题。若做：intent 应映射到 UI 的状态行等价物，而非改行标题。
**P13 divider 不带 `summary:true`**｜CONFIRMED——TUI 按角色渲染专用组件、无通用 summary 概念（compaction-summary-message.ts:26-90 消费 method/tokensBefore/tokensAfter/summary/warning）；wire 侧 `summary:true` 是 OpenCode turn-摘要选择器约定，修复保留（projection.ts:648 补标志）。
**P14 todo 状态/字段**｜CONFIRMED——TUI 无文字标签，用字形+颜色+删除线：abandoned=error 色+删除线、blocked=warning+`(blocked: <blocker>)`（todo.ts:1031-1050）；**priority 全 TUI/SDK 不存在，ACP 桥同样捏造 'medium'**（acp-event-mapper.ts:281-285，我方常量与先例一致）。修：UI 补 blocked/abandoned 视觉态 + blocker 文本；priority 维持 medium。
**P15 providers headers 非字符串值**｜**CONFIRMED 且方向锁定为服务器强制字符串**——models.yml schema 是严格 string map（models-config-schema-bundle.ts:285/189/240），非字符串值会让 ConfigFile **整份配置丢弃**仅留 warn（config-file.ts:242-264）；UI 放宽反而让用户存下引擎静默丢弃的配置。修：domain-providers 写入前 String() 强制；UI 解析失败降级为单行提示。
**P16 观察项**：`session.deleted` 缺 info（消费方容忍）；旧单数 `skill` 根分组失效（useSkillsStore.ts:102）；`.omp` scope 推断随 P1 处理。

## 二、实施计划（三批次，每批次独立可交付、独立提交）

### 批次 1：高危修复（4 项，相互独立可并行）

| 项 | 改动（TUI 修正后） | 测试 | 验证 | 风险 |
|---|---|---|---|---|
| P1 技能链 | `/skill` 读 `filePath`，输出 `{name, description, location, content(去frontmatter正文)}`；skill-routes 补 `.omp/skills` scope | endpoints 路由测试：location/content 非空且与 discoverSkills 一致 | bun test + 浏览器冒烟技能页 | 低 |
| P2 finish | 冷热两路 `finish: stopReason` 直映射（'aborted'/'error'/'stop'/'toolUse'，未知省略） | projection/engine 单测断言 info.finish；turn 摘要选择器测试复核 | bun test + 冒烟 turn 完成后解除 hold | 中：UI 完成判定口径需对照（turn 终结=agent_end 等价） |
| P4 retry.messageID | persistenceKey 解析 timestamp → wire id 推导（复用 `#wireIdResolver`），entryId 不入载荷 | engine 事件测试：断言 messageID 为 `msg_a...` 形态 | bun test + 重试场景冒烟 | 低 |
| P5 goal.state | UI zod 改对象 schema（含 goal 字段）；消费分支对齐 TUI（dropped/enabled/paused） | reducer 单测：对象帧不再丢 | vitest + goal 指示器冒烟 | 低 |

### 批次 2：中危修复（5 项）

| 项 | 改动（TUI 修正后） | 测试 | 风险 |
|---|---|---|---|
| P6 commands.v1 | ompFeatures() 加 `'commands.v1': true` | omp-parity.test 断言键 | 极低 |
| P7 tokens.total | UsageInput 加 `totalTokens?`，projectUsage 补 `total`（依据 OpenCode-wire 先例，文档注明非 TUI 语义） | projection 单测 | 低 |
| P8 null 帧 | engine.ts:1520/1501 条件展开省略字段；model_changed 语义对齐 invalidate+重读 | engine 事件测试：undefined/null 源时无该键且帧存活 | 低 |
| P9 agent.md 往返 | **raw frontmatter 保真合并**（保留原始记录、只改请求键、未知键存活；写形状 `---\n{YAML2}\n---\n\n{body}\n`） | domain-modes.test：手写含 autoloadSkills+未知键 → update → 全部存活 | 低-中：需保证 YAML 注释策略与现有一致 |
| P10 assistant 图片 | content 循环 image 分支 → wire file part（data URL + mime） | projection 单测 | 中 |

### 批次 3：低危批量 + 收尾

- P11：projectUserMessage 补 `synthetic`（UI 暗色/折叠渲染跟进）。
- P12：**降级为可选**——行标题维持 `toolName`（与 TUI 一致）；intent 若要展示，映射到状态行等价物，另立设计。
- P13：divider info 补 `summary: true`。
- P14：StatusRow 补 blocked/abandoned 视觉态 + `(blocked: ...)` 文本；priority 维持 medium（ACP 先例）。
- P15：domain-providers 写入前 header 值 String() 强制；ProvidersPage 解析失败降级单行提示。
- P16：观察不动（`.omp` scope 随 P1）。

### 每批次统一门禁

`bun test server/lib/omp-host/`（311+新增）全绿 → `bun run type-check` 双项目 0 → `bunx oxlint` 新增类 0 → `bun run check:events` OK → 涉及 UI 面的浏览器冒烟（技能页/消息渲染/goal 指示器/providers 页）→ 独立提交。

## 三、观察清单（不在本计划内，待消费面落地时处理）

- `getTelemetry`/`getEntries`/`getCustomMessages` 三个结构化端点零生产消费方（omp-resync.ts:150-154 明确推迟）。
- `parseCustomMessageEntry` 丢服务器 `text` 字段；`omp.model.changed` role 死字段；`turnUsage` 未读 `reasoningTokens`/`totalTokens`/`contextTokens`。
- `GET /api/omp/models` 注册表失败降级与零模型不可分辨（无消费方）。
- plugins `requiresRestart`、agentRuns `kind/parentId/history/hasTranscript` 等未读字段。
- SDK 自身 `enableSkillCommands` 门控不对称（RPC/ACP truthy vs TUI 默认开）——域内若复刻需选定一侧并注明。

## 附录 A：TUI 重审方法与范围

参照系：`node_modules/@oh-my-pi/pi-coding-agent/src`（18.0.4，含 modes/**、tools/、session/、goals/、config/、cli/）。逐条裁决输出 CONFIRMED / ADJUSTED / REFUTED；本轮结果：CONFIRMED ×12，ADJUSTED ×3（P7 口径、P9 序列化策略、P12 标题归属），REFUTED ×0。关键修正均已并入上文对应条目。

## 附录 B：类型化阶段已修的四个静默 bug（提交 8ac3c036）

1. `getTodos` 读 `items`/`todos`，SDK 18 字段是 `tasks`——恒返回 `[]`，已修 + 回归测试。
2. telemetry 读 `usage.reasoning`，SDK 字段是 `reasoningTokens`——恒缺失，已修。
3. `getEntries` ttsr 读 `entry.rules`，SDK 字段是 `injectedRules`——恒 undefined，已修。
4. `/skill` 输入侧读 `skill.path`（SDK 是 `filePath`）恒空串——类型化时按"零行为变化"原则保留，随本计划 P1 修复。
