# 第 10 章 · Todo 阻塞信息面(Todo Blocker Surface)

状态:设计定稿,待实施
日期基线:2026-08-28(omp SDK 18.0.4;证据锚点以当日树为准)
上游依据:00-MASTER D1(双轨契约——vendored wire 不扩张)、D6-R1(omp 事件单通道);docs/omp-host-field-loss-fix-plan.md P14 遗留项

---

## 1. 业务目标

Todo 列表中 agent 标记 blocked 的条目,当前只显示"已阻塞"状态(Warning 色 + 图标);目标是同时显示**被什么挡住**——SDK `TodoItem.blocker` 已携带该文本,但传输途中被丢弃。

## 2. 现状数据流(blocker 丢失点)

```
SDK 事件 todo_reminder            每条 todo: {content, status, blocker?}
    ↓ engine.ts:1486-1493 投影     只取 content/status/priority —— blocker 在此丢弃
wire 事件 todo.updated             固定三字段管道(types.gen.d.ts:496-509)
    ↓ ui sync/event-reducer.ts:315 (UI 唯一 todo 消费点;GET /session/{id}/todo 端点零 UI 调用方)
StatusRow.tsx TodoItemRow          blocked 分支有视觉态,无原因文本
```

约束:vendored wire 生成类型按 D1 不得扩张,blocker 进不了 `todo.updated` 的 payload。

## 3. 设计方案:新开 omp 原生事件

```
SDK 事件 todo_reminder
    ├→ wire todo.updated(原样三字段,不动——现状兜底,旧 UI/回落路径零迁移)
    └→ omp.todo.updated(新增,经 OmpEventBus → /api/omp/events 单通道)
         payload: { todos: Array<{ content, status, blocker? }> }   ← 完整 SDK 形状
    ↓ UI 新增消费(能力存在时优先)
StatusRow blocked 行: "已阻塞({reason})" / blocker 缺失时裸 "(已阻塞)"
```

### 3.1 引擎

- `#handleEngineEvent` 的 `todo_reminder` 与 `todo_auto_clear` 两处,在 wire emit 旁追加 `#ompPublish(hostSession, 'omp.todo.updated', { todos }, { durable: false })`;todos map 读 `todo.blocker`(string 时携带)。
- omp-event-registry.json 登记 `omp.todo.updated`(volatile,scope=server);`bun run check:events` 的注册表校验自动覆盖。
- **volatile 的理由**:todo 是瞬态 UI 状态;断流不重放,SDK 每 N 轮全量重发 `todo_reminder`,下一次事件即自愈。冷启动为空与现状一致(现状 UI 也只靠事件,无快照消费)。

### 3.2 UI

1. `omp-event-reducer.ts` 新增 case `omp.todo.updated`,schema:
   `z.object({ todos: z.array(z.object({ content: z.string(), status: z.enum(['pending','in_progress','completed','abandoned','blocked']), blocker: z.string().optional() })) })`,
   存 `draft.todo[sessionID]`(整表覆盖语义,同 wire 事件);畸形帧整帧丢弃(zod 边界既有策略)。
2. `useOmpSessionStore` 导出 `useOmpTodoState(directory, sessionID)`。
3. StatusRow `TodoItemRow`:blocked 分支在状态标签后追加
   `({t('chat.statusRow.todo.status.blockedBy', { reason })})`,`reason` 缺失时用裸 `chat.statusRow.todo.status.blocked`;新 i18n 键 `blockedBy: 'Blocked by {reason}'` + 11 语言真实翻译(locale-ui-patterns 流程)。
4. **回落**:事件流中未见 `omp.todo.updated`(旧服务器)时维持 wire `todo.updated` 消费——两路写同一 store slot,omp 帧后到即覆盖,无需能力协商。

## 4. 明确不做

- 不改 wire `todo.updated` payload / vendored 生成类型(D1)。
- 不给 `GET /session/{id}/todo` 快照端点加字段——**UI 对该端点零调用**(查证:唯一 todo 消费方是 wire 事件 reducer),追加字段服务零消费者。
- 不做 todo 编辑面(写入归 agent 的 todo 工具)、不做 TUI 阶段进度头与 markdown 往返标记。
- priority 维持现状捏造值 `medium`(wire 侧历史行为,ACP 桥同款)。

## 5. 验证方案

1. 引擎:dispositions 测试 `todo_reminder` 断言 omp.todo.updated 帧含 blocker 与五态 status;`todo_auto_clear` 断言空数组帧;wire todo.updated 保持三字段(既有断言不破)。
2. 注册表:`bun run check:events` 通过。
3. reducer:合法帧覆盖 slot、畸形 status 整帧丢弃、重复帧 NO_CHANGE。
4. 视觉:blocked 行显示 reason;缺失时裸标记(vitest 组件断言或冒烟)。
5. 全门禁:bun test / tsc 双包 / oxlint 新增类 0 / check:events。

## 6. 开放问题

无——三段(投影/通道/渲染)均有既有先例(omp.custom.appended 的 volatile 发布、StatusRow 状态渲染、reducer case 模式)。

## 7. 依赖

无外部依赖;实施顺序建议先于 11 章(小改动先趟平 omp 事件新增流程)。
