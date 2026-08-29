# 第 10 章 · Todo 阻塞信息面(Todo Blocker Surface)

状态:**已实施**(`a30bb0fb`,2026-08-28)——三处改动照设计落地:投影携带 blocker(非空才带键)、Todo 类型补可选字段(注释含 re-vendor 重放说明)、StatusRow 渲染 `(被 {reason} 阻塞)`/裸标记,blockedBy/blockedBare 键全 11 语言;reducer 类型透传零改动。
日期基线:2026-08-28(omp SDK 18.0.4;证据锚点以当日树为准)
上游依据:00-MASTER D1(todo 属 wire 承载的重合面——重合面字段补齐合法;不可动的只有 omp SDK 本体);docs/omp-host-field-loss-fix-plan.md P14 遗留项

---

## 1. 业务目标

Todo 列表中 agent 标记 blocked 的条目,当前只显示"已阻塞"状态(Warning 色 + 图标);目标是同时显示**被什么挡住**——SDK `TodoItem.blocker` 已携带该文本,传输途中被丢弃。

## 2. 现状(blocker 丢失点)

```
SDK 事件 todo_reminder            每条 todo: {content, status, blocker?}
    ↓ engine.ts:1486-1493 投影     只取 content/status/priority —— blocker 在此丢弃
wire 事件 todo.updated             payload 类型 Todo = {content, status, priority}(types.gen.d.ts:496-509)
    ↓ ui sync/event-reducer.ts:315 (UI 唯一 todo 消费点)
StatusRow.tsx TodoItemRow          blocked 分支有视觉态(P14 已修),无原因文本
```

## 3. 设计方案:现有通道补字段,三处改动

wire client 是 OpenChamber 拥有的契约拷贝(文件头自述 "OMPChamber's owned wire contract");todo 本就是 D1 认定的 wire 重合面——直接在重合面上补齐字段,不开新通道。

### 3.1 引擎(1 处)

`#handleEngineEvent` 的 `todo_reminder` map 追加:
```ts
...(typeof todo.blocker === 'string' && todo.blocker ? { blocker: todo.blocker } : {}),
```
`todo_auto_clear` 空数组路径不受影响。

### 3.2 wire 类型(1 行)

`types.gen.d.ts` 的 `Todo` 接口加 `blocker?: string;`(可选,缺席=旧语义)。
**Re-vendor 成本**:下次从 `@opencode-ai/sdk` 再生成时该行被覆盖——所有显式读写 `blocker` 的位置(引擎发射、UI 渲染)在 tsc 下立即报错,属**响亮失败**而非静默回归;重放本行补丁即恢复。在文件头注释的既有豁免清单(头注释 + error-interceptor 路径)追加本字段豁免记录。

### 3.3 UI(2 处)

1. `event-reducer.ts:315` 的 todo.updated case:slot 形状加 `blocker?: string`(透传存储,不做 schema 校验——wire 事件流解析层现状即类型透传)。
2. StatusRow `TodoItemRow` blocked 分支:状态标签后追加
   `({t('chat.statusRow.todo.status.blockedBy', { reason })})`;reason 缺失时裸 `(blocked)`(TUI formatTodoLine 同款);i18n 键 `blockedBy: 'Blocked by {reason}'` + 11 语言真实翻译(locale-ui-patterns 流程)。

## 4. 明确不做

- 不改 omp SDK(node_modules)——总纲红线。
- 不开并行 omp 原生事件、不做能力协商/回落——两端同仓同发,旧帧只是无 blocker 字段,UI 可选读取自然兼容。
- 不给 `GET /session/{id}/todo` 快照端点补字段——UI 对该端点零调用(查证:唯一 todo 消费方是 wire 事件 reducer),补了服务零消费者。
- 不做 todo 编辑面、TUI 阶段进度头、markdown 往返标记;priority 维持现状捏造值 `medium`(ACP 桥同款)。

## 5. 验证方案

1. 引擎:dispositions 测试 `todo_reminder` 断言 wire todo.updated 帧含 blocker;缺失时无该键(既有三字段断言不破)。
2. UI:vitest 组件断言 blocked 行渲染 reason / 缺失时裸标记。
3. 全门禁:bun test / tsc 双包 / oxlint 新增类 0 / check:events。

## 6. 开放问题

无。

## 7. 依赖

无;半小时量级,随任意批次顺带。
