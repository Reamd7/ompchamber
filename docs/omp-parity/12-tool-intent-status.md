# 第 12 章 · 工具意图状态行(Tool Intent Status)

状态:设计定稿,待实施(可选增强)
日期基线:2026-08-28(omp SDK 18.0.4)
上游依据:docs/omp-host-field-loss-fix-plan.md P12(TUI 裁决:行标题=tool.label ?? toolName,intent 属状态行);TUI event-controller.ts:1377-1380、:490-499

---

## 1. 域概述与边界

工具执行时,模型在 `ToolCall.intent` 里陈述调用理由("检查用户认证逻辑")。TUI 把它放进 **working/status 消息位**(状态行的当前动作提示),而**不是**工具行标题。OpenChamber 当前:落定后的工具行标题已有 intent(projection.ts:830-841 冷投影,engine.ts:1410 热投影 toolStarted title),**流式执行中**的"当前正在干什么"提示缺失。本章补状态行消费。

**边界**:不改工具行标题(与 TUI 一致);不改 intent 的采集(投影已透传)。

## 2. 现状分析

- **数据已到位**:冷投影把 intent 放进 tool part 的 `state.title` 与 `state.metadata.intent`;热路径 `tool_execution_start` 的 `event.intent` 已进 toolStarted 的 title(engine.ts:1408-1411)。UI 侧 `useAssistantStatus` 已存在(组件消费 assistant 状态文案)。
- **TUI 语义**:`#updateWorkingMessageFromIntent(event.intent)` → `ctx.setWorkingMessage(intent + interruptHint)`,流式期间从 args[INTENT_FIELD] 或 tool.intent(args) 持续推导(event-controller.ts:1192-1211);turn 结束清除。
- **缺**:OpenChamber 的流式状态行(会话进行中顶部/底部的动态提示)没有消费 tool part 的 intent。

## 3. 目标语义

工具执行中,状态行显示"⟳ {intent}"(intent 缺失时回落工具名);同一时刻只有一个工作意图(最新启动的工具);turn 结束清除。

## 4. 设计方案

### 4.1 数据源选择:已投影的 wire part,不加新事件

intent 已在 `message.part.updated` 的 tool part(state.title/metadata.intent)里流过 wire。**不加 omp 事件**——UI 从 part 流推导即可,零服务器改动。

### 4.2 UI 改动

1. **推导器**:`useAssistantStatus`(或其数据源 store)监听 tool part 更新——`state.status === 'running'`(pending≡running 的既有约定)且 `state.metadata.intent` 非空时,记录 `workingIntent = intent`;part 转 completed/error 时清除自己;`session.idle` 清全部。
2. **展示**:状态行动态段渲染 workingIntent(截断 ~60 字符);无 intent 时显示工具名;均无则维持现有文案。
3. **优先级**:thinking 脉冲文案与工具意图互斥——有 running tool 时工具意图优先(模型"停下手上事去调工具"是更具体的当前动作)。

### 4.3 明确不做

- 多工具并发时的意图列表(TUI 也只显示一个 working message)。
- intent 进工具行标题或历史行(落定后 title 已有,历史行不变)。

## 5. 迁移与兼容

纯 UI 增量,零服务器/协议改动;旧事件流(无 intent)自动回落工具名。

## 6. 验证方案

1. 推导器单测:running+intent 记录 / completed 清除 / idle 清空 / 无 intent 回落工具名。
2. 组件冒烟:流式会话中状态行随工具启动显示意图、落定消失。
3. 全门禁。

## 7. 开放问题

- 状态行具体挂载点(会话头部状态条 vs 输入框上方)——实施时按现有布局定。

## 8. 依赖

无;建议随任意 UI 批次顺带实施(半天量级)。
