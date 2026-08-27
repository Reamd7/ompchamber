# 第 10 章 · Todo 阻塞信息面(Todo Blocker Surface)

状态:设计定稿,待实施
日期基线:2026-08-28(omp SDK 18.0.4;证据锚点以当日树为准)
上游依据:00-MASTER D1(双轨契约)、D6-R1(事件单通道);docs/omp-host-field-loss-fix-plan.md P14 遗留项

---

## 1. 域概述与边界

Todo 列表当前丢弃 SDK `TodoItem.blocker`("被什么挡住"的自由文本),blocked 状态虽已可视化(warning 色 + 图标,StatusRow.tsx:35-41),但用户看不到**挡住它的原因**。本章设计 blocker 从 SDK 到 UI 的完整通路。

**边界**:只做 blocker 字段的透传与展示;不改 todo 的写入面(OpenChamber 无 todo 编辑器,todo 由 agent 的 todo 工具写入)、不改 wire `todo.updated` 既有三字段消费者。

**非目标**:TUI 的 `formatPhaseProgress` 阶段进度(roman numeral 阶段头)与 markdown 往返标记——UI 已有自己的列表形态。

## 2. 现状分析

- **投影丢弃点**:engine.ts:1486-1495 `todo_reminder` → `bus.emit('todo.updated', { todos })`,map 只留 `content/status/priority`(`priority: todo.priority ?? 'medium'` 读的是 SDK TodoItem 上不存在的字段,恒为 medium——与 ACP 桥同款捏造,保留);`blocker` 无人读取。
- **wire 契约无槽位**:vendored `Todo` 类型(types.gen.d.ts:496-509)是 `{content, status, priority}`,无 blocker 字段。
- **UI 消费**:StatusRow.tsx 的 `TodoItemRow` 按 `todo.status` 渲染色调/图标/删除线(blocked=warning),标签走 i18n `chat.statusRow.todo.status.blocked`(11 语言已就位)。
- **SDK 真值**:`TodoItem = {content, status: pending|in_progress|completed|abandoned|blocked, blocker?}`(tools/todo.ts:25-30);TUI 渲染 `blocked` 为 `(blocked: <item.blocker>)` 或 `(blocked)`(todo.ts:1031-1050)。

## 3. 目标语义(对齐 TUI)

blocked 行显示阻塞原因文本,缺失时显示裸 `(blocked)` 标记;其余状态不变。

## 4. 设计方案

### 4.1 通道选择:omp 原生事件,不动 vendored wire(D1)

00-MASTER D1 裁定 wire gen **不再扩张**。blocker 不进 `todo.updated` 的 vendored payload,而是新增 omp 原生事件:

- **事件名**:`omp.todo.updated`(注册进 omp-event-registry.json,scope=server,volatile——todo 列表是瞬态 UI 状态,快照另有权威源,见 4.3)。
- **payload**:`{ todos: Array<{ content: string; status: 'pending'|'in_progress'|'completed'|'abandoned'|'blocked'; blocker?: string }> }`——完整 SDK 形状,无 priority(priority 是 wire 侧捏造,omp 面不需要)。
- **发射点**:engine.ts `todo_reminder` / `todo_auto_clear` 两处,与 wire `todo.updated` 并联发射(双轨期:wire 事件保持不动,既有消费者零迁移)。

### 4.2 引擎改动

1. `#handleEngineEvent` 两个 todo case 补 `#ompPublish(hostSession, 'omp.todo.updated', { todos }, { durable: false })`,todos map 读 `todo.blocker`(string 时携带)。
2. 事件登记:omp-event-registry.json 加 `omp.todo.updated` 条目;`check:events` 的 omp 名称扫描自动覆盖(名字含 `omp.` 前缀,注册表为唯一权威)。

### 4.3 快照与重连

todo 快照权威源是 wire `GET /session/{id}/todo`(engine.getTodos,已修 tasks 字段并有回归测试)。**缺口**:该端点同样丢 blocker。改法:engine.getTodos(P4 修复后的版本)在 wire 三字段之外追加 `blocker`——vendored 生成类型无此字段,但 wire payload 是我们发射的 JSON,追加字段对类型消费者透明(optional 字段不存在于类型中不报错,只有显式严格解析才会拒)。若不接受此松动,替代方案为 omp 快照端点 `GET /omp/sessions/{id}/todo`;**取舍:先用 wire 追加字段**(零新端点、重连矩阵零改动),UI zod schema 显式加 `blocker: z.string().optional()` 收窄。

**重连对账**:omp.resync 矩阵不为此新增步骤——todo 属于瞬态状态,断流后由下一次 `todo_reminder` 全量覆盖(SDK 每 N 轮全量重发)。

### 4.4 UI 改动

1. `omp-event-reducer.ts`:新增 `omp.todo.updated` case,schema `z.object({ todos: z.array(z.object({ content: z.string(), status: z.enum([...5 态]), blocker: z.string().optional() })) })`,存 `draft.todo[sessionID]`;负例测试:畸形 status 整帧丢弃。
2. `useOmpSessionStore` 导出 `useOmpTodoState(directory, sessionID)`。
3. StatusRow.tsx:`TodoItemRow` 消费 omp store(能力门控 `todoBlocker.v1`?**不需要**——新事件是纯增益,UI 在事件缺席时回落现状);blocked 分支追加 `<span className="text-muted-foreground">({t('chat.statusRow.todo.status.blockedBy', { reason })})</span>` 或裸 `(blocked)`。i18n 键 `chat.statusRow.todo.status.blockedBy`: 'Blocked by {reason}' + 11 语言真实翻译。
4. 冷路径:wire todo 端点追加的 blocker 字段进 store 同一 slot(StatusRowContainer 的装载器把 wire todos 归一成同一形状)。

## 5. 迁移与兼容

双轨期 wire `todo.updated` 保持三字段原样;UI 全部切到 omp store 后(验收条件 7.3 达成),wire todo 事件与 wire todo 端点的追加字段按 07 章删除列车处理。旧服务器(无 omp.todo.updated)上 UI 自动回落 wire 路径,无版本协商。

## 6. 验证方案

1. 引擎:dispositions 测试新增 `todo_reminder` 断言 omp.todo.updated payload 含 blocker;`todo_auto_clear` 断言空数组帧。
2. 注册表:`bun run check:events` 通过(新事件名已注册)。
3. reducer:对象帧接受、畸形帧丢弃、重复帧 NO_CHANGE 三个用例。
4. UI 视觉:StatusRow blocked 行显示 reason 文本;blocker 缺失显示裸标记(组件测试或人工冒烟)。
5. 全门禁:bun test / tsc 双包 / oxlint 新增类 0。

## 7. 开放问题

无——blocker 是 SDK 已有数据,通路三段(投影/通道/渲染)均有既有先例(omp.custom.appended 的 volatile 事件、StatusRow 的状态渲染)。

## 8. 依赖

无外部依赖;实施顺序建议在 11 章(结构化读取面)之前(blocker 改动小,先行验证 omp 事件新增流程)。
