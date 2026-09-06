# 第 14 章 · 运行对话记录读取面(Agent Run Transcript Read)

状态:**已重构——专用端点退役,读取并入标准会话读路径**(2026-09-05)。钻入统一为"打开该运行的子会话,只读"(OpenCode 子会话同款 UX:嵌入式 `mode:'chat', readOnly` 面板)。下述 §4.1 端点已删除;本章保留为设计沿革记录,现行契约为:

1. **行携带 `childSessionID`**(活 ref 从 session 取,parked 由引擎预热缓存:sessionFile 头一次性读取,身份不可变)。
2. **`GET /session/{childSessionID}` 与 `GET /session/{childSessionID}/message`** 解析子会话:目录作用域 = 转录文件位于该目录 sessions 根下;wire `parentID` = 宿主会话(共享 UI 对 parentID 会话禁 prompt,"Return to parent" 生效)。写路径(update/delete/materialize/fork/move)保持仅宿主语义。
3. **UI**:任务卡行/工作状态行点击 → `openContextPanelTab(mode:'chat', dedupeKey: session:{childSessionID}, readOnly)`;`agentRun` 面板模式与 `AgentRunTab` 组件已删除。
4. 历史磁盘行(diskScan 未接)暂不可点——与既有 backlog 一致。

---

## 以下为退役前的原始设计(2026-09-03,存档)

状态:随批次 B 实施(计划见 docs/task-agent-progress-ui-plan.md 层 2 批次 B;B2 形态已裁决为侧板新标签页)。
日期基线:2026-09-03(omp SDK 18.0.4)
上游依据:04 章 §5.5(agent-runs 域)、§5.5.2(行操作门控先例);11 章(结构化读取面:on-demand 读取不进引导矩阵的先例)

---

## 1. 域概述与边界(存档)

给"点开一条子代理运行,查看它的对话过程"提供权威读取端点与消费面。

**边界**:只读。revive/kill/chat 等行为操作已由 §5.5.2 的 `POST /omp/agent-runs/{sessionID}/{agentId}` 承担,本章不碰;不做运行中实时推送(读取是快照语义,刷新由消费方重拉,见 OQ-3);不做条目编辑。

## 2. 现状分析

- 行数据:`GET /api/omp/agent-runs` 的行带 `hasTranscript` 布尔(R7:sessionFile 不出服务器),但没有任何读取面消费它。
- 服务端可达性:live/parked 运行的 `AgentRef` 挂在 engine 的每会话 `AgentRegistry` 上(`agentsSnapshot` 已按此枚举),ref 保留 `sessionFile`(JSONL 路径)——运行中文件由子进程实时追加,读它即得最新快照。
- 历史磁盘行:`DiskScanRow` 只带 `hasTranscript`,**不保留路径**——v1 无法读取,见 OQ-2。
- 投影基建:engine 的冷投影(`#projectedMessages`)已示范 `SessionManager.open` → `buildSessionContext({transcript:true})` → `projectConversation` 链路;`projectConversation` 支持最小选项(`{sessionID, directory}`)。

## 3. 目标语义

1. 工作状态面板的 agentRuns 行(含任务卡片行,批次 C 联动)可点开,在侧板新标签页只读查看该运行的对话过程。
2. 运行中、parked(可 revive 的保留体)都可读;aborted 后 registry 清除前仍可读。
3. 旧引擎/能力关闭时入口隐藏,不报错。

## 4. 设计方案

### 4.1 端点

`GET /omp/agent-runs/{sessionID}/{agentId}/transcript?directory=…`

- 门控:`agentRuns.v1`(与快照/行为端点一致,关闭时 503 形状同既有 gate)。
- 行解析:复用 `handleAgentRunAction` 的语义——`aggregator.row(sessionID, agentId)` 找行,directory 不匹配或行不存在 → 404 `agent-run-not-found`。
- 读取:engine 侧 `hostSession = sessions.get(sessionID)` → `hostSession.agentRegistry.get(agentId)` → `ref.sessionFile`;无 ref 或无 sessionFile → 404 `no-transcript`。open `SessionManager` → `buildSessionContext({transcript:true}).messages` → `projectConversation(messages, { sessionID: manager.getSessionId(), directory })`。
- 响应:`{ sessionID, agentId, displayName, status, messages }`(messages 为 wire 投影消息,消费方按只读会话消息渲染)。
- 生命周期:manager 用后即关(冷读先例)。

### 4.2 矩阵与能力

- **不进 bootstrap 矩阵**:按需读取,无 durable omp 事件与之配对(ch11 Timeline Tab 先例——on-demand 读取不登记引导步骤)。`check:events` 不受影响。
- 能力沿用 `agentRuns.v1`,不新增键。

### 4.3 消费面(B2,已裁决形态)

- 侧板 context panel 新标签:`dedupeKey: run:{sessionID}::{agentId}`,只读。
- 行入口:工作状态面板 agentRuns 行加点击;任务卡片行入口归批次 C 联动。
- 渲染:轻量消息列表(复用既有 markdown/工具行渲染件),不嵌完整 SessionChat——运行记录是只读快照,不需要 composer/权限/同步语义。
- 刷新:打开时拉一次;不做轮询(OQ-3)。

### 4.4 产物入口(B3)

- 行的 `history.outputPath`(已保留 `agent://` URL 形态)在面板行/层 1 终态行提供打开入口,经既有 `agent://` URI 域解析——无服务器改动。

## 5. 迁移与兼容

纯增量:一个 GET 路由 + engine 一个方法 + UI 消费面。旧引擎 404 → 入口隐藏(与 ch11 空态隐藏同策)。wire 契约无破坏性变更。

## 6. 验证方案

1. 路由测试:行存在且有 transcript → 200 且 messages 为投影形状;行不存在/越权 directory → 404 `agent-run-not-found`;行存在但无 registry ref(历史磁盘行)→ 404 `no-transcript`;能力关闭 → gate 503 形状。
2. 投影一致性:同一 JSONL 经本端点与 `#projectedMessages` 的最小选项投影消息数一致(抽样断言)。
3. UI 组件测试:标签页渲染消息列表 + 空态;入口在 `agentRuns.v1` 关闭时隐藏。
4. 冒烟:对一个跑过并行 task 的真实会话,点开运行行查看对话过程。
5. 全门禁:bun(omp-host)/隔离测试(ui)/双包 tsc/oxlint。

## 7. 开放问题

- **OQ-1 大记录**:运行记录通常有界(单任务会话);v1 不分页,若实测超千条再加 tail 截断参数(先例:ch11 OQ-2)。
- **OQ-2 历史磁盘行读取**:需扩展 disk scan 在服务器侧保留路径(不出 wire),v1 明确 404 `no-transcript`。
- **OQ-3 运行中实时刷新**:v1 打开时拉一次;后续可挂 `omp.agents.updated` revision 触发轻量重拉,或 SSE 流式(成本高,暂不做)。

## 8. 依赖

- 批次 A 已落地(行上 live 指标与徽章);本章仅依赖既有 agent-runs 域与 projectConversation 基建。
