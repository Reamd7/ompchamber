# 第 11 章 · 结构化读取面(Structured Read Surfaces)

状态:设计定稿,待实施
日期基线:2026-08-28(omp SDK 18.0.4)
上游依据:05 章 §5.2.1(结构化读取端点)、§5.6(重连矩阵推迟裁定);docs/omp-host-field-loss-fix-plan.md 遗留项 2

---

## 1. 域概述与边界

服务器三个结构化读取端点已实现且字段已修(`getTelemetry` 的 reasoningTokens、`getEntries` 的 injectedRules 均在本轮修复):

- `GET /omp/sessions/{id}/telemetry` — 每 turn 的 token/耗时/ttft
- `GET /omp/sessions/{id}/entries` — 会话结构化条目(compaction/branch/model_change/mode_change/ttsr/retry_recovery)
- `GET /omp/sessions/{id}/custom-messages` — advisor/irc/异步结果等流内注入

**但没有任何 UI 在消费它们**(omp.ts:280-282 的 `OmpSessionAPI` 三个方法零生产调用方;omp-resync.ts:151 注明推迟)。本章定义三个消费面的产品形态与接线。**边界**:只做"读已有数据并展示";不做遥测聚合统计页(usage 报表是 08 章原创面范畴)、不做条目编辑。

## 2. 现状分析

- **API 层就绪**:`lib/api/omp.ts` 三个方法 + zod schema(OmpTurnTelemetryEntry/OmpSessionEntry/OmpCustomMessageEntry)已 typed;`parseCustomMessageEntry`(omp.ts:168-179)**丢弃服务器 `text` 字段**——落地时必须补(历史 advisor 笔记的正文就在 text 里)。
- **事件侧孪生**:三个面各有 live 事件(omp.usage.turn / omp.custom.appended / divider 冷投影),UI 已消费事件;缺的是**重连/冷启动/翻历史**时的权威读取。
- **既有渲染件**:turnUsage.ts + TurnUsageRow.tsx 消费 omp.usage.turn 事件渲染每 turn 用量条;OmpCustomMessage.tsx 渲染 customType 卡片;divider 投影(chat timeline dividers,main 侧 1.20.x 已上)渲染 compaction/branch。

## 3. 目标语义

1. **逐 turn 遥测**:消息流中每条 assistant 消息的用量行在**重连/刷新后依然存在**(当前只有 live 事件喂的行,刷新即丢)。
2. **会话条目浏览**:用户能查看会话的结构化历史——压缩在何时发生、模型/模式切换序列、TTSR 注入了哪些规则、重试恢复记录。
3. **自定义消息浏览**:历史 advisor/irc/异步注入在冷启动后可读。

## 4. 设计方案

### 4.1 遥测:补种子,不建新面

TurnUsageRow 的数据源 `useOmpSessionStore` 的 telemetry slot 已由 omp.usage.turn 事件填充。方案:**会话引导时拉一次 telemetry 端点做种子**。

- 接线点:omp-resync 矩阵的 transcript 步骤之后追加 `getTelemetry` 步骤(volatile 快照,不进 durable 步骤表);结果逐条 `applyOmpEvent` 形状的合成帧写入同一 slot(不新造 store)。
- 幂等:telemetry slot 按 messageID 键合并,重复种子不叠加。
- UI 零新组件——TurnUsageRow 冷启动后自然有数据。

### 4.2 会话条目:会话信息抽屉的新 Tab

入口:现有会话信息面(消息 header 的详情/抽屉)新增"时间线"(Timeline)分区,按时间正序渲染 entries:

- `compaction` → 行:📷 图标 + method 标签 + tokensBefore→after(复用 divider 投影的 label 逻辑,但这是**列表视图**非流内注入)
- `model_change` / `mode_change` → 行:切换前后值
- `ttsr_injection` → 行:注入规则名列表(`rules: injectedRules`,字段已修)
- `retry_recovery` → 行:note + 恢复状态
- 数据:`getEntries` 全量(端点支持 kinds 过滤,Tab 全量一次拉,前 200 条 + "加载更早")
- 组件:新 `SessionTimelineTab`,放 `components/sections/session/`;zod 已有 OmpSessionEntry。
- `entries` 端点现为同步 JSON;若大会话延迟明显,后续升 SSE 流式(开放问题 OQ-2)。

### 4.3 自定义消息:补冷读对账,渲染复用既有卡片

OmpCustomMessage.tsx 已按 customType 分层渲染(advisor/irc/async-result…)。冷路径:引导时拉 `getCustomMessages` 合成 omp.custom.appended 形状写入 draft.customDetails slot(与遥测同法)。**同时修 parseCustomMessageEntry 丢 text 的缺陷**(schema 加 `text: z.string().optional()`,卡片正文回落它)。

### 4.4 重连矩阵登记

三个读取按 05 章矩阵规则登记为 volatile 步骤(telemetry/custom-messages)与 on-demand(Timeline Tab 打开时拉取,不进引导)。断流对账只补 volatile 两项;Tab 数据每次打开重拉。

## 5. 迁移与兼容

纯增量:三个端点与 API 方法已存在,无服务器改动(custom-messages 的 text 透传检查即可)。无能力开关必要(数据面只读,旧服务器 404 时 Tab 显示空态并隐藏)。

## 6. 验证方案

1. **冷启动对账测试**:模拟引导序列,断言 telemetry/custom 种子写入 slot 且 messageID 幂等。
2. **parseCustomMessageEntry**:text 字段往返单测。
3. **Timeline Tab**:vitest 组件测试(各 kind 行渲染 + 空态);人工冒烟:对一个含压缩/重试的真实会话打开 Tab。
4. **TurnUsageRow 冷存留**:刷新页面后用量行仍在(集成测试或冒烟)。
5. 全门禁:bun/vitest/tsc/oxlint。

## 7. 开放问题

- **OQ-1** Timeline Tab 的信息架构归属(会话详情抽屉 vs 独立面板)——实施时以现有抽屉容量定,不阻塞。
- **OQ-2** 大会话(千条 entries)是否需要分页端点参数——先做前 200 截断,实测后再议。
- **OQ-3** 遥测是否需要 directory 作用域批量拉取(多会话列表的用量汇总)——超出本章,归 08 章 usage 报表。

## 8. 依赖

无外部依赖;建议在 10 章之后实施(共用"事件形状合成帧写入 slot"的接线模式,10 章先趟平)。
