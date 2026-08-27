# omp-host 数据变形丢失问题：问题点、根因与实施计划

> 状态：计划待实施（批次 1 未开工）
> 来源：2026-08-27 omp-host TypeScript 类型化后对全部变形缝的四路审计（projection / omp 事件 / domain 序列化 / UI 读取），类型化本身已修的四个静默 bug 见文末附录，提交 `8ac3c036`。
> 门禁基线：`bun test server/lib/omp-host/` 311 pass / 0 fail；`bun run type-check` 双项目 0 错误；oxlint 新增类 0；`bun run check:events` OK。

## 零、四类根因

| 根因类 | 机制 | 为什么现有门禁抓不到 | 涉及问题 |
|---|---|---|---|
| A. 投影覆盖缺口 | 投影函数只构造"已知必要字段"，wire 契约的可选字段从不发射 | 输出侧类型全 optional，tsc 对"漏发"零感知；测试只断言已发射字段 | P2/P3/P7/P10/P11-P13 |
| B. zod 整帧丢弃 | UI 事件 reducer 的 schema 校验失败即丢弃整个 envelope，字段级可空被放大成事件级丢失 | schema 测试用合法帧，非法形状无负例 | P5/P8 |
| C. 序列化白名单 vs 解析全量 | 写路径只序列化表单已知键，SDK 解析的其余键在 GUI 保存时被抹掉 | 往返测试只覆盖表单自己的键 | P1/P9/P15 |
| D. ID 体系错配 | 事件载荷携带 SDK 内部 ID，UI 按另一套 wire ID join，恒 miss | 两套 ID 各自类型正确，tsc 无法跨系统对齐 | P4 |

## 一、问题明细

### 高危

**P1 引擎技能永远进不了 UI**（根因 C+D）
- 问题点：技能页/斜杠面板只显示本地扫描结果，引擎技能（bundled、`~/.omp/agent/skills`、项目 `.omp/skills`）全部缺失，无任何报错。
- 原因三层叠加：
  1. `GET /skill` 输出 `{name, description, path}`，wire 契约字段名是 `location`（`packages/web/server/lib/omp-host/endpoints.ts:594-606` vs `packages/ui/src/lib/opencode/wire/gen/types.gen.d.ts:7147-7153`）；
  2. 输入侧读 `skill.path`，SDK 字段是 `filePath`，恒空串（endpoints.ts:600）；
  3. 代理层 `packages/web/server/lib/opencode/skill-routes.js:151-156` 按 `item?.location` 过滤，引擎行全部被丢；本地扫描根（skills.js:156-236）只覆盖 `~/.claude|.agents|.opencode` + `OPENCODE_CONFIG_DIR`，不含 `.omp` 目录。

**P2 `AssistantMessage.finish` 从不发射**（根因 A）
- 问题点：完成的 turn 在 UI 里永远"没完成"。
- 原因：projection.ts:883-904（冷）与 StreamProjector startAssistant/finishAssistant（热）都不写 `finish`；源字段 `stopReason` 已读取（projection.ts:835/837/879）但只用于推导 status。连锁三处：流式渲染协调永挂（MessageBody.tsx:1416-1426 的 hold 永真）、turn 摘要选择器跳过所有消息（projectTurnSummary.ts:36-37）、推送通知正文查询落空（template-runtime.js:159-165）。

**P3 工具结果图片全部丢失**（根因 A）
- 问题点：截图/浏览器类工具的图片永不显示（transcript 与工具输出弹窗均无）。
- 原因：`ToolResultMessage.content` 是 `(TextContent|ImageContent)[]`，投影用 `textOfContent` 只拼 text（projection.ts:866；热路径 engine.ts:1342-1344 同样只传 text）；wire `ToolStateCompleted.attachments`（types.gen.d.ts:378）从未填过。TUI 侧证明图片会出现（assistant-message.ts:159 `#toolImagesByCallId`）。

**P4 retry 恢复笔记永不渲染**（根因 D）
- 问题点："限流重试""换凭据重试"笔记从不出现在消息卡下。
- 原因：`omp.retry.ended` 载荷 `messageID` 发的是 SDK persistenceKey/entryId（engine.ts:1469），UI 按 wire 消息 id join（omp-event-reducer.ts:445-453、ChatMessage.tsx:216），恒 miss。冷读孪生路径 getEntries 用 `wireIdFor` 映射正确（engine.ts:1679），证明是漏映射而非无解。

**P5 goal 模式事件整帧被丢**（根因 B）
- 问题点：goal 进度/状态翻转/退出永不实时更新，goal 行与指示器冻在快照种子。
- 原因：SDK `goal_updated.state` 是对象 `{enabled, mode: 'active'|'exiting', reason?}`（goals/state.ts:16-20），engine 原样转发（engine.ts:1530），UI zod 声明 `z.string()`（omp-event-reducer.ts:112-115）→ safeParse 失败整帧丢弃（:653-654，已实测复现）。

### 中危

**P6 `commands.v1` 能力键从未发射**——domain-commands 已实现+注册、DOCUMENTATION.md:103-109 标注 landed，但 ompFeatures()（omp-parity.ts:39-75）无此键 → 服务端恒 501（domain-commands.ts:218 门控）、UI 双重门控不 fetch（useOmpCommandsStore.ts:55）。测试注入自己的 features 表（domain-commands.test.ts:132/148）所以 CI 全绿。Tier B 斜杠命令面整个不可达。

**P7 `tokens.total` 从不发射**——SDK `Usage.totalTokens`（权威最终窗口数，pi-catalog types.ts:104-107）被 projectUsage 丢弃（projection.ts:480-497），UI 退化为累加各桶（tokenUtils.ts:41-52），多轮 turn 上下文占比虚高（tokenUtils 注释记录过 330% 案例：cache.read 3,291,956 vs 232,872 真实窗口）。

**P8 thinking/model 事件发 null 致整帧丢弃**——engine.ts:1520 `thinkingLevel: event.thinkingLevel ?? null`、engine.ts:1501-1503 `model: session.model ? {...} : null`，UI zod 的 optional 不收 null → 清空 thinking 档位、切到无思考模型、session model 未设时整帧被丢，chip/badge 停留旧值（已实测 safeParse 复现）。

**P9 agent .md 编辑往返丢手写配置**——SDK frontmatter 解析 12 键（discovery/helpers ParsedAgentFields：name/description/tools/spawns/model/output/thinkingLevel/autoloadSkills/readSummarize/blocking/prewalk/advisor），`serializeAgentMarkdown` 只写 9 键（domain-modes.ts:519-554）；update 全量重序列化（domain-modes.ts:779）→ 手写 `autoloadSkills`/`blocking`/`output` 在 GUI 保存一次后静默消失。违反 providers 域"表单没显示的手写配置必须存活"的既定设计承诺（domain-providers PUT 的 field-merge 语义）。

**P10 assistant 自带图片不进 transcript**——content 循环只处理 text/thinking/toolCall（projection.ts:794-877），ImageContent 块直接跳过；wire FilePart 槽位存在但从不产出。

### 低危

**P11** 用户消息 `synthetic` 标志不投影（projection.ts:570-572）——auto-continue 注入的消息显示为用户手打；UI 的 synthetic 过滤显示路径（partUtils.ts:43-52 等）永不生效。
**P12** 流式中工具行用原始工具名而非 intent 标题，落定后才有——live/cold 标题跳变（engine.ts:1296-1297 未传 title；toolStarted 按 callID 去重吞掉后续 intent；toolFinished completed 分支硬编码 toolName，projection.ts:1218）。
**P13** divider 不带 `summary:true`（projection.ts:648-687 只标 metadata.ompRole），叠加 P2 后 turn 摘要兜底可能选中 divider 的合成文本当答案（projectTurnSummary.ts:26-28）。
**P14** todo 投影：`blocked`/`abandoned` 无标签映射显示成 pending（StatusRow.tsx:69-70）、`blocker` 丢弃（wire 契约无字段）、priority 恒 medium 为捏造常量（engine.ts:1400-1404；SDK TodoItem 无 priority）。
**P15** providers 列表 zod 一票否决：models.yml 手写一个非字符串 header 值（如数字）→ UI OmpFileProviderSchema（omp.ts:1269 `z.record(z.string(), z.string())`）整表 parse 失败 → 引擎 provider 区静默空白无提示（domain-providers.ts:311 直通原值；ProvidersPage.tsx:273-279 静默保留旧值）。
**P16** 契约偏移观察项：`session.deleted` 缺 `info` 字段（engine.ts:814；消费方均容忍）；旧单数 `skill` 根目录分组解析失效（useSkillsStore.ts:102-111 锚定 `/skills/`，`.opencode/skill` 落空）；`inferSkillScopeAndSourceFromPath` 不识 `.omp` 根（skill-routes.js:89-125，当前被 P1 掩盖）。

## 二、实施计划（三批次，每批次独立可交付、独立提交）

### 批次 1：高危修复（4 项，相互独立可并行）

| 项 | 改动 | 测试 | 验证 | 风险 |
|---|---|---|---|---|
| P1 技能链 | endpoints.ts `/skill`：读 `skill.filePath`，输出对齐 vendored 契约 `{name, description, location, content}`；skill-routes.js `inferSkillScopeAndSourceFromPath` 补 `.omp/skills` 根 scope 判定 | endpoints 层新增 `/skill` 路由测试：location 非空、与 discoverSkills 返回一致 | bun test + 浏览器冒烟技能页：引擎技能（含 bundled）出现且分组正确 | 低：输出字段改名对内部无直接消费方（UI 经代理层） |
| P2 finish | projection 冷/热两路补 `finish`；先建 SDK `StopReason` → wire finish 枚举映射表，未知值省略字段 | projection 单测 + engine 事件测试断言 `message.updated` info.finish | bun test + 浏览器冒烟：turn 完成后工具行解除 hold、无抖动 | 中：UI 渲染协调逻辑切换状态，需视觉确认 |
| P4 retry.messageID | engine.ts:1469 改用与 getEntries 相同的 wire id 解析（复用 `#wireIdResolver`/`resolveWireIdToEntryId` 逆向） | engine 事件测试：注入 retry_error_update，断言 messageID 为 wire id 形态（`msg_a...`） | bun test + 冒烟：重试场景笔记出现在消息卡下 | 低 |
| P5 goal.state | UI omp-event-reducer.ts GoalUpdatedPayload `state` 改对象 schema `{enabled, mode, reason?}` 可选；核对 OmpGoalIndicator/useOmpSessionGoal 消费 | reducer 单测：对象 state 帧不再 drop | vitest + 浏览器冒烟 goal 指示器随事件更新 | 低 |

### 批次 2：中危修复（5 项）

| 项 | 改动 | 测试 | 风险 |
|---|---|---|---|
| P6 commands.v1 | omp-parity.ts ompFeatures() 加 `'commands.v1': true` | omp-parity.test 断言键存在；UI fetch 触发冒烟 | 极低（翻开关即活） |
| P7 tokens.total | UsageInput 加 `totalTokens?`；projectUsage 补可选 `total` | projection 单测断言 total 透传 | 低 |
| P8 null 帧 | engine.ts:1520/1501 改条件展开省略字段（不发 null）；UI schema 不动 | engine 事件测试：源为 undefined/null 时载荷不含该键且帧不被丢 | 低 |
| P9 agent.md 往返 | AgentDefinitionSerialization/Frontmatter/mergeDefinition 补 `autoloadSkills`/`blocking`/`output` 透传 + 序列化 | domain-modes.test 往返用例：手写三键的 .md → update 改 description → 三键仍在 | 低：SDK 解析器本就认这些键 |
| P10 assistant 图片 | content 循环加 image 分支 → wire file part（先查 SDK ImageContent 形状与 wire FilePart mime/URL 约定） | projection 单测：含 ImageContent 的消息产出 file part | 中：图片 URL 形态需与 UI 渲染器核对 |

### 批次 3：低危批量 + 收尾

- P11/P12/P13：projectUserMessage 补 `synthetic`；toolStarted/toolFinished 带 intent title；divider info 补 `summary: true`（各配 projection 单测）。
- P14：StatusRow 标签映射补 `blocked`/`abandoned`；todo 投影在契约允许处补 `blocker`。
- P15：UI omp.ts headers schema 放宽为 string|number|boolean（倾向 UI 放宽保 YAML 原值，实施时定夺）；ProvidersPage 解析失败降级为单行提示而非静默空。
- P16：仅记录观察，不动代码（含 `session.deleted` info、单数 skill 根分组、`.omp` scope 推断随 P1 处理）。

### 每批次统一门禁

`bun test server/lib/omp-host/`（311+新增）全绿 → `bun run type-check` 双项目 0 错误 → `bunx oxlint` 新增类 0 → `bun run check:events` OK → 涉及 UI 面的浏览器冒烟（技能页/消息渲染/goal 指示器/providers 页）→ 独立提交。

## 三、观察清单（不在本计划内，待消费面落地时处理）

- `getTelemetry`/`getEntries`/`getCustomMessages` 三个结构化端点零生产消费方（omp-resync.ts:150-154 明确推迟）——修复只惠及外部契约消费方。
- `parseCustomMessageEntry` 丢服务器 `text` 字段（omp.ts:168-179）。
- `omp.model.changed` 的 `role` 死字段（reducer 读、engine 不发，无组件消费存储值）。
- `turnUsage.usageFromTelemetry` 未读 `reasoningTokens`/`totalTokens`/`contextTokens`。
- `GET /api/omp/models` 注册表失败降级为 roles-only 与零模型不可分辨（UI `.default([])`；当前无消费方）。
- plugins `requiresRestart`、agentRuns `kind/parentId/history/hasTranscript` 等未读字段。

## 附录：类型化阶段已修的四个静默 bug（提交 8ac3c036）

1. `getTodos` 读 `items`/`todos`，SDK 18 字段是 `tasks`——恒返回 `[]`，已修 + 回归测试。
2. telemetry 读 `usage.reasoning`，SDK 字段是 `reasoningTokens`——恒缺失，已修。
3. `getEntries` ttsr 读 `entry.rules`，SDK 字段是 `injectedRules`——恒 undefined，已修。
4. `/skill` 输入侧读 `skill.path`（SDK 是 `filePath`）恒空串——类型化时按"零行为变化"原则用注释 cast 保留，随本计划 P1 一并修复。
