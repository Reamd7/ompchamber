# omp task 工具多 agent 进度 UI：两层工作计划

> 状态：**层 1 已完成**（2026-09-03）；层 2 未启动，待另立批次。
>
> 层 1 落地记录：① wire 透传——`omp-host/engine.ts` tool_execution_update 对 task 工具转发 `partialResult.details`，`projection.ts` toolPartial 新增 details 合并（最新快照整体替换，保留 asyncState）；`event-dispositions.json` 注记已更新。② UI 消费——`taskToolModel.ts` 新增 `readTaskAgentRows`（progress=live 行 / results=settled 行，settled 优先，index 排序，畸形忽略）与 `formatAgentDuration`；`ToolPart.tsx` 新增 `TaskAgentRowsList`/`TaskAgentRowItem`（状态色点+agent 名+label+当前工具+tokens+时长+重试文案，memo+签名比较接入流式内容通知），空态条件与 shouldRenderTaskSummary 纳入 agentRows。③ i18n——`chat.toolPart.taskAgent.*` 8 键 × 11 词典（en/de/es/fr/ja/ko/pl/pt-BR/uk/zh-CN/zh-TW）全量真实翻译。
> 门禁：bun test omp-host 369/0（含新增 task details 透传测试）、ui parts 91/0（含 4 个新模型测试）、双包 type-check 新增 0（剩余 6 个为 @lezer/@codemirror 依赖重复既有噪音）、oxlint 新增类 0、check:events OK；`bun run dead-code` 因 bunx knip 缓存损坏未能运行（环境问题）。浏览器视觉冒烟未做——需真实 omp 会话跑并行 task 工具。
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

## 层 2：agent hub 对齐（后续独立批次，未实施）

对齐 TUI 的多 agent 总览体验，工作量显著更大，需先做范围设计：

- `agent-hub.ts` / `agents-hub.ts` 等价物：跨会话/会话内所有运行中子 agent 的总览面板（进度、tokens、cost、当前工具、切换查看）。
- `agent-transcript-viewer` 等价物：单个子 agent 运行 transcript 查看（数据源：SDK `TASK_SUBAGENT_LIFECYCLE_CHANNEL` 事件 / artifacts `outputPaths`，wire 侧尚无对应通道，需新投影）。
- `running-subagent-badge` 等价物：全局状态行/头部徽章。
- 层 1 的窄门控（仅 task 工具转发 details）在层 2 视需要泛化；`AgentProgress.inflightTaskDetails`（嵌套子 agent）与 `recentOutput` 流式 tail 亦留待层 2。
- WorkStatusPanel Subagents 区块目前只列 OpenCode 式子会话（按 parentId）；omp 子 agent 运行不是 wire 子会话，层 2 决定是否并入该面板或另立 surface。
