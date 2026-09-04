# omp task 工具多 agent 进度 UI：两层工作计划

> 状态：**层 1 与层 2 批次 A/B/C 全部完成**（提交链 `b8674cca` → `ddeb98b2` → `e7073ecf` → `27f5f5f4` → 批次 C）；批次 C 交付：任务卡片行“查看运行”跳转（已实证 `progress.id ≡ registry agentId`，同一 id 体系）、嵌套子代理父行内缩进一层（`inflightTaskDetails`，深度封顶一层）、运行中行尾随最近输出摘要（`recentOutput` 尾行，generate-effect 动画）。浏览器目验仍待真实会话。
>
> **Rebase 复验（2026-09-03，rebase 至 v1.27.1 之后）**：层 1 全部改动完好；omp-host 369/0、ui 官方隔离跑 830/832 文件（唯一失败文件为 main 继承的 `terminalApi.test.ts`，见文末缺陷登记）。新 main 带来三处与层 2 相关的变化：① 子会话分支已支持每行花费（`useSubagentCostRollup`，按 `parentID` 子会话递归汇总）——批次 A2 的范围据此修正；② omp-host 已全量 strict 化（`1c5da257`），层 1 代码在其上类型检查通过；③ 词典新增土耳其语 `tr.ts`——层 1 的 8 个 `taskAgent` 键已补译（`2c4fe2e0` 之后的补丁）。
>
> **合跑污染定性（修正早前判断）**：裸 `bun test <目录>` 合跑时 `WorkStatusSubagentsSection`（4 例）与 `ReasoningPart`（2 例）互相污染失败，机制是 `mock.module` 进程级注册会传播到已加载消费方且还原不可靠（`mock.restore()` 无效、"beforeEach 注册 + afterEach 还原"实验亦失败，实验已全部回退）。**仓库官方门禁 `bun run test`（逐文件隔离进程）不受影响、全绿**；裸合跑不得用作 gate 结论。详见文末缺陷登记。
>
> 层 1 落地记录：① wire 透传——`omp-host/engine.ts` tool_execution_update 对 task 工具转发 `partialResult.details`，`projection.ts` toolPartial 新增 details 合并（最新快照整体替换，保留 asyncState）；`event-dispositions.json` 注记已更新。② UI 消费——`taskToolModel.ts` 新增 `readTaskAgentRows`（progress=live 行 / results=settled 行，settled 优先，index 排序，畸形忽略）与 `formatAgentDuration`；`ToolPart.tsx` 新增 `TaskAgentRowsList`/`TaskAgentRowItem`（状态色点+agent 名+label+当前工具+tokens+时长+重试文案，memo+签名比较接入流式内容通知），空态条件与 shouldRenderTaskSummary 纳入 agentRows。③ i18n——`chat.toolPart.taskAgent.*` 8 键 × 12 词典全量真实翻译（`tr.ts` 为 rebase 后补齐）。
> 门禁（提交时）：bun test omp-host 369/0（含新增 task details 透传测试）、ui parts 全绿（含 4 个新模型测试）、双包 type-check 新增 0（剩余为 @lezer/@codemirror 依赖重复既有噪音）、oxlint 新增类 0、check:events OK；`bun run dead-code` 因 bunx knip 缓存损坏未能运行（环境问题）。浏览器视觉冒烟未做——需真实 omp 会话跑并行 task 工具。
> 参照系：`node_modules/@oh-my-pi/pi-coding-agent/src`（18.0.4）TUI 渲染 + 本仓库 wire 投影/消费端现状。

## 背景与问题

omp TUI 对 task 工具有完整的实时多 agent 展示（`src/task/render.ts`：每 agent 一行——状态图标、agent 类型徽章、当前工具、tokens、cost、时长、429 重试状态、嵌套子 agent 进度；另有 agent-hub/agents-hub 总览、agent-transcript-viewer、running-subagent-badge）。

OpenChamber web UI 的 `TaskToolSummary`（`packages/ui/src/components/chat/message/parts/ToolPart.tsx`）只认 OpenCode task 工具契约（`metadata.summary/entries` 摘要数组、`<task_metadata>` 输出块、`metadata.sessionID` 子会话链接）。omp-host 会话里 pi task 工具的 `AgentProgress` 数据无人消费，于是渲染 fallback 文案 "No subagent session id on task metadata."（`ToolPart.tsx:1045`），即用户截图所见。

## 关键事实（已核实）

数据模型（`pi-coding-agent/src/task/types.ts`）：

- `AgentProgress`（:398）：`status(pending|running|completed|failed|aborted)`、`agent`、`task/description`、`currentTool/lastIntent`、`tokens`、`durationMs`、`cost`、`toolCount`、`retryState{attempt,maxAttempts,delayMs,errorMessage}`、`retryFailure{attempt,errorMessage}`、`index`。
- `TaskToolDetails.progress?: AgentProgress[]`（:544）——**只出现在运行中的 update 载荷**（`task/index.ts` `emitCombined` :1270-1282 单 agent 路径同样经 onUpdate 发 buildDetails()）。
- 完成态 `details.results: SingleResult[]`（:474）：`agent`、`exitCode/aborted/error`、`durationMs`、`tokens`、`index`——无 progress。

wire 投影现状（本仓库）：

- `tool_execution_end` → `metadata: { details }` 落地（`omp-host/engine.ts:1599-1608`）——**终态数据已在线上**。
- `tool_execution_update` → 只透传 `text` + `asyncState`（`engine.ts:1590-1598` → `projection.ts:1506 toolPartial`），**运行中的 `details.progress` 在此被丢弃**——这是层 1 需要补的一段投影，而非纯消费端。

## 层 1：task 工具每 agent 进度行（本次实施）

目标：omp task 工具运行中/完成后，web UI 在 task 工具卡片内渲染每 agent 行（状态 / 当前工具 / tokens / 时长 / 重试）。

| # | 改动 | 位置 | 说明 |
|---|---|---|---|
| 1a | update 透传 details | `omp-host/engine.ts` tool_execution_update + `projection.ts` toolPartial | 仅 `toolName === 'task'` 门控转发 `partialResult.details` 进 `metadata.details`（最新快照整体替换；避免给其他工具的 update 无谓增重）；`toolPartialMeta` 合并保留 asyncState |
| 1b | 解析 AgentProgress/SingleResult | `packages/ui/.../parts/taskToolModel.ts` | `readTaskAgentRows(metadata)`：严格收窄 `metadata.details.progress`（live 行）与 `metadata.details.results`（settled 行），按 `index` 排序，畸形形状忽略；纯函数可测 |
| 1c | 渲染 | `ToolPart.tsx` TaskToolSummary | 每行：状态色点/图标 + agent 名 + label（description ?? task）+ 当前工具/lastIntent + `formatCompactTokenCount(tokens)` + 时长 + 重试文案；复用 ToolRevealOnMount/ToolScrollableSection/memo+签名比较（流式热路径，performance-engineering 契约）；空态条件纳入 agentRows |
| 1d | i18n | `packages/ui/src/lib/i18n/messages/*` | `chat.toolPart.taskAgent.*` 键（状态 ×5、重试 ×2 等），en + es/fr/ko/pl/pt-BR/uk/zh-CN/zh-TW 全量真实翻译 |

验收：

- bun test（omp-host dispositions：task update 携带 progress 到 wire metadata；非 task 工具不转发）
- vitest（ui：readTaskAgentRows 形状/排序/畸形输入）
- 双包 type-check + `bunx oxlint` 新增路径 0
- 局限：完整视觉冒烟需要真实 omp 会话跑并行 task 工具，单测 + wire 测试覆盖数据面；UI 渲染面以类型 + 组件结构审查为准（如实报告，未做浏览器目验）。

## 层 2：agent hub 对齐（已细化，未实施；三批次独立交付）

### 现状勘定（2026-09-03 调研，修正本文初版判断）

agent-runs 链路**已存在**，层 2 要做的是补全信息、打通查看、建立联动，不是从零建：

- 服务端：engine 挂 agent-runs 聚合器（spec 04 §5.5，engine.ts:1221/1258 保留 host session）；`GET /omp/agent-runs` 快照 + `POST /omp/agent-runs/{sessionID}/{agentId}` 门控（domain-uri.ts:1716/1722）；`projectAgentRun`（domain-uri.ts:930-968）行含 `activity`（文本）、`history{agent,modelRole,resolvedModel,metrics,readOnly,outputPath(agent:// URL)}`、`hasTranscript`。
- 事件：`omp.agents.updated`（durable，快照端点 /api/omp/agent-runs，单调 revision）。
- UI：`useOmpAgentRunsStore`（runtimeKey::directory 键控，`agentRuns.v1` 能力门控，失败不伪装空成功）；WorkStatus Subagents 区块已有 agentRuns 分支（running/parked/aborted/done + blocker）。
- TUI 参照系：hub = `registry/agent-registry.ts`（AgentRef/AgentStatus）+ `session-observer-registry.ts`（ObservableSession.progress：tokens/requests/tools/cost/durationMs/contextTokens）+ `agent-hub-projection.ts` 聚合 + `agent-transcript-viewer.ts`。

与 TUI hub 的差距（按批次收敛）：

| 差距 | TUI 证据 | 现状 |
|---|---|---|
| 运行行缺实时用量指标 | 每行实时显示 token 用量、花费、时长、当前工具 | 行只有 `activity` 一段文本 + 结束后的 `history.metrics`；运行中的指标没有并入投影 |
| 无法点开一条运行看它的对话过程 | 就地查看运行中子代理的对话记录 | 只知道有没有记录（`hasTranscript` 布尔）；没有读取接口。已结束运行的产物文件可以经既有 `agent://` 链接域读取，但界面上没有入口 |
| 没有全局的"有子代理在跑"提示 | 状态行上的运行徽章 | agentsRevision 已经送到界面管线，但没有任何地方消费它 |
| 任务卡片与运行总览互不相通 | 总览里可以切入对应的子代理 | 层 1 的卡片行没有跳转；两边的运行标识预计同一体系（executor 经 AgentRegistry.global().get(result.id)），待批次内验证 |
| 嵌套与输出细节未显示 | 嵌套子代理进度、最近输出尾巴 | 层 1 未渲染 |

| # | 改动 | 测试 | 风险 |
|---|---|---|---|
| A1 | 服务端把运行中的实时用量并入快照行：投影时读取会话观察者的进度数据（与 SDK progressMetrics 同口径），`OmpAgentRun` 增 `live?: {tokens,cost,durationMs,currentTool,contextTokens,contextWindow}`；界面侧 `OmpAgentRunRecord` 与校验 schema 同步 | 聚合测试：运行中的行带实时指标、已结束的行不带；界面 schema 解析 | 低：只增可选字段 |
| A2 | 工作状态面板的 agentRuns 行对齐子会话行的信息密度：随运行刷新 token 用量、时长、当前活动；花费复用 `formatCost`（omp 运行没有会话消息，数值来自 A1 的 live.cost 或结束后的 `history.metrics`），注意与子会话分支 `useSubagentCostRollup` 的口径差异并在行展示上保持一致；点击行为依赖 B2，先只做展示 | WorkStatusSubagentsSection 测试扩展（agentRuns 分支行带指标） | 低；agentRuns 分支当前不可点击、无花费，是信息缺口 |
| A3 | 有子代理在跑时，Header 出现全局运行中徽章：订阅 agentsRevision 加目录级繁忙行数（已裁决） | store 派生选择器测试 | 低；位置需对照文案与主题约定 |

### 批次 B：点开一条运行，查看它的对话记录（中）

| # | 改动 | 测试 | 风险 |
|---|---|---|---|
| B0 | 先写规格：docs/omp-parity/ 新章，定义"读取一条运行的对话记录"的地址、分页与权限语义 | — | 先规格后实现（项目惯例） |
| B1 | 新增 `GET /omp/agent-runs/{sessionID}/{agentId}/transcript`：有记录文件时读取并按现有 entries 口径投影；登记进启动矩阵（是否持久快照依 B0 裁决） | 路由测试：存在/404/越权目录拒绝；投影与 entries 端点一致性抽查 | 中：新端点 + 矩阵覆盖 |
| B2 | 界面上点开运行行（工作状态面板或任务卡片行），在侧板新标签页（`dedupeKey: run:{key}`，只读）里看这条运行的对话过程 | 组件测试 + 手动冒烟 | 中：复用只读会话渲染还是轻量列表，B0 定 |
| B3 | 已结束的运行提供产物入口：总览行和层 1 的终态行可经 `agent://{outputPath}` 打开产物文件（链接域已存在） | 既有链接域测试补入口断言 | 低 |

### 批次 C：任务卡片与运行互通 + 嵌套细节（增强，可后置）

| # | 改动 | 测试 | 风险 |
|---|---|---|---|
| C1 | 任务卡片里的子代理行可跳到对应的运行：行内"查看运行"打开侧板对应标签；先验证 progress.id 与 registry agentId 是同一标识体系 | 标识关联单测（executor fixture） | 中：跨标识体系需实证 |
| C2 | 运行中的子代理如果自己又派了子代理，在父行内缩进显示一层（`AgentProgress.inflightTaskDetails`） | readTaskAgentRows 嵌套解析测试 | 低 |
| C3 | 运行中的行尾部滚动显示最近输出摘要（`recentOutput`，仅运行中的行做动画） | — | 低；性能契约：只给活动行做动画 |
| C4 | 工具部分详情转发目前的窄门控（仅 task 工具）——等其他工具出现真实消费需求再泛化 | — | 按需 |

### 已裁决（2026-09-03，用户确认）

1. 批次顺序：**A→B→C**。
2. B2 点开形态：**侧板新标签页**（`dedupeKey: run:{key}`，只读，与打开子会话一致）。
3. A3 徽章落点：**Header 全局**（目录级繁忙数，面板折叠时也可感知）。

### main 继承缺陷登记（rebase 复查发现，均与层 1/层 2 改动无关）

| 缺陷 | 现象 | 处置 |
|---|---|---|
| 裸 `bun test <目录>` 合跑时测试互相污染 | `WorkStatusSubagentsSection` 4 例 + `ReasoningPart` 2 例失败；各文件单独跑全绿。机制：`mock.module` 进程级注册会传播到已加载消费方，且还原不可靠；仓库官方门禁是 `bun run test`（`run-isolated-tests.mjs` 逐文件隔离进程），不受影响 | **接受**：官方门禁隔离跑全绿即可；禁止用裸合跑当下 gate 结论 |
| `terminalApi.test.ts` 2 例失败 | terminal transport 的 replay/双订阅断言（terminal-enhancer 合并 `f19a337b` 引入） | **待立项**：main 继承，与本项目无关，登记给 terminal 负责人 |
| `bunx knip` 缓存损坏 | `bun run dead-code` 无法运行（`formatly` 模块缺失） | 环境问题，重装 bunx 缓存后补跑 |
