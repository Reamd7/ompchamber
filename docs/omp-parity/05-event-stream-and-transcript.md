# 第 05 章 域 E:事件流与流内元素(Event Stream & In-Stream Elements)

状态:设计稿 v3(REVISED 2026-08-20;一轮 D6 裁决 R1/R2/R3/R4/R5/R6/R12 + 二轮评审 H4/H5/M7/M10 已落位,见 §5.2.4/§5.3/§5.11)
归属:omp-parity 总纲 `00-MASTER.md`(裁决原则、D1–D6 适用;与本章冲突处以 D6 为准)
本章的硬性输入:
- 总纲 D2 + D6-R6:engine `#handleEngineEvent` 的 `default:` 清零 —— SDK `AgentSessionEvent` union 的**每个成员**(24 个 = core `AgentEvent` 10 + session 扩展 14,§5.1.1)必须有显式 case 或带理由的 intentional-ignore;**处置(P1)与渲染(P2)分离**。
- 总纲 D1 + D6-R1:本章是 **omp 原生事件的唯一权威** —— 唯一 `OmpEventBus → /api/omp/events` SSE 通道 + §5.0 事件注册表;01/02/03/04/06/08 章只引用本章注册表,不得自定义通道或事件名。

---

## 1. 域概述与边界

### 1.1 管什么

- **事件管道**:omp SDK `AgentSessionEvent`(core `AgentEvent` 10 个成员,`pi-agent-core/src/types.ts:864-885`;session 扩展 14 个,`session/agent-session-events.ts:12-64`;合计 24)→ omp-host `engine.js #handleEngineEvent` → `WireEventBus`(wire 轨)+ `OmpEventBus`(omp 轨,R1 唯一)→ UI reducer/stores → 组件。
- **事件注册表(唯一权威,D6-R1)**:全部 omp 原生事件(`omp.<域>.<事件>`)的 SDK 源类型 → 公开名 → payload → producer → durable/volatile → 作用域 → 快照端点 → reducer 在 §5.0.3 统一登记;其余章只引用(§1.2)。
- **被丢弃事件的处置表 + 全量覆盖清单**(总纲 D2/D6-R6):auto_compaction_start/end、auto_retry_start/end、retry_fallback_applied/succeeded、model_changed、ttsr_triggered、todo_auto_clear、irc_message、notice(info/warning)、thinking_level_changed、goal_updated、tool_execution_update、`agent_end.isTerminal === false`(§5.1);24 成员覆盖清单与 CI 守卫(§5.1.1)。
- **auto_retry 取代语义(P1/P2 拆分,二轮 H4/M10)**:P1 = loader/status + `omp.retry.*` 取代 overlay(**零 wire 突变,不产 `message.part.removed`**);P2 = 门控经验证的合成 tool part 回收(§5.3.4;D1 唯一例外 + D6-R5 的首产延后至 P2;`message.removed` 不用,07 章照删)。
- **customType 流内元素的分层渲染**(45+ 类型:advisor、irc:*、async-result、skill-prompt、lsp-late-diagnostic、live-delegation、hidden preludes 等)。
- **transcript 元素投影**:per-turn usage row(含 ttft/tok-s)、cache-invalidation 分隔线、compactionSummary/branchSummary 折叠条目、fileMention 行、bashExecution/pythonExecution 执行角色、hookMessage。
- **双轨通道分配与重连重放**:wire 轨(既有 `/event`)与 omp 轨(唯一 `/api/omp/events`,R1)的判据、envelope、进程内单调 id、`Last-Event-ID` 有界重放、断流对账矩阵(R12)与 capabilities 事件 schema 协商(R2)。

### 1.2 不管什么(接口给其他章)

| 相邻域 | 边界 |
|---|---|
| 01 模型选择 | 本章保证**模型徽章真值**的传输(`model_changed`/fallback → wire `session.updated` + `omp.model.changed`/`omp.fallback.applied|succeeded`);role 体系、默认模型链在 01 章(payload 草案 01 §5.6,注册表 §5.0.3-A) |
| 02 Agent 与模式 | `omp.mode.changed`/`omp.goal.updated`/`omp.plan.*` 的**消费面**(goal 状态条、模式 UI)在 02 章;事件名/payload 草案 02 §5.4,登记在 §5.0.3-B |
| 03 审批与交互 | 审批/ask 桥的对话框事件 = `omp.dialog.requested`/`omp.dialog.settled`,经本章通道下发(§5.0.3-C);payload/生命周期(R11 settle 语义)归 03 章 |
| 04 协议与实体 | IRC 的 bus/hub 实体、Agent Hub、会话树在 04 章;`omp.agents.updated`/`omp.tree.updated`/`omp.jobs.updated` 在 §5.0.3-D 登记(payload/producer 归 04 章);本章管 `irc_message` 的卡片渲染(`omp.custom.appended`) |
| 06 设置 | thinking level 的持久化设置键;`omp.settings.updated` 经本章通道(§5.0.3-F),payload/触发时机归 06 章 |
| 07 残留清除 | `session.next.*` durable stream、tui-bridge 事件等死契约的删除清单;本章**不采纳** `session.next.*` 作为投影目标(总纲 D1 修订已裁决;唯一例外 `message.part.removed` 见 §5.3.4,首产延后 P2 门控,07 章守卫仍按 R5 放行);**wire `session.error` 终局 = 本章不生产**(§5.11,07 章 G08 HOLD 的 error 项可删,替换清单已列);P2 门 b 的 reducer 壳保留不变式需 07 §5.8 对账(§5.3.4) |
| 08 原创面 | WorkStatusPanel 的 usage/配额聚合消费 `info.tokens`(数据本章已投影);queue 方案 B 的 `omp.queue.changed`(§5.0.3-G)与写入协议(requestId/ack/outbox,R12)归 08 章,对账入本章 §5.2.4 矩阵 |

---

## 2. 现状分析(OpenChamber 侧)

### 2.1 事件管道现状

```
AgentSessionEvent (SDK)
  → engine.js #handleEngineEvent (506-614)      [映射点,9 个 case]
  → StreamProjector (projection.js:379-583)      [流式 turn 投影]
  → WireEventBus (events.js:10-58)               [单调 id + 2048 重放环 + 目录定向]
  → endpoints.js sseHandler (553-591)            [/event 与 /global/event SSE,Last-Event-ID]
  → ui sync event-pipeline.ts → event-reducer.ts → directory stores → 组件
```

- `#handleEngineEvent` 实际映射(`engine.js:506-614`):`message_start/update/end`、`tool_execution_start/end`、`agent_start`(→`session.status busy`,:579)、`agent_end`(→`session.updated`+`session.idle`,:582-596)、`todo_reminder`(→`todo.updated`,:598-605)、`notice`(仅 `level==='error'` 打 server console,:607-610)。**其余全部落入 `default: return`(611-612),静默丢弃。**
- wire 侧**全部**产出事件(scout WireOpenCodeResidue 实测):`session.created/updated/deleted/status/idle`、`todo.updated`、`message.updated`、`message.part.updated`、`message.part.delta`(文本/推理,`projection.js:460-503`)。**不产出**任何 removal、retry、compaction、ttsr、notice 类事件。
- `WireEventBus`:envelope `{id, type, properties}` + `directory` 路由;`subscribeSince(lastEventId)` 先重放环(容量 2048,`events.js:8`)再订阅(`events.js:46-58`)。SSE 心跳 15s(`endpoints.js:577`)。

### 2.2 投影现状(冷 + 流式)

- **custom 消息**:`projectCustomMessage`(`projection.js:159-189`)把 omp `custom` 角色压成一条 assistant 侧合成消息,文本 part 带 `[omp:<customType>] ` 前缀 + `synthetic: true`;id 由 `wireMessageId('custom', timestamp, label+body)` 确定性生成(`projection.js:41-44`,角色字符 `c`)。`display:false` 与空文本被丢弃(`projection.js:361-362`)。
- **未处理角色**:`projectConversation`(`projection.js:349-369`)只处理 `user/assistant/custom/toolResult`。**`bashExecution`、`pythonExecution`、`fileMention`、`hookMessage`、`branchSummary`、`compactionSummary` 全部静默跳过**——历史里的 compaction 分隔线、!bash/$python 执行记录、文件引用行在 OpenChamber 完全不可见。
- **usage**:`projectUsage`(`projection.js:69-86`)投影 `input/output/reasoning/cacheRead/cacheWrite` 进 wire `info.tokens`(`projection.js:319-320`);`cost` 恒 0(SDK 侧 per-message 无 cost,注释 :81-84);**`duration`/`ttft` 丢弃**(SDK `AssistantMessage` 上有,`pi-ai/src/types.ts:937-939`)。UI 目前只在 ContextSidebarTab(`ContextSidebarTab.tsx:76-84`)、VSCodeLayout(:704-708)、contextUsage 聚合中消费 tokens,**transcript 无 per-turn usage 行**。
- **流内 tool part**:`toolStarted/toolFinished`(`projection.js:507-560`)只有 running/completed/error 三态;**无 partial 更新路径**(对应被丢弃的 `tool_execution_update`)。
- **冷读路径**:live 会话用 `live.agentSession.messages`(`engine.js:372`),冷文件用 `SessionManager.buildSessionContext({transcript:true})`(`engine.js:383`)。二者共用 `projectConversation`,id 确定性已有测试保障(`omp-host.test.js:119-122,137-147`)。

### 2.3 UI 消费现状(与本章相关的"已建未用"机器)

这些是本章设计可以直接点亮的**既有 UI 机器**(避免重复建设):

| 机器 | 位置 | 现状 |
|---|---|---|
| retry status 形状 | wire `SessionStatus {type:'retry',attempt,message,next}`(`types.gen.d.ts:510-527`);reducer `event-reducer.ts:325-331`;全局表保留 retry 条目(`global-session-status.ts:23,37`);快照校验 `sync-context.tsx:530-549` | **零生产者**。`useAssistantStatus.ts:343-349` 已把它变成 `retryInfo{attempt,next}` 喂给 working 状态 |
| 队列闸门 | `useQueuedMessageAutoSend.ts:169-173`:busy/retry→idle 边沿触发派发 | 因 retry 从不出现,当前只感知 busy/idle |
| `message.part.removed` | reducer `event-reducer.ts:457-473`(按 partID splice,**空则删消息**,:465-466 ← P2 门 b 要改为永不删壳,§5.3.4) | **零生产者**(P1 保持零;P2 门控后才首产) |
| `message.removed` | reducer `event-reducer.ts:386-395` | **零生产者** |
| todo 清空 | `todo.updated` 全量替换;WorkStatusTasksSection live 优先、persist 兜底 | 只在 `todo_reminder` 到达时刷新(`engine.js:598`),`todo_auto_clear` 丢弃 → **todo 陈旧** |
| toast 通道 | sonnder/permission-toast 模式(`sync-context.tsx:1527-1560` 为 permission.asked 建 toast) | 可复用于 notice |
| shellAction 文本 part | `MessageBody.tsx:1859-1935` 处理 `shellAction` text part(scout ChamberProductSurface) | 无生产者(可为 bash/python 执行角色复用该渲染分类) |

### 2.4 已知行为缺陷(由丢弃直接导致)

1. **auto-retry 不可见 + 重影**:失败尝试流式渲染(error 态 tool 卡 + error 消息),retry 成功后新尝试以新确定性 id 再渲染一遍——用户看到同一 turn 两份;TUI 仅回收**未 commit 的合成失败卡**(`#syntheticFailureCards` ∧ `isBlockUncommitted`,`event-controller.ts:1942-1955`)。修复分两期:P1 可见性(loader/status/取代 overlay,零 wire 突变),P2 门控回收(§5.3)。
2. **过早 idle**:`agent_end` 无条件 `session.idle`(`engine.js:595`)。SDK 对"async 交付将继续"的 settle 标 `isTerminal:false`(`agent-session.ts:2836-2839`);TUI 据此抑制完成通知(`event-controller.ts:2200-2201`)并视为调度暂停(:1705-1719)。OpenChamber 则立刻 idle → **队列自动派发误触发**(`useQueuedMessageAutoSend.ts:170-172` 的 idle 边沿)。
3. **模型徽章失真**:mid-session 模型切换与 retry fallback 均无事件;`ChatMessage.tsx:281-363` 从 `message.info.providerID/modelID` 解析徽章,fallback 后新消息虽带真实模型,但会话头/registry 元数据不更新(scout WireOpenCodeResidue B3/B4:"OpenChamber can silently run a different model than selected")。
4. **compaction 不可见**:进行中无 loader;完成后 `compactionSummary` 角色被投影丢弃(§2.2),历史无分隔线。
5. **irc 卡延迟**:live 事件丢弃,冷投影 `[omp:irc:*]` 文本只有 refetch 才出现(scout B7)。
6. **notice 无 info/warning 通道**:仅 error 进 server console(`engine.js:607-610`)。

---

## 3. 目标语义(omp/TUI 侧规格)

行为对齐以 TUI 源码为规格说明(总纲 D5)。TUI 的完整事件分发表(`event-controller.ts:233-285`)覆盖每一个 `AgentSessionEvent` 变体,是本章处置表的对照基准:

| SDK 事件 | TUI 行为 | 证据 |
|---|---|---|
| `auto_compaction_start` | 取消 idle compaction/recap 定时器;终端进度条;状态容器换 loader,文案 = reason 前缀 + action 标签("Auto context-full maintenance…/Auto-handoff…/Auto-shake…/Auto-snapcompact…")+ "(esc to cancel)" | `event-controller.ts:1826-1859` |
| `auto_compaction_end` | 撤 loader;aborted/result/error/skipped 分支提示;`flushCompactionQueue({willRetry})` 释放压缩期间排队的消息 | `event-controller.ts:1861-1940` |
| `auto_retry_start` | `#retryPending=true`;登记 superseded assistant 组件;**回收未 commit 的合成失败卡**(`#retractToolCardEntry`,仅 `isBlockUncommitted`);ThinkingLoop 错误解除置顶横幅;loader `Retrying (n/max) in Xs…` | `event-controller.ts:1942-1977` |
| `auto_retry_end` | 撤 loader;`applyRetryRecovery(retryError.retryRecovery)` 按 `persistenceKey` 应用恢复注记;终端失败置顶横幅 "Retry failed after N attempts: …" | `event-controller.ts:1979-2034` |
| `retry_fallback_applied/succeeded` | `showWarning("Fallback: A -> B")` / `showStatus("Fallback succeeded on M")` | `event-controller.ts:2036-2046` |
| `ttsr_triggered` | `TtsrNotificationComponent` 逆流警告框;**连续通知合并进上一块**(未 commit 时) | `event-controller.ts:2048-2068` |
| `todo_reminder` / `todo_auto_clear` | 提醒框 / `reloadTodos()`;ACP 把 auto_clear 映射为 plan entries `[]`(acp-event-mapper.ts:289-290,scout) | `event-controller.ts:2070-2076` |
| `irc_message` | 签名去重(role:customType:timestamp);`addMessageToChat` 建卡;**TTL 过期回收 + 在场卡数量上限**,仅回收未 commit 块 | `event-controller.ts:856-914` |
| `notice` | error/warning/info → `showError/showWarning/showStatus` | `event-controller.ts:976-985` |
| `model_changed` / `thinking_level_changed` | 状态行失效重绘 / 思考可见性传播到已渲染消息 | `event-controller.ts:255-283` |
| `tool_execution_update` | 更新 pending tool 组件的部分结果;async state 终态(completed/failed)且属于 parked 后台调用才视为终端,否则仍等 `tool_execution_end` | `event-controller.ts:1443-1468` |
| `agent_end.isTerminal=false` | 调度暂停而非运行结束:命令可立即挂载、抑制完成通知 | `event-controller.ts:1705-1719,2200-2201`;`live/controller.ts:322` |

**流内元素(TUI 规格)**:

- **per-turn usage row**:`formatUsageRow`(`usage-row.ts:19-42`)= `本地时间戳 ↑in(=input+cacheWrite) ↓out ⛁cacheRead(>0 时) ⏱ttft(>0 时) ⚡tok/s(duration>100ms 且 output>0 时,tok/s = output/duration×1000)`;数据源是 `message_end` 携带的 `AssistantMessage.usage/duration/ttft/timestamp`(`event-controller.ts:1308-1321`;类型 `pi-ai/src/types.ts:917,937-939`;`Usage` 字段 `pi-catalog/src/types.ts:95-135`)。
- **cache-invalidation 分隔线**:纯客户端推导,`detectCacheInvalidation(prev, current)`(`cache-invalidation-marker.ts:49-66`):prev.cacheRead≥2048 && current.cacheRead==0 && current.cacheWrite>0 && reprocessed(=cacheWrite+input)≥2048 —— 只对显式前缀缓存(Anthropic/Bedrock)报,隐式缓存(Google/OpenAI)不误报;渲染 `────── ⊘ cache miss · N tokens`。
- **compaction/branch/handoff 分隔线**:统一的可折叠 slim divider(`compaction-summary-message.ts:10-191`),compaction 带 `warning`(死端警告,`session-entries.ts:108-113`)。
- **消息角色全集**(`session/messages.ts:237-305` 序列化处):`user/assistant/toolResult/bashExecution/pythonExecution/custom/hookMessage/branchSummary/compactionSummary/fileMention`;fileMention 呈 `└ Read <path> (N lines)` 行(scout OmpProductSurface §C)。
- **customType 库存**:45+ 核心类型 + 扩展任意类型(scout OmpProductSurface §C);隐藏类型经 `display:false` 标记(如 magic-keyword 通知 `agent-session.ts:5262-5300`、todo preludes `todo-tracker.ts:149-184`、ttsr 注入 `ttsr-coordinator.ts:276-278`),显示判定 `isDisplayableQueuedMessage`(`queued-messages.ts:28-30`)。
- **恢复注记(持久化)**:superseded 失败消息携带 `AssistantRetryRecovery`(`pi-ai/src/types.ts:857-882`),`auto_retry_end.retryErrors` 携带 `RetryErrorUpdate{entryId, persistenceKey?, note, retryRecovery}`(`shared-events.ts:252-267`)。

---

## 4. 差距清单

| # | 差距 | 分类 | 优先级 | 风险 | 概述 |
|---|---|---|---|---|---|
| GAP-E01 | drop-switch 清零(D6-R6 处置/渲染分离) | 建 | P1 | 高 | `engine.js:611-612` default 静默丢弃 15 个 union 成员,违反 D2/R6;逐类显式处置(§5.1)+ 24 成员覆盖清单与 CI 守卫(§5.1.1) |
| GAP-E02 | auto_retry:loader 缺失 + supersession 无回收 | 建 | **P1 状态+overlay / P2 门控回收**(二轮 H4/M10 拆分) | 高 | P1:retry status 生产 + `omp.retry.started{supersededMessageID}` 取代标注,零 wire 突变;P2:仅合成 settle tool parts 的经验证回收(§5.3.2/§5.3.4) |
| GAP-E03 | 模型徽章失真 | 改 | P1 | 高 | `model_changed`/`retry_fallback_*` 丢弃 → session 元数据与徽章陈旧(§5.4) |
| GAP-E04 | compaction 进度/结果不可见 | 建 | P1 | 中 | loader 无、`compactionSummary/branchSummary` 冷投影丢弃(§5.5) |
| GAP-E05 | todo_auto_clear 未映射 | 建 | P1 | 低 | 补 `todo.updated {todos:[]}`(§5.1 行 8) |
| GAP-E06 | irc live 缺失 + notice 无 toast | 建 | P1 | 中 | `irc_message` 仅冷投影;notice info/warning 无通道(§5.1 行 9/10) |
| GAP-E07 | ttsr 无 live 卡 | 建 | **P1 处置 / P2 渲染**(R6 重定级) | 低 | 事件透传 `omp.ttsr.triggered` 随 P1a 落;逆流规则注入警告框 + 合并语义渲染 P2(§5.1 行 7) |
| GAP-E08 | thinking_level / goal 传输缺失 | 建 | **P1 处置 / P2 渲染**(R6 重定级) | 中 | 事件透传 `omp.thinking.changed`/`omp.goal.updated`(01/02 定名)随 P1a 落;消费面在 02 章,渲染 P2(§5.1 行 11/12) |
| GAP-E09 | tool_execution_update 丢弃 | 建 | P1 | 中 | 长任务无部分结果;补 projector partial 路径(§5.6) |
| GAP-E10 | agent_end 无条件 idle | 改 | P1 | 高 | `isTerminal:false` → 过早 idle → 队列误派发(§5.7) |
| GAP-E11 | customType 无分层渲染 | 建 | P2 | 中 | 45+ 类型全部落 `[omp:<type>]` 文本;需 T1–T4 分层 + 结构化透传(§5.8) |
| GAP-E12 | per-turn usage row 缺失 | 建 | P2 | 中 | tokens 已投影但无行;ttft/duration 丢弃(§5.9) |
| GAP-E13 | cache-invalidation 分隔线缺失 | 建 | P2 | 低 | 纯 UI 推导可移植(§5.9) |
| GAP-E14 | transcript 角色投影缺口 | 建 | P2 | 中 | bash/python/fileMention/hookMessage/branchSummary/compactionSummary 被丢弃(§5.10) |
| GAP-E15 | omp 原生事件通道 + 注册表 + 重放策略缺失(R1 唯一权威) | 建 | P1 | 高 | 唯一 `OmpEventBus → /api/omp/events` SSE、§5.0 事件注册表、envelope/单调 id/有界重放、capabilities 事件 schema 协商(R2)、omp-host 归属与鉴权穿透(R4)(§5.0/§5.2) |
| GAP-E16 | 断流对账无统一 bootstrap | 建 | P1 | 高(D2 违例面,二轮 H5 升级) | R12:canonical bootstrap 顺序 + generation/version 规则 + queue 纳入矩阵(08 章 requestId/ack/outbox 写入协议跨引);二轮 H5:补 settings/models/tree 行 + durable→快照端点全覆盖机器校验(§5.2.4/§5.1.1-3e) |

P0/P1 分期对应总纲 D4 + D6-R6:P1 = 全部 24 成员的显式处置(D2 强制,含 E07/E08 的处置半边)+ 可见性桥核心(E01–E06、E09、E10、E15、E16);P2 = 实体面配套渲染(E07/E08 的渲染半边、E11–E14)。

---

## 5. 设计方案

### 5.0 双轨通道总则与事件注册表(D6-R1:唯一权威)

#### 5.0.1 双轨通道分配(遵守 D1,REVISED)

| 轨 | 载体 | 判据 |
|---|---|---|
| **wire 轨** | 既有 OpenCode wire 事件(`WireEventBus` → `/event`),**零新增事件类型** | 与 OpenCode 语义重合:session 状态、message/part 增删改、todo。允许启用**已在契约且 UI 已 reduce 但零生产**的类型——`session.status{retry}`(P1a 点亮)与 `message.part.removed`(点亮延后 P2 门控,§5.3.4;仍是 D1 唯一例外/R5)——这是"点亮"而非"扩张"(`message.removed`、`session.error` 均不点亮:前者 07 章照删,后者见 §5.11 终局) |
| **omp 轨(R1 唯一)** | 唯一 `OmpEventBus` → `GET /api/omp/events` SSE + `/api/omp/sessions/{id}/…` 冷读,经 `RuntimeAPIs` + `runtimeFetch` | 全部 omp 原生概念:model/thinking/fallback、mode/goal/plan、dialog、agents/jobs/tree、settings、queue、compaction 细节、ttsr、notice、customType 结构、turn telemetry |

判据的边界案例:compaction 的**分隔线条目**是 transcript 数据(message 轨,wire 投影),compaction 的**进度与死端警告**是 omp 原生(omp 轨);retry 的**loader**用 wire `session.status{retry}`(OpenCode 已定义该状态,R12/总纲 §7.6 复用裁决),retry 的**恢复注记与取代标注**用 omp 轨(`omp.retry.ended.retryErrors` / `omp.retry.started.supersededMessageID`,wire 无字段可放;§5.3.2);破坏性 tool part 回收 = P2 门控,不属 P1(§5.3.4)。

**REVISED(R1)—— 被废止的并行通道设计**:01 v1 §5.6 备选(c)「复用 wire `/event` 发 `omp.*`」、03 v1 §5.2 `ompchamber:omp-dialog-*`(全局广播器 SSE+WS fan-out)、04 v1 §5.0 `ompchamber:omp-agents/tree/jobs`(WireEventBus emit)一律废止;各章修订版只登记名称/payload 并引用本章(01/02 修订版已按此对齐)。`openchamber:` 前缀的 OpenChamber 原生合成事件(`ompchamber:notification` 等)不受影响——它们不是 omp 概念,不属本注册表。

#### 5.0.2 命名归一(R1/R3,REVISED)

统一 `omp.<域>.<事件>`(域名词 + 过去式动词),禁止 `ompchamber:omp-*` 与下划线事件名。v1 草案名的归一映射(各章修订已按此对齐):

| 原草案名(出处) | 归一名 |
|---|---|
| `omp.model.fallback {phase}`(01 v1) | `omp.fallback.applied` / `omp.fallback.succeeded` |
| `omp.session.model_changed` / `omp.session.thinking_level` / `omp.session.goal`(05 v1) | `omp.model.changed` / `omp.thinking.changed` / `omp.goal.updated`(01/02 定名) |
| `ompchamber:omp-dialog-requested/-settled`(03 v1) | `omp.dialog.requested` / `omp.dialog.settled` |
| `ompchamber:omp-agents/-tree/-jobs`(04 v1) | `omp.agents.updated` / `omp.tree.updated` / `omp.jobs.updated`(名从 04 修订版;04 v1 名违反 D1 命名规约) |
| `omp:queue_changed`(08 v1) | `omp.queue.changed` |
| `omp.session.compaction/retry/retry_fallback/ttsr/custom_message/notice/agent_settled/turn_usage`、`omp.resync`(05 v1) | `omp.compaction.started/ended`、`omp.retry.started/ended`、`omp.fallback.applied/succeeded`、`omp.ttsr.triggered`、`omp.custom.appended`、`omp.notice.raised`、`omp.session.settled`、`omp.usage.turn`、`omp.stream.resync` |

**envelope 归一**:payload 不携带 `directory`/`sessionID`(envelope 已带,§5.2.1);他章 payload 草图中的这两个字段移入 envelope,注册表 payload 列已省略,实现以 envelope 为准(重复字段视为冗余忽略)。

#### 5.0.3 事件注册表(唯一权威;列序 = SDK 源类型 → 公开名 → payload 概要 → producer → durable/volatile → 作用域 → 快照/refetch 端点 → UI reducer 备注)

> 路径遵守 R3(集合复数)。非 SDK 源事件的「SDK 源类型」列标 `—`(宿主自产)。

**A. 模型域(01 章)**:

| SDK 源类型 | 公开名 | payload 概要 | producer | d/v | 作用域 | 快照端点 | reducer 备注 |
|---|---|---|---|---|---|---|---|
| `model_changed`(payload-less) | `omp.model.changed` | `{model:{provider,id}, thinkingLevel, role?}` | `#handleEngineEvent` case(01 §5.6;registry meta.model 同步 + wire `session.updated` 同发) | durable | session | wire session snapshot + `GET /api/omp/models`(角色面) | useSessionModelStore 权威刷新;徽章真值 |
| `thinking_level_changed` | `omp.thinking.changed` | `{thinkingLevel, configured?, resolved?}` | 同上(01 §5.6;无 wire 动作) | durable | session | 无专用快照;engine transcript 恢复(restoreThinkingLevel)+ `GET /api/omp/models`(configured)兜底 | ThinkingPill 三态(01/02) |
| `retry_fallback_applied` | `omp.fallback.applied` | `{from, to, role}` | 同上(01 §5.6;不额外发 wire,随后 `model_changed` 承担,agent-session.ts:7271-7273) | durable | session | wire session snapshot | sessionModelStore `fallbackActive=true` + ⚠ 角标 |
| `retry_fallback_succeeded` | `omp.fallback.succeeded` | `{model, role}` | 同上(不回写 registry,§5.4) | durable | session | 同上 | 清 `fallbackActive`;status toast |

**B. 模式域(02 章)**:

| SDK 源类型 | 公开名 | payload 概要 | producer | d/v | 作用域 | 快照端点 | reducer 备注 |
|---|---|---|---|---|---|---|---|
| —(模式状态机) | `omp.mode.changed` | `{mode, data?}`(plan:{planFilePath}/loop:{state,remaining,limit}/prewalk:{target}) | engine enterMode/exitMode + `#materialize` 恢复投影(02 §5.4) | durable | session | `GET /api/omp/sessions/{id}/mode` | useSessionModeStore.modeBySession |
| `goal_updated` | `omp.goal.updated` | `{goal, state?}`(goals/state.ts 直通) | `#handleEngineEvent` case(02 §5.6) | durable | session | `GET /api/omp/sessions/{id}/goal`(mode 快照含 goal) | goalBySession;状态图标(02) |
| —(proposal 桥) | `omp.plan.review_requested` | `{details: PlanApprovalDetails}` | omp-host `xd://propose` handler(02 §5.5,`preparePlanForReview`) | durable(挂起审批不可丢) | session | `GET /api/omp/sessions/{id}/plan`(review? 字段) | planBySession + PlanReviewOverlay 拉起 |
| —(plan 落盘) | `omp.plan.updated` | `{planFilePath}` | engine plan 写入钩子(02 §5.5) | durable | session | 同上 | planBySession;Plan tab 刷新 |

**C. 审批域(03 章;生命周期归 03/R11)**:

| SDK 源类型 | 公开名 | payload 概要 | producer | d/v | 作用域 | 快照端点 | reducer 备注 |
|---|---|---|---|---|---|---|---|
| —(WebUIContext select/ask/confirm/input/editor) | `omp.dialog.requested` | `{dialog: OmpDialog}`(全量描述,03 §5.2;UI 无需二次取数) | omp-host PendingDialogRegistry(03 章) | durable(错过请求 = 错过审批) | directory | `GET /api/omp/dialogs?directory=` | useOmpDialogStore;`omp.dialog.settled` 前置 |
| —(respond/abort/超时/R11 shutdown settle) | `omp.dialog.settled` | `{dialogId, outcome:'responded'\|'cancelled'\|'timeout'\|'aborted'}` | 同上(03 章 R11) | durable | directory | 同上 | pending 集移除;双端竞答回滚(03 §5.6) |

**D. 实体域(04 章)**:

| SDK 源类型 | 公开名 | payload 概要 | producer | d/v | 作用域 | 快照端点 | reducer 备注 |
|---|---|---|---|---|---|---|---|
| —(registry onChange) | `omp.agents.updated` | `{agentRuns: OmpAgentRun[], revision}`(快照式,250ms 合并;R3 AgentRun 类型) | AgentRunsAggregator(04 §5.5) | durable(重放幂等,last-wins) | directory | `GET /api/omp/agent-runs?directory=`(R3) | useOmpAgentRunsStore 最后快照全量替换 |
| —(job 注册/落定钩子、async-result 抵达) | `omp.jobs.updated` | `{snapshot: OmpJobSnapshot}`(快照式;**仅 `capabilities.jobs=true` 时生产**,R12) | engine job 钩子(04 §5.6;UI 5s 轮询兜底) | durable(快照式) | directory+session | `GET /api/omp/jobs`(R3;形状归 04 章;capability 门控同 R12) | jobs 视图全量替换 |
| —(navigateTree/label) | `omp.tree.updated` | `{leafId, kind:'navigate'\|'label'\|'summary', entryId?}`(轻量 delta) | engine navigateTree/appendLabelChange(04 §5.4) | durable(仅作触发器) | directory+session | 04 章 getTree 快照(`GET /api/omp/sessions/{id}/tree`,§5.4.4 重拉语义) | 收到即整树重拉,不做增量 apply |

**E. 流内域(本章)**:

| SDK 源类型 | 公开名 | payload 概要 | producer | d/v | 作用域 | 快照端点 | reducer 备注 |
|---|---|---|---|---|---|---|---|
| `auto_compaction_start` | `omp.compaction.started` | `{reason, action}` | `#handleEngineEvent`(§5.1 行 1) | volatile | session | 无(进行态;产物权威 = wire 消息 + entries) | loaders[sid].compaction;超时自回收 |
| `auto_compaction_end` | `omp.compaction.ended` | `{action, aborted, willRetry, skipped?, errorMessage?, tokensBefore?, wireMessageID?}`(wireMessageID 预绑定消除 join 竞态,§5.5) | 同上(§5.1 行 2;tail-sync) | volatile | session | `GET /api/omp/sessions/{id}/entries?kinds=compaction` | 撤 loader;queued flush;divider 权威在 wire 消息 |
| `auto_retry_start` | `omp.retry.started` | `{attempt, maxAttempts, delayMs, errorMessage, supersededMessageID?}` | `#handleEngineEvent`(§5.3.2;P1 零 wire 突变,不发 removal) | volatile(loader 注记;权威 = wire `session.status{retry}`) | session | wire `/session/status` | loaders[sid].retry;supersededMessageID → 取代 overlay(dim/折叠,不改 transcript);R12 复用裁决 |
| `auto_retry_end` | `omp.retry.ended` | `{success, attempt, finalError?, retryErrors:[{messageID, note, retryRecovery}]}` | 同上(§5.3) | durable(恢复注记 = 持久展示态) | session | `GET /api/omp/sessions/{id}/entries?kinds=retry_recovery` | notes[messageID] chip;终端失败横幅 |
| `ttsr_triggered` | `omp.ttsr.triggered` | `{rules:[{name}]}` | `#handleEngineEvent`(§5.1 行 7) | volatile | session | 无(瞬时警告) | 逆流警告卡;连续合并;TTL 自回收 |
| `irc_message`(+tail-sync 兜底通道) | `omp.custom.appended` | `{message:{wireMessageID, customType, attribution, timestamp, text, details?, display}}` | `#handleEngineEvent`(§5.1 行 9)+ tail-sync(§5.5) | durable(wireMessageID join 幂等) | session | `GET /api/omp/sessions/{id}/custom-messages` | customDetails[wireMessageID] 注入;`display:false` 不建卡 |
| `notice` | `omp.notice.raised` | `{level:'info'\|'warning'\|'error', message, source?}` | `#handleEngineEvent`(§5.1 行 10;error 级保留 console.error) | volatile | directory(会话级 notice 带 sessionID) | 无(瞬时) | toast 三级;去重键 (level,source,message) |
| `agent_end {isTerminal:false}` | `omp.session.settled` | `{isTerminal:false}` | `#handleEngineEvent`(§5.7) | volatile(权威 = wire busy 保持) | session | wire `/session/status` | "等待后台交付"次级状态 |
| `message_end`(assistant) | `omp.usage.turn` | `{messageID, usage, ttftMs?, durationMs?, timestamp}` | finishAssistant 钩子(§5.9) | durable | session | `GET /api/omp/sessions/{id}/telemetry` | TurnUsageRow 数据源;tokens-only 降级 |

**F. 设置域(06 章)**:

| SDK 源类型 | 公开名 | payload 概要 | producer | d/v | 作用域 | 快照端点 | reducer 备注 |
|---|---|---|---|---|---|---|---|
| —(watcher / 本宿主 PUT 广播) | `omp.settings.updated` | `{revision, keys[], origin:'web'\|'external'}` | omp-host settings watcher + PUT 成功广播(06 §5.4) | durable | directory | `GET /api/omp/settings?directory=` | useOmpSettingsStore;external 仅重写未聚焦键 |

**G. 队列域(08 章)**:

| SDK 源类型 | 公开名 | payload 概要 | producer | d/v | 作用域 | 快照端点 | reducer 备注 |
|---|---|---|---|---|---|---|---|
| —(引擎 followUp/steer 队列变化) | `omp.queue.changed` | `{version}`(单调;仅作 refetch 触发器) | engine 队列钩子(08 §5.7 方案 B) | durable | session | `GET /api/omp/sessions/{id}/queue` | 渲染引擎快照;**写路径协议归 08 章**(requestId 幂等 + server ack + outbox,R12) |

**H. 控制面**:

| SDK 源类型 | 公开名 | payload 概要 | producer | d/v | 作用域 | 快照端点 | reducer 备注 |
|---|---|---|---|---|---|---|---|
| —(SSE 端点首帧,断流时) | `omp.stream.resync` | `{scope:[受影响域], lastEventId}` | `/api/omp/events` 端点(§5.2.1) | volatile(控制帧,不进环) | directory | §5.2.4 矩阵按 scope GET | 触发对账;不进业务 reducer |

**注册表治理规则**:
1. 新增/修改 omp 原生事件必须**先改本表**(spec 层)与代码镜像 `omp-event-registry.json`(§5.1.1 守卫 d 步),并 bump `eventSchema`(§5.2.3);他章只写「见 05 §5.0.3」,禁止重复定义 payload。
2. durable 判据:事件承载**可错过即状态错乱**的信息(审批、恢复注记、结构补充、revision 通知);volatile 判据:瞬时展示态(loader、toast、警告卡),错过由快照/超时自愈。
3. 快照端点是各域**权威**;事件永远只是增量通知(总纲 D2「断流不是空状态」)。
4. **durable 条目的快照端点必须出现在 §5.2.4 bootstrap 矩阵**(二轮 H5):registry 镜像 `snapshotEndpoints[]` ⊆ `omp-bootstrap-matrix.json`(CI 断言,§5.1.1 第 3e 步)。仅校验事件名 union 覆盖不充分——durable 事件缺快照端点 = gap 后无权威恢复步骤,直接违反 D2。

### 5.1 被丢弃事件的处置表(总纲 D2/D6-R6 逐类裁决;处置 P1 / 渲染 P2 分离)

| # | SDK 事件(payload) | 处置 | 事件形状 | UI 表面 | TUI 对照 | 处置/渲染 |
|---|---|---|---|---|---|---|
| 1 | `auto_compaction_start {reason, action}` | **omp 轨** | `omp.compaction.started {reason, action}`(volatile) | 会话底部 loader:"Auto context-full maintenance…(esc to cancel)" 等 4 种 action 文案;busy 状态保持 | event-controller.ts:1826-1859 | P1 / P1 |
| 2 | `auto_compaction_end {action, result?, aborted, willRetry, errorMessage?, skipped?}` | **双轨**:进度/警告走 omp 轨;产物条目走 wire 轨 | `omp.compaction.ended {action, aborted, willRetry, skipped?, errorMessage?, tokensBefore?, wireMessageID?}`(volatile)+ wire `message.updated/part.updated`(compactionSummary 投影,§5.5) | loader 撤除;transcript 插入可折叠分隔线;aborted/error 显示警告;queued 消息由 omp 事件触发 flush | event-controller.ts:1861-1940 | P1 / P1 |
| 3 | `auto_retry_start {attempt, maxAttempts, delayMs, errorMessage, errorId?}` | **双轨(P1 仅状态;回收 = P2 门控,二轮 H4/M10)** | wire `session.status {type:'retry', attempt, message:errorMessage, next:Date.now()+delayMs}` + `omp.retry.started {attempt, maxAttempts, delayMs, errorMessage, supersededMessageID?}`(volatile,注记用)。**P1 不产 `message.part.removed`**(§5.3.2);P2 仅对合成 settle tool parts 发 removal(§5.3.4 门 a/b/c) | working 区 loader "Retrying (n/max) in Xs…"(useAssistantStatus 已消费 `retryInfo`);msg_A 取代 overlay(dim/折叠);superseded 合成 tool 卡移除 = P2 | event-controller.ts:1942-1977 | P1 / P1(回收 P2) |
| 4 | `auto_retry_end {success, attempt, finalError?, retryErrors?}` | **双轨** | wire `session.status {type:'busy'}`(若 loop 继续;最终 `session.idle` 仍由 agent_end 发)+ `omp.retry.ended {success, attempt, finalError?, retryErrors:[{messageID, note, retryRecovery}]}`(durable) | loader 撤;恢复注记 chip 挂到 superseded 消息;终端失败置顶错误条 "Retry failed after N attempts: …" | event-controller.ts:1979-2034 | P1 / P1 |
| 5 | `retry_fallback_applied {from,to,role}` / `retry_fallback_succeeded {model,role}` | **双轨**:真值走 wire,提示走 omp | `omp.fallback.applied {from, to, role}` / `omp.fallback.succeeded {model, role}`;wire 真值刷新由随后 `model_changed` 承担(SDK 保证,agent-session.ts:7271-7273;§5.4) | warning toast "Fallback: A -> B" / status toast "Fallback succeeded on M";徽章 fallbackActive 角标 | event-controller.ts:2036-2046 | P1 / P1 |
| 6 | `model_changed`(无 payload,真值读 `session.model`)| **双轨**:真值走 wire | wire `session.updated`(registry model 字段同步,§5.4)+ `omp.model.changed {model:{provider,id}, thinkingLevel, role?}`(durable,01 §5.6 形状) | 会话徽章/模型指示即时换真值;与 fallback 联动防"静默换模型" | event-controller.ts:255-258;agent-session.ts:7257-7273 | P1 / P1 |
| 7 | `ttsr_triggered {rules}` | **omp 轨** | `omp.ttsr.triggered {rules:[{name}]}`(volatile;UI 端合并连续块) | transcript 尾部逆流警告卡 "⚠ Injecting rule: <name> ⟲",可折叠 | event-controller.ts:2048-2068 | **P1 / P2**(R6 拆分) |
| 8 | `todo_auto_clear` | **wire 轨** | `todo.updated {todos:[]}`(与 ACP 映射一致) | WorkStatusTasksSection/StatusRow 清空(live 置空,persist 兜底自然失活) | event-controller.ts:2074-2076 | P1 / P1 |
| 9 | `irc_message {message:CustomMessage}` | **双轨** | wire `message.updated`+`message.part.updated`(复用 `projectCustomMessage`,确定性 id,冷热一致)+ `omp.custom.appended {message:{wireMessageID, customType, attribution, timestamp, text, details?, display}}` | live irc 卡(不再等 refetch);冷读同 id 去重;TTL/上限语义见 §8.2 | event-controller.ts:856-914 | P1 / P1 |
| 10 | `notice {level, message, source?}` | **omp 轨**(error 级保留现有 console.error) | `omp.notice.raised {level, message, source?}`(volatile) | toast:error/warning/info 三级(复用 permission-toast 基建模式,sync-context.tsx:1527-1560);去重键 (level,source,message) | event-controller.ts:976-985;engine.js:607-610 | P1 / P1 |
| 11 | `thinking_level_changed {thinkingLevel?, configured?, resolved?}` | **omp 轨** | `omp.thinking.changed {thinkingLevel, configured?, resolved?}`(durable,01 定名) | composer 思考档位指示与 reasoning 折叠可见性同步(消费面 01/02 章) | event-controller.ts:259-283 | **P1 / P2**(R6 拆分) |
| 12 | `goal_updated {goal, state?}` | **omp 轨** | `omp.goal.updated {goal, state?}`(durable,02 定名) | goal 状态行/会话行图标(消费面 02 章) | interactive-mode.ts:1197(scout);segments.ts:232-236(scout) | **P1 / P2**(R6 拆分) |
| 13 | `tool_execution_update {toolCallId, partialResult}` | **wire 轨** | `message.part.updated`(tool part `state:{status:'running', input, output:partialText, metadata:{asyncState?}}`,§5.6) | 长任务(hub/后台任务)流式部分输出;ToolPart 已支持流式输出节流(ToolPart.tsx:1246-1248,scout) | event-controller.ts:1443-1468 | P1 / P1 |
| 14 | `agent_end {isTerminal:false}` | **wire 轨** | 不发 `session.idle`;发 `session.status {type:'busy'}` 保持 + `omp.session.settled {isTerminal:false}`(volatile) | 侧栏活动点保持;"等待后台结果"次级状态;**队列闸门保持关闭**(§5.7) | event-controller.ts:1705-1719,2200-2201 | P1 / P1 |

处置表落地后 `#handleEngineEvent` **不再依赖静默 `default:`**(D6-R6 明令禁止):switch 覆盖全部 24 个 union 成员(§5.1.1),末端 default 仅作 defense-in-depth —— 命中清单外类型时 `console.error` + 计数上报(dev 构建直接 throw),绝不静默返回;真正的防线是 §5.1.1 的 CI 守卫(SDK 新增事件未登记 → CI 失败)。

#### 5.1.1 全量覆盖清单与 CI 守卫(D6-R6 设计)

**覆盖清单(24 成员,锁定 SDK 版本生成)**:core `AgentEvent`(`pi-agent-core/src/types.ts:864-885`)10 个 + session 扩展(`session/agent-session-events.ts:12-64`)14 个:

| 成员 | 处置类 | 落点 |
|---|---|---|
| `agent_start` / `agent_end`(含 `isTerminal` 分支) | wire 既有 / 改 | engine.js:579 / :582-596;isTerminal 分支 §5.7 |
| `message_start` / `message_update` / `message_end` | wire 既有 | StreamProjector(projection.js:379-583) |
| `tool_execution_start` / `tool_execution_end` | wire 既有 | projection.js:507-560 |
| `tool_execution_update` | wire 新增 | §5.6 partial 路径 |
| `turn_start` / `turn_end` | **intentional-ignore(注释 case)** | 消息面由 message_*/tool_execution_* 承载,turn 边界是 TUI 内部概念,web 无消费面;case 体 = 理由注释 + return |
| `todo_reminder` | wire 既有 | engine.js:598-605 |
| `notice` | omp 升级 | 既有 error-console 分支(engine.js:607-610)升级为 §5.1 行 10 全量处置 |
| 其余 12 类 drop | §5.1 处置表行 1-12 | 本表即登记 |

**清单生成与 CI 守卫设计**:
1. `packages/web/server/lib/omp-host/event-dispositions.json`(committed manifest):`{ "<sdkType>": { "track": "wire-existing" | "wire-new" | "omp" | "dual" | "ignore", "note": "<理由,ignore 必填>" } }` —— §5.1 表与上述清单的机器可读镜像。
2. `omp-event-registry.json`(§5.0.3 的代码镜像):omp 公开名 + {durable, scope, snapshotEndpoints[], schema 自 vX.Y 引入} 元数据。snapshotEndpoints 用规范化标识(omp 端点 = 路径模板;wire 端点 = `wire:/session` 式前缀);volatile 条目可为空。
3. `scripts/check-event-coverage.mjs`(CI 步骤,`bun run check:events`):
   a. 解析锁定版 SDK 的 `.d.ts` union(pi-agent-core `AgentEvent` + pi-coding-agent `AgentSessionEvent`,取 `type: "…"` 字面量;`--sdk-dist` 指定 CI 检出路径);
   b. 断言 manifest 键集 == SDK 成员集 —— **SDK 新增事件未登记 → exit 1,CI 失败**;
   c. 断言 `engine.js #handleEngineEvent` 的 switch case 集 ⊆ manifest(ignore 成员也必须有显式 case);
   d. 断言 manifest 中 track 含 omp 的成员在 `omp-event-registry.json` 有对应条目。
   e. **durable→快照矩阵覆盖(二轮 H5)**:断言每个 `durable:true` 条目的 `snapshotEndpoints[]` ⊆ `omp-bootstrap-matrix.json`(§5.2.4 表的机器镜像,有序端点清单);任一缺失 → exit 1。只校验事件名 union 覆盖不算通过——durable 事件登记了却无 bootstrap 恢复步骤即违 D2。
4. `scripts/check-omp-event-names.mjs`(并入上脚本亦可):全仓(含 `docs/omp-parity/*` 与 `packages/`)grep `omp\.[a-z]+\.[a-z_]+` 的每个名字必须 ∈ `omp-event-registry.json`;`ompchamber:omp` 前缀零命中 —— 防并行命名回潮(R1)。
5. SDK 升级流程:bump 依赖 → CI 红 → 在本表 + manifest + 注册表登记处置并 bump `eventSchema`(§5.2.3)→ 绿。**禁止先合代码后补登记。**

### 5.2 GAP-E15/E16:omp 事件通道设计(REVISED,R1/R2/R3/R4/R12)

#### 5.2.1 服务端:总线、envelope、端点、鉴权、重放

- **总线**:把 `events.js` 的 `WireEventBus` 泛化为 `RingEventBus`(emit 增加 `durable: boolean`;重放环只收 durable 条目,volatile 只走实时订阅),wire 总线行为不变(全 durable,字节级回归)。新增 `OmpEventBus` 实例:`capacity 512`,`{directory, sessionID?}` 双键定向(§5.0.3 各条目的 d/v 与作用域列即 emit 参数)。
- **envelope(R1 最终形,payload 见 §5.0.3)**:

```jsonc
{ "id": 4821,                     // omp-host 进程内全局单调(跨目录单序列;进程重启归零,靠 resync 对账,不续播)
  "type": "omp.model.changed",    // 注册表公开名
  "directory": "<directoryKey>",  // 与 wire /event 同 directoryFromRequest 归一化
  "sessionID": "sess_…",          // 可空(directory 作用域事件省略)
  "schemaVersion": "1.0",         // §5.2.3
  "createdAt": 1724044800000,
  "payload": { /* 领域字段,不含 directory/sessionID */ } }
```

- **端点**(D6-R4:全部只注册在 omp-host `endpoints.js` 的 `/api/omp/*` 路由组,04 章 D04-1 同底座;web server 仅做既有 `/api` 透传代理,不新增路由。路径遵守 R3 复数规约):
  - `GET /api/omp/events?directory=<key>` —— SSE;`directory` 缺省 = 全目录。帧格式与 `endpoints.js:573` 相同(`id:`/`event:`/`data:` 三行 + 15s 心跳)。
  - `GET /api/omp/sessions/{id}/custom-messages?directory=` —— 结构化 customType 清单(冷):`[{wireMessageID, customType, timestamp, attribution, details}]`,只含 `display !== false` 且有 wire 投影的条目(wireMessageID 与 `[omp:]` 文本消息一一对应)。
  - `GET /api/omp/sessions/{id}/telemetry?directory=` —— per-turn 遥测(冷):`[{messageID, timestamp, input, output, cacheRead, cacheWrite, reasoningTokens?, totalTokens?, ttftMs?, durationMs?}]`(数据源:`AssistantMessage.usage/duration/ttft`,§5.9)。
  - `GET /api/omp/sessions/{id}/entries?directory=&kinds=compaction,branch_summary,model_change,mode_change,ttsr_injection,retry_recovery` —— 结构化会话条目(冷):`CompactionEntry{summary,tokensBefore,warning?}`(session-entries.ts:96-114)、`BranchSummaryEntry{fromId,summary}`(:116-124)、恢复注记等,供分隔线/注记冷读。
- **鉴权与穿透(R4,REVISED)**:omp-host 进程级 Basic auth(host.js:36-38)覆盖全部 `/api/omp/*`(v1 的「bearer 头/与 `/api/config/agents` 同层」表述作废);SSE 长连接与 `Last-Event-ID` 请求头必须穿透 web server 代理与 relay 隧道——直连/桌面/VS Code/relay 四通道的认证头转换归 R4 转换表,本章立验收项:普通 HTTP、SSE 建连、`Last-Event-ID` 续传三者在四通道全部通过(§7.2)。按 relay-transport skill,allowlisted `/api/*` 的 HTTP+SSE 天然过隧道;不为该通道开 WebSocket。
- **id 与重放(R1)**:`id` 为进程内全局单调单序列(不按 directory 分序列——`Last-Event-ID` 单值语义,跨目录客户端在前端过滤)。重连带 `Last-Event-ID` → 环内 durable 事件按序重放(**有界**:环容量 512,只保证最近 512 条 durable);volatile(loader、toast、ttsr、resync 帧)不重放——重放只会复活过期 loader,UI volatile 状态自带超时回收。
- **断流不是空状态(D2/R12,REVISED)**:`lastEventId` 早于环头(缺口)时,SSE 首帧发 `omp.stream.resync {scope:[受影响域], lastEventId}`,UI 按 §5.2.4 矩阵对受影响域执行权威 GET 对账——**绝不把 gap 当作"没有事件"**;受损域不可判定(如全目录断流)时按 §5.2.4 全序重跑。进程重启(id 归零,客户端 id 超前于环尾)同样触发 resync。
- **与 wire 轨的关系**:两条 SSE 并行订阅;wire `/event` 的 `Last-Event-ID` 环语义不变(events.js:46-58)。**顺序性边界**:跨轨无全局序。会话生命周期(busy/idle/retry/消息/todo)权威 = wire 轨;实体域(mode/goal/plan/dialog/agents/jobs/settings/queue/telemetry)权威 = §5.0.3 各域快照端点,omp 事件只是增量通知,任何缺口走 GET。UI 不得用 omp 事件驱动会话生命周期状态(v1「omp 轨只做增强」表述按此收窄:对本章流内域仍是增强,对 A–D/F/G 域是唯一的增量通知面,但权威永远在快照端点)。

#### 5.2.2 UI 端

- `packages/ui/src/sync/omp-event-pipeline.ts`(新):经 `runtimeFetch` 流式消费 `/api/omp/events`,按 directory 分发;未知 `type`(minor 版本新增,§5.2.3)忽略 + syncDebug 记录,不报错。重连策略复用 relay-transport skill 的 Reconnect 分支(指数退避、offline/hidden 长退避、4xx 长退避、可中断等待)。
- `omp-event-reducer.ts`(新)→ `useOmpSessionStore`(zustand,per-directory child):`{ loaders:{[sessionID]: {compaction?, retry?}}, notes:{[messageID]: retryNote}, customDetails:{[wireMessageID]: payload}, thinking?, goal?, modelFlash? }`。reducer 遵循 sync-state-invariants:同实体事件合并、无变化不写、volatile 状态带时间戳拒绝陈旧回放。
- 其余域 reducer 归各章(01 sessionModelStore、02 useSessionModeStore、03 useOmpDialogStore、04 useOmpAgentsStore、06 useOmpSettingsStore、08 queue store)——本章只定义通道、envelope 与注册表,不定义他章 store 内部结构。
- `RuntimeAPIs` 扩展(`packages/ui/src/lib/api/types.ts`):`OmpEventsAPI { subscribeEvents(directory|null, handlers): Subscription }`、`OmpSessionAPI { getCustomMessages / getTelemetry / getEntries }`、`OmpCapabilitiesAPI { getCapabilities() }`,各 runtime(web/desktop/vscode/mobile)同实现(runtimeFetch 已解耦);禁止组件自行拼 URL(R3)。

#### 5.2.3 capabilities 事件 schema 协商(R2,REVISED)

- `GET /api/omp/capabilities` → `{ version, eventSchema, features, minUiVersion }`(总纲 R2:端点组版本/feature 状态/最低 UI 版本由该端点统一承载;本章认领 `eventSchema` 与事件通道 feature)。
- **eventSchema 版本规则**:格式 `major.minor`。minor = 加法(新增事件类型、新增可选字段)——旧 UI 未知 `type` 忽略,兼容;major = 破坏(字段删除/语义变更)——引擎保留 N-1 双发一个版本窗口,UI 按 envelope `schemaVersion` 选择处理,过窗删除。注册表任何条目变更必须先 bump(§5.0.3 治理规则 1),CI 校验 manifest 与 `eventSchema` 声明同步(§5.1.1 第 3 步)。
- **三矩阵(R2)**:

| 矩阵 | 行为 |
|---|---|
| 新 UI + 旧 engine | capabilities 404/无 `eventSchema` → omp-event-pipeline 不启动,降级 wire-only(retryInfo/todo/partial 等 P1a 面仍可用,§6.1) |
| 旧 UI + 新 engine | 旧 UI 不订阅 omp 通道,零影响;误连时未知 `omp.*` type 被既有 event-pipeline 忽略(protocol 先例,runtime.test.js 消费侧过滤) |
| relay 旧 bundle | capabilities 探测失败/超时 → 同降级 wire-only;SSE 过隧道由 §5.2.1 R4 验收项保证 |

- 本地 feature flag(v1 的 `ompParityEvents`,06 章纳管方案)废止:事件通道与各域门控(01 `modelRoles.v1`、02 `modes.v1` 等)统一由 capabilities `features` 服务端裁决(R2)。

#### 5.2.4 bootstrap 与 resync 矩阵(R12,REVISED,GAP-E16)

**canonical bootstrap 顺序**(冷启动与全量对账共用;顺序固定,后步依赖前步):

| 序 | 域 | 端点 | generation/version 规则 |
|---|---|---|---|
| 1 | capabilities | `GET /api/omp/capabilities` | 无状态;决定后续各域是否可用(feature 门控,R2) |
| 2 | session snapshot | wire `/session` 列表 + `/session/{id}`(既有) | wire Session.time.updated 幂等替换 |
| 3 | modes / model | `GET /api/omp/sessions/{id}/mode`(含 goal/plan,02)+ **`GET /api/omp/models`**(角色面:roles/cycleOrder/enabledModels/fallbackChains/legacyDefaults,01 §5.3(1)/§5.5;二轮 H5 补);model 会话真值仍 = wire Session.model(本表序 2;与 01 §5.5 重连对账口径一致:session snapshot + `/api/omp/models`) | 全量替换,无版本号(小载荷幂等);roles 变更由 `omp.settings.updated`(序 5)revision 通知 |
| 4 | dialogs | `GET /api/omp/dialogs?directory=`(03) | 全量替换 pending 集(无版本,幂等) |
| 5 | settings(二轮 H5 补) | `GET /api/omp/settings?directory=`(06;注册表 F 行 `omp.settings.updated` 的权威恢复步骤) | revision 单调(事件携带 revision,跳变→GET);directory 作用域 |
| 6 | agents / jobs / queue / tree | `GET /api/omp/agent-runs?directory=`、`GET /api/omp/jobs`(04)、`GET /api/omp/sessions/{id}/queue`(08 方案 B)、`GET /api/omp/sessions/{id}/tree`(04;注册表 D 行 `omp.tree.updated` 的重拉目标,二轮 H5 补) | agents:revision 单调(事件携带 revision,跳变→GET);jobs:snapshot 全量替换;queue:version 单调;tree:事件即触发整树重拉(§5.0.3-D) |
| 7 | transcript 增量 | wire `/session/{id}/message` + 本章 `/api/omp/sessions/{id}/{custom-messages,telemetry,entries}` | wire 确定性 message id 序列;omp 侧按 wireMessageID/messageID join |

**resync 触发与范围**:任何事件 gap(§5.2.1 `omp.stream.resync` 帧、SSE 错误重建、目录切换、进程重启 id 归零)→ 对 `scope` 列出的域执行权威 GET;scope 不可信/全目录断流 → 按 1→7 全序重跑。**规则:事件只做增量通知,GET 才是权威;断流 ≠ 空状态(总纲 D2)。** 有 revision/version 的域可先比对再决定是否重拉(跳变才 GET);无版本域一律全量替换(载荷小,幂等)。

**机器校验(二轮 H5)**:本表镜像为 committed manifest `omp-bootstrap-matrix.json`(有序端点标识清单);CI 断言每个 durable 注册表条目的 `snapshotEndpoints[]` ⊆ 该 manifest(§5.1.1 第 3e 步)——durable 事件已登记却无 bootstrap 恢复步骤 = CI 失败。本轮补入的 settings/models/tree 三处缺口正是该检查要防的回归类别(只校验事件名覆盖抓不住它)。

**queue 专项(R12)**:`omp.queue.changed {version}` 只触发 `GET /api/omp/sessions/{id}/queue` refetch;**写入路径协议归 08 章**——requestId 幂等 + server ack + outbox(本地队列项仅在收到 ack 后出箱,失败保留 outbox 重试);方案 B 以该协议完备 + queue 纳入本矩阵为启用门槛(R12 明文)。

### 5.3 GAP-E02:auto_retry 的 loader、取代标注与消息回收(REVISED-2,二轮 H4/M10:P1/P2 拆分)

#### 5.3.1 语义基线与二轮修正

TUI 的回收语义(event-controller.ts:1942-1955)是规格:**"retry 取代刚失败的 turn:其 assistant + tool calls 从上下文中剪除并重流。移除 message_end 以合成 aborted/error 完成 settle 的卡片,使 retry 的新卡不重复渲染同一调用(#6879)。只有未 commit 的卡可移除;已上 scrollback 磁带的留作历史。"** 实现上可回收集 = `#syntheticFailureCards` 登记表 ∧ `isBlockUncommitted()` —— 即**仅为 settle 在途 tool call 而合成的失败卡**;已 commit 的块(带真实 result 的工具证据)永不回收。

v2 设计把回收对象扩大为"失败消息的全部 tool parts"(`message.part.removed`×N 无条件),会删除用户已看过的 committed 工具证据,超出 TUI 语义;且 07 §5.8 记录的现 reducer 行为——最后一个 part 被移除即删除消息壳(event-reducer.ts:457-473,空数组分支 :465-466)——与 v2 §5.3.2 的"壳保留"假设直接冲突。二轮评审(H4/M10)裁决**拆分**:

- **P1(随 P1a/P1b)**:retry loader/status + `omp.retry.*` 取代 overlay —— **零 wire 突变,`message.part.removed` producer 保持零**(§5.3.2)。
- **P2(门控)**:经验证的合成 tool part 回收;门 a/b/c(§5.3.4)全绿前 producer 不投产。

Web 侧"合成 settle"的精确对应物:`finishAssistant` 时 `projectAssistantMessage` 对**无 result 的 toolCall block** 合成的 settle part(projection.js:254-269:aborted → error 'Aborted',其余 → 空 output 'completed');**有 result 的 part(isError/completed,projection.js:270-292)= committed 证据,永不回收**。

#### 5.3.2 P1:retry 状态与取代 overlay(零 wire 突变)

```
[失败尝试流式]  message.updated(msg_A) + part.updated(text/reasoning/tool×n)
                message_end(stopReason:'error'|'aborted') → finishAssistant → msg_A error 态落定
auto_retry_start:
  1. wire session.status {type:'retry', attempt, message:errorMessage, next:Date.now()+delayMs}
     // 现有形状;useAssistantStatus.ts:343-349 已渲染 retryInfo;队列闸门因 'retry'≠'idle' 关闭
  2. omp  omp.retry.started {attempt, maxAttempts, delayMs, errorMessage, supersededMessageID?}
     // supersededMessageID = projector 刚落定的 msg_A wire id(finishAssistant 返回值,engine 已持有)
     // UI 据此把 msg_A 标注 superseded:展示层 dim/折叠 + "↻ 重试中,已被取代" 徽标
     // —— 纯 overlay:不改 wire transcript、不发任何 removal
  // P1 明确不产 wire message.part.removed(producer 保持零;07 守卫不受影响)
[SDK 丢弃失败尝试、重流]
attempt-2: message_start → 新 StreamProjector → msg_B(新 timestamp → 新确定性 id,无 id 冲突)
auto_retry_end:
  3. wire session.status {type:'busy'}(loop 继续;最终 session.idle 仍由 agent_end 发,engine.js:595)
  4. omp  omp.retry.ended {success, attempt, finalError?,
       retryErrors:[{messageID:<msg_A 的 wire id>, note, retryRecovery}]}
```

- **P1 的诚实语义**:重影不消除——msg_A 与 msg_B 都在 wire transcript 里,但 msg_A 被明确标注取代 + 恢复注记。§2.4-1 的可见性缺陷修复;重复观感由 overlay 缓解,彻底去重延后 P2。
- **恢复注记**:`auto_retry_end.retryErrors` 的 `persistenceKey` 是 SDK entry 键;engine 换算成 msg_A 的确定性 wire id 随 omp 事件下发。UI 在 msg_A 尾部渲染 dim 单行 chip(如 "↻ recovered (credential) · <note>");冷读经 `entries?kinds=retry_recovery` 拿同一份数据(持久化展示状态,pi-ai types.ts:857-882 注释 "Persisted presentation state")。
- **终端失败**:success=false → UI 置顶错误横幅(对齐 :2021-2031),由 `omp.retry.ended` 驱动。
- **R12/总纲 §7.6 复用裁决(P1 半边)**:`session.status{type:'retry'}` 复用现有 wire SessionStatus retry 形状承载 auto_retry,07 章 A9 从「删除」改「保留」——P1a 即点亮。

#### 5.3.3 冷热一致性规则(P1 形态)

- **live 会话**(读 `live.agentSession.messages`,engine.js:372):SDK 已把 superseded 失败消息从 runtime context 剪除(event-controller.ts:1958-1960 注释 "The retry path drops the failed assistant from runtime context")→ 冷重读后 msg_A 消失。P1 无 removal 事件 → live wire 流(含 msg_A)与冷重读(无 msg_A)存在**既存偏差**(重影在重读后消失,与今日行为一致);P1 接受该偏差并以 overlay 标注缓解,不与它对抗(对抗 = P2 回收,§5.3.4)。
- **文件冷读**(`transcript:true`,engine.js:383):完整 transcript 保留 superseded 尝试及其 `retryRecovery` 注记(TUI 全量导出同理,session-history-format.ts:263-266,424-430 处理隐藏/主上下文区分)。投影规则:`projectAssistantMessage` 读到 `retryRecovery.status==='recovered'` 时,tool parts 默认折叠(消息级 collapsed 标记),文本壳保留 + 注记。**open question 见 §8-3。**
- 测试锚点:P1 下重试场景跑完后,live 事件流内 **`message.part.removed` 零出现**;live 重投影与文件冷投影的 id/part 集合按上述折叠规则断言(扩展 omp-host.test.js:119-122 的确定性测试模式)。

#### 5.3.4 P2:经验证的合成 tool part 回收(门 a/b/c,三门全绿方可投产)

**回收范围(对齐 TUI `#syntheticFailureCards` ∧ `isBlockUncommitted`)**:仅 msg_A 中由 `!result` 分支合成 settle 的 tool parts(projection.js:254-269);文本/推理 part、有 result 的 tool part(含 isError,projection.js:270-292)、消息壳一律保留。三门:

- **门 a —— 可回收标记(服务端能力)**:投影侧记账——`StreamProjector.finishAssistant` 在 stopReason ∈ {error, aborted} 时记录 `syntheticSettledPartIds`(本次落定消息中 `!result` 分支产出的 part id 集合;projector 已有 per-callID 的 `toolPartIds` 台账,projection.js:391,509-511);`auto_retry_start` 到达时**仅**对这些 id 逐个发 `wire message.part.removed {sessionID, messageID, partID}`。上游显式标记(AssistantMessage content 现无 synthetic/uncommitted 字段)若未来提供则优先(§8-10)。无标记可判的 part 宁可不回收——保守取舍:**重影优于删证据**。
- **门 b —— 壳保留不变式(UI reducer 硬前置修改)**:现 reducer `event-reducer.ts:457-473` 按 partID splice 后**空则删消息 parts**(:465-466)——必须先改为"最后一个 part 被移除也**永不删壳**"(空消息渲染为 error 态壳/注记宿主)。该修改独立有价值(防御任何 removal 生产者的壳误删)。**协调 07 章**:其 §5.8 引用的"05 §7-2 测试断言(壳无 `message.removed`)"需按本章 v3 §7.1-2 升级为"全部 part 移除后消息壳仍存在";`message.removed` 删除裁决不变、与本修改互不冲突。
- **门 c —— 三类测试(缺一不启用 producer)**:
  1. **tool-part-only 消息**:msg_A 仅含合成 tool part(s) → 全部移除后壳保留(error 态、零 part);
  2. **last-part**:文本 + 合成 tool part → tool part 移除后文本壳完整;
  3. **已显示/已锚定**:UI 已渲染 part 收到 removal → 幂等 splice、无重复删除、无 crash、无 dangling 引用;committed part(有 result)**收不到** removal(producer 侧断言:门 a 集合 == removal 事件集)。
- **冷热对账(P2 形态)**:回收后 live wire 流(msg_A 壳 + 文本,tool parts 缺)与 live 重读(SDK 已剪除 msg_A)仍非逐条相等——壳保留是**良性多显**(与 TUI committed-row 语义同源:已渲染的历史不抹除);文件冷读保留全量 + 折叠投影。断言:removal 恰好覆盖 `syntheticSettledPartIds`;壳与文本 part 在两条投影路径均存在。
- **wire 复用裁决(D1 唯一例外 + D6-R5 + 总纲 §7 写作期发现 6,REVISED-2)**:`message.part.removed` 首产从 P1a **延后至 P2 门控**;R5 的"守卫与 DAG 移除该键、reducer 保留"裁决不变(07 章守卫继续放行,仅生产时点后移)。`message.removed` 仍按残留删除(07 章对账)。

#### 5.3.5 备选方案与取舍(REVISED-2)

| 方案 | 内容 | 取舍 |
|---|---|---|
| **B(采纳,拆两期)** P1 overlay + P2 门控回收 | §5.3.2 / §5.3.4 | P1 零风险先修可见性;P2 以"合成 settle"精确边界 + 壳不变式 + 三类测试收窄破坏面。代价:P1 期间重影仍在(已被标注) |
| A 整消息回收 | `auto_retry_start` 时对 msg_A 发 `message.removed` | 丢弃错误细节;文件冷读(transcript:true)会重新出现 → 冷热不一致更严重;`message.removed` 是 07 章删除项。否决 |
| C 不回收,仅靠幂等 part 覆盖 | retry 用同一 wire id 重流覆盖 | 破坏确定性 id 契约(projection.js:3-11:live 流式与冷重投影必须同 id;retry 是新的 AssistantMessage 新 timestamp)。否决 |
| D(v2 原案)无条件回收失败消息全部 tool parts | ~~v2 §5.3.2 步骤 2~~ | 扩大 TUI 语义(会删 committed 证据);与现 reducer 删壳行为冲突;无已锚定防护。二轮 H4/M10 否决 |

### 5.4 GAP-E03:模型徽章真值

- `model_changed`(事件无 payload,真值读 `session.model`):engine → `registry.update(dir, sid, {model})` + `bus.emit('session.updated', …)`(engine.js:594 已有同型发射,复用)+ `omp.model.changed {model:{provider,id}, thinkingLevel, role?}`(durable;01 §5.6 形状,role 取 `getLastModelChangeRole`)。
- `retry_fallback_applied`:engine 同步 registry(`to`)+ 发 `omp.fallback.applied {from, to, role}`;**不直接发 wire `session.updated`**——SDK 保证 fallback 路径随后发出 `model_changed`(`#setModelWithProviderSessionReset`,agent-session.ts:7271-7273),wire 真值刷新由该 case 承担(避免双发;与 01 章修订版同裁决)。后续 `msg_B.info.modelID/providerID` 天然带真值(finishAssistant 读 message.model,projection.js:203)→ `ChatMessage.tsx:281-363` 徽章自愈。
- `retry_fallback_succeeded`:不回写 registry(成功发生在 fallback 模型上,回退策略属 SDK;TUI 只 showStatus)。仅 `omp.fallback.succeeded {model, role}` + toast。
- 边界:与 01 章的接口——registry `model` 字段最终语义迁移到 role 体系(01 章);本章只保证"session 元数据与实际执行模型一致"这一不变量。

### 5.5 GAP-E04:compaction 进度与分隔线条目

- **进度**(omp 轨):`omp.compaction.started/ended` 驱动 loader;`aborted`/`errorMessage`/`warning` 分支提示;**compaction 期间排队的消息 flush** 由 ended 触发(对齐 TUI `flushCompactionQueue({willRetry})`,event-controller.ts:1937)——OpenChamber 的 messageQueueStore 派发闸门当前只认 idle/busy;在 omp store 暂存 compaction 中的 session,`resolveQueuedSessionStatusType`(useQueuedMessageAutoSend.ts:192-208)把该状态视作 busy,flush 时机改由 compaction ended + idle 共同决定。错位防御:两事件均 volatile 不重放,断流错过 ended 的会话由 idle 边沿 + transcript 快照自然收敛(queued 消息不丢,仅延迟)。
- **分隔线条目**(wire 轨):`projectConversation` 扩展 `compactionSummary`/`branchSummary` 角色 → 复用 `projectCustomMessage` 通道生成合成 assistant 消息(`[omp:compactionSummary] <summary>` / `[omp:branchSummary] <summary>`),归类 T2 折叠分隔线渲染(§5.8);结构化元数据(`tokensBefore`、`warning`、`fromId`)经 `entries` 端点补充。
- **live 插入**:`auto_compaction_end` 后 SDK 将 summary 物化为消息;engine 执行 **tail-sync**——对比 `session.messages` 尾部与已发射 wire 消息 id,把未发射的 `custom/compactionSummary/branchSummary` 逐条投影发射,并把该合成消息的 wire id 预绑定进 `omp.compaction.ended.wireMessageID`(消除 divider join 竞态,§8 OQ-4 已采纳)。该机制同时修复 B7 类"只在 refetch 后出现"的通病(扩展注入的 custom 消息无专属事件时,agent_end/compaction_end 的 tail-sync 兜底)。

### 5.6 GAP-E09:tool_execution_update(部分结果)

- `StreamProjector` 新增 `toolPartial(callID, { text, asyncState })`:仅当 part 已存在(running 态)时发射 `message.part.updated`,state 保持 `status:'running'`,追加 `output`(部分文本)与 `metadata.asyncState`(hub 后台任务状态)。**不设终态**:终端归属 `tool_execution_end`(对齐 event-controller.ts:1450-1456 的注释——final async snapshot 对 parked 后台块才是终端;engine 无法可靠复刻该判据时,一律非终端,由 end 收口,语义保守正确)。
- engine case:`case 'tool_execution_update': hostSession.projector?.toolPartial(event.toolCallId, …)`。`partialResult.details.async.state` 透传为 `metadata.asyncState`(TUI 同源字段,:1449)。
- reducer 侧零改动:`message.part.updated` 按索引替换(event-reducer.ts:425-433);ToolPart 流式输出已有节流渲染路径(scout ToolPart.tsx:1246-1248)。

### 5.7 GAP-E10:`agent_end.isTerminal === false`(过早 idle)

- engine:`case 'agent_end'` 检查 `event.isTerminal === false`:
  - 仍执行 projector 收口与 `session.updated`(engine.js:582-594 现有逻辑);
  - **跳过 `session.idle`**,改发 `session.status {type:'busy'}`(保持;幂等,areSessionStatusesEqual 判等跳过,event-reducer.ts:325-331)+ `omp.session.settled {isTerminal:false}`(volatile,UI 显示"等待后台交付"次级状态);
  - 后续 async 交付唤醒 → `agent_start` → busy(正常);真正终局 `agent_end`(isTerminal≠false)→ `session.idle`。
- **闸门正确性**:session_status 停留 busy → 队列不派发(useQueuedMessageAutoSend.ts:170-172 要求 idle 边沿);`resolveQueuedSessionStatusType` 的 in-flight 兜底(:192-208)也不会误判——trailing assistant 已 completed。输入框允许输入并发送(TUI 同语义:命令可立即挂载,event-controller.ts:1716-1719)——prompt 走 steer/followUp 与 SDK 的 async 唤醒共存(engine.js:690-695 已实现 steer 交付)。
- **快照一致**:`/session/status` 轮询快照(global-session-status.ts:110-115 消费)必须把 awaiting-async 会话报告为 busy——engine 维护 per-session `awaitingAsync` 标记,agent_start/terminal agent_end/超时(如 10min 无唤醒)清除,防快照把 busy 降级为 idle(monotonic 快照不降级是 UI 侧既有防线,服务端仍应以真值为准)。

### 5.8 GAP-E11:customType 分层渲染

#### 5.8.1 数据透传决策(wire part 字段 vs RuntimeAPIs)

wire Text part 没有 customType 字段,且 D1 禁止扩类型。三个候选:

| 方案 | 内容 | 取舍 |
|---|---|---|
| **采纳:前缀即契约 + omp 轨补结构** | `[omp:<type>] ` 前缀由我们自己的投影写入(projection.js:161),UI 解析同一前缀完成**分层分类**(零服务端往返);需要结构化 `details/attribution` 的 T1 卡片再经 `omp.custom.appended` live 事件或 `/api/omp/sessions/{id}/custom-messages` 冷读补齐(wireMessageID join) | 前缀是我们自有投影格式,非外部契约,round-trip 合法;wire 兼容消费者(导出、旧客户端)仍得到可读文本 |
| 否决:wire part 加字段 | 手改 vendored gen | 违反 D1 |
| 否决:全量走 RuntimeAPIs | 分层也走 fetch | 冷读/弱网时整层不渲染;且 90% 类型只需分类不需要结构 |

#### 5.8.2 渲染分层与映射表

| 层 | 判据 | customType 清单 | 渲染 | 数据源 |
|---|---|---|---|---|
| **T1 可见卡片** | 需独立视觉身份 | `advisor`、`irc:incoming`、`irc:autoreply`、`irc:relay`、`async-result`、`skill-prompt`、`lsp-late-diagnostic`、`live-delegation`、`collab-prompt`、`background-tan-dispatch` | 专属 React 卡组件(advisor 卡带 severity 轨、irc 卡带作者/时间、async-result 带 jobId 等) | 文本(前缀剥离)+ omp 结构 details |
| **T2 折叠分隔线** | 历史折叠点 | `compactionSummary`、`branchSummary`、handoff 类 custom(对齐 compaction-summary-message.ts:122-126) | slim divider,默认折叠,展开见 summary;warning 徽标 | 文本 + `entries` 端点(tokensBefore/warning/fromId) |
| **T3 隐藏** | `display:false`(投影已丢弃,projection.js:361) | `eager-todo-prelude`、`mid-run-todo-nudge`、`todo-error-reminder`、`ultrathink-notice`、`orchestrate-notice`、`workflow-notice`、`prewalk-*`、`plan-mode-*`、`goal-mode-context`、`vibe-mode-context`、`checkpoint-active-reminder`、`interrupted-thinking`、`resolve-reminder`、`tool-call-loop-redirect`、`thinking-loop-redirect`、`image-attachment-description`、`ttsr-injection`、`goal-continuation`、`session-stop-continuation`、`gemini-tool-call-reminder` 等 | **不渲染**(现状保持) | 无 |
| **T4 兜底** | 未登记/扩展注册类型 | 其余全部(含扩展 `sendMessage` 任意 customType) | 现有 `[omp:<type>]` 文本卡 + 边框样式(对齐 TUI message-frame 兜底) | 文本 |

- **T3 的"隐藏但不重显"规则**:live `omp.custom.appended` 带 `display:false` 一律不建卡(仅 syncDebug 日志);重连重放、tail-sync、冷读三条路径统一按 `display:false` 过滤——与现状冷投影过滤(projection.js:361)形成双保险。
- **T1/T2 live 事件**:`omp.custom.appended` 到达时若同 `wireMessageID` 的 wire 消息已渲染,只注入结构 details,不重复建卡(wire 轨才是消息权威)。
- 分层表是 UI 端常量(`customTypeTiers.ts`),未知类型默认 T4;类型迁移期无需服务端配合。

### 5.9 GAP-E12/E13:usage row 与 cache-miss 分隔线

- **live**:engine 在 `message_end`(assistant)时发 `omp.usage.turn {messageID, usage, ttftMs?, durationMs?, timestamp}`(durable)。数据在 `finishAssistant` 的入参 AssistantMessage 上现成(pi-ai types.ts:917,937-939)。
- **冷读/缺口**:`/api/omp/sessions/{id}/telemetry` 端点同构;`omp.stream.resync` 对账(§5.2.4)后按 scope 补拉。
- **UI**:`TurnUsageRow` 挂在每个 assistant turn 尾部(turn grouping 已有 isLastAssistantInTurn 概念,MessageBody.tsx:1306-1308)。格式对齐 `formatUsageRow`(usage-row.ts:19-42):`HH:MM:SS ↑in(input+cacheWrite) ↓out ⛁cacheRead(>0) ⏱ttft(>0,×0.1s) ⚡tok/s(duration>100ms 且 output>0)`。tokens 主数据可用 wire `info.tokens`(projection.js:319-320)——即使 omp 通道未开,降级渲染 tokens-only 行。
- **cost 不进 usage row**:per-message cost SDK 不报(projectUsage 注释 projection.js:81-84;`Usage` 无 cost 字段,pi-catalog types.ts:95-135);会话级 $cost 归 08 章 usage-report 聚合。明示降级,不造假数。
- **cache-invalidation**:移植 `detectCacheInvalidation`(cache-invalidation-marker.ts:49-66)为 UI 纯函数,输入 = 相邻两个 assistant turn 的 `info.tokens`(cache.read/write/input 全部已投影,projection.js:76-79)——**零服务端改造**。规则原样保留:MIN_CACHE_FOOTPRINT 2048、仅显式缓存(cacheWrite>0)、只标 warm→cold 跃迁。渲染 `────── ⊘ cache miss · N tokens` divider 于 turn 上沿。

### 5.10 GAP-E14:transcript 角色投影扩展

`projectConversation`(projection.js:349-369)新增角色投影(全部走合成消息 + 前缀分类,消息 id 确定性规则不变):

| 角色 | 投影 | 渲染 | TUI 对照 |
|---|---|---|---|
| `bashExecution {command, output, exitCode, cancelled}` | user 侧合成消息,文本 part `[omp:bash] $ <command>` + 折叠输出块 | 细行执行卡(复用 shellAction 分类渲染,MessageBody 1859-1935 已有该 part 处理路径) | messages.ts:237-257;TUI `!/!!` 执行角色 |
| `pythonExecution {code, output, exitCode}` | 同上,`[omp:python]` | 同上 | messages.ts:258-278;`$/$$` |
| `fileMention {files[]}` | user 侧合成消息,每文件一行 `[omp:file-mention] └ Read <path> (N lines)` | 细引用行组 | messages.ts:294-302;TUI fileMention 行 |
| `hookMessage` | assistant 侧合成消息 `[omp:hook]`(T4 边框卡) | glyph 折叠卡 | messages.ts:280-285 |
| `compactionSummary`/`branchSummary` | §5.5(T2) | 折叠分隔线 | messages.ts:286-293 |

- 合成 user 消息以 `parentID=''` 独立成段,turn grouping(projectTurnRecords)按前缀分类跳过 turn 锚定,不干扰 user→assistant 配对(`projectCustomMessage` 已用同一技巧让 note 骑乘 parentID,projection.js:170-172)。
- 备选(否决):折叠进前一 user 消息的 additionalParts——交错出现在 assistant 后(bash 执行可以跟在回答后)时会错锚,且改变既有 part id 序列破坏冷热确定性。

### 5.11 wire `session.error` 终局(二轮 M7 裁决:不生产)

**裁决:omp-host 不生产 wire `session.error`**(types.gen.d.ts:997;生产者基线见 07 §2.1——本裁决使其保持零生产者,07 章 G08 HOLD 的 error 项就此解锁整链删除)。引擎侧错误经既有/本章设计的三条路径沉降,无需该事件:

1. **turn 级失败展示**:msg error 态(`projectAssistantMessage` 的 `info.error`,projection.js:305-312)+ tool part error 态(projection.js:270-279)——wire 消息面已承载;
2. **错误提示**:`omp.notice.raised {level:'error'}`(§5.1 行 10;error 级同时保留 server console.error,engine.js:607-610)+ `omp.retry.ended {success:false}` 终端失败横幅(§5.3.2);
3. **终态/flush 权威**:wire `session.idle`(terminal `agent_end` 发,engine.js:582-596);完成/失败系统通知的唯一权威 = terminal `agent_end`(stopReason),notice 仅即时 toast——与 08 章错误通知去重(二轮 M9)同口径。

**07 章删除替换清单(四处同步,删除须带 event-pipeline 终态单测回归)**:

| 现消费点 | 替换 |
|---|---|
| `event-pipeline.ts:491-496`(终态判定列表含 `session.error`) | 删该项;终态 = `session.idle`/`session.created`/`session.deleted`(`session.idle` 为 flush/收口权威) |
| `session-event-router.ts:67-69`(error 触发 flush) | flush 终态由 `session.idle` 承担(既有分支保留) |
| `sync-context.tsx:1604-1621`(error 通知生成) | 替换为 `omp.notice.raised{level:'error'}` toast(§5.1 行 10)+ `omp.retry.ended{success:false}` 横幅(§5.3.2)+ terminal `agent_end` 通知权威(M9 口径) |
| `event-reducer.ts:344-352`(error 复位 idle) | 由 `session.idle` case(:334-342)承担,直接删 case |

---

## 6. 迁移与兼容

### 6.1 分期(对应总纲 D4 P1/P2)

| 期 | 内容 | 特征 |
|---|---|---|
| **P1a(纯 wire,无新依赖)** | **全部 24 成员显式 case + manifest + CI 守卫(§5.1.1;含 ttsr/thinking/goal 的 case——emit 到 `OmpEventBus`,通道未开时为空操作)**;处置表行 3/4/13 的 wire 侧与行 5 的 wire 联动;`session.status{retry}` 首产(总纲 §7.6;**`message.part.removed` 不在 P1a——二轮 H4/M10 延后 P2 门控,§5.3.4**) | UI 零新组件即点亮(retryInfo、队列闸门、部分结果、todo 清空);旧行为=新事件不发的退化 |
| **P1b(omp 通道)** | `/api/omp/events` + `GET /api/omp/capabilities`(eventSchema 1.0)+ omp-event-pipeline/reducer/store;注册表 A–H 组事件点亮;tail-sync;`omp.session.settled`;retry 取代 overlay(`omp.retry.started{supersededMessageID}`,§5.3.2);bootstrap/resync 矩阵 + `omp-bootstrap-matrix.json` 机器校验(§5.2.4) | 门控 = capabilities(R2;v1 草案的本地 flag `ompParityEvents` 废止);无 capabilities/无通道 → 自动降级 wire-only |
| **P2** | T1 卡片层、T2 分隔线、usage row + telemetry、cache-miss divider、ttsr/thinking/goal 的**完整渲染**(D6-R6:处置已在 P1a 落)、transcript 角色投影扩展;**auto_retry 合成 tool part 回收**(§5.3.4 门 a/b/c 全绿后投产;reducer 壳保留不变式随本项落地) | 均为加法(retraction 附带 reducer 壳不变式修改,见 §6.2);T4 兜底保证未覆盖类型不回归 |

### 6.2 兼容性要点

- **wire 契约零变更(P1)**:不新增事件类型、不修改 gen;`session.status{retry}` 是契约既有成员的**点亮**(types.gen.d.ts:510-527;总纲 §7.6),旧 UI 已能 reduce。`message.part.removed`(types.gen.d.ts:615-637;R5)点亮延后至 P2 门控(§5.3.4)——届时附带 UI reducer 壳保留不变式修改(`event-reducer.ts:457-473` 的"空则删消息"(:465-466)→"永不删壳",正是门 b 要修的对象,须与 producer 同批发布);`message.removed`、`session.error` 不点亮(前者 07 章照删;后者 §5.11 终局,零生产者保持)。
- **存量会话数据**:冷投影新增合成消息(compaction 分隔线、执行卡等)会在**重读时出现**——消息 id 确定性 ⇒ 同一会话重复读取结果稳定;UI message store 是会话级缓存,无迁移;`[omp:]` 前缀消息在旧 UI 中本来就以文本显示,新 UI 只是换渲染,无数据损坏面。
- **并发会话/多目录**:omp 总线沿用 directory 定向(events.js:5,29);`/api/omp/events?directory=` 与 wire `/event` 同 scoping 语义;多目录客户端(桌面)可订阅全局流自行过滤。
- **回滚**:P1a 的每个 case 均可独立 revert(行为回退为"不可见",与今日一致);omp 通道回滚 = omp-host 撤下 capabilities 的 eventSchema/feature key,UI 按 §5.2.3 矩阵自动降级 wire-only;`RingEventBus` 泛化保持 wire 总线字节级行为不变(重放环语义、id 单调)。
- **relay/移动端**:SSE 过隧道已验证模式(relay-transport skill;认证头按 R4 转换表);不得为 omp 通道引入 WebSocket(否则触发 WS allowlist/URL-token 全套新面);relay 旧 bundle 按 §5.2.3 矩阵降级。

---

## 7. 验证方案

### 7.1 单元/集成(omp-host,bun:test)

1. **处置表穷举**:`#handleEngineEvent` 对 15 个 drop 成员逐类喂合成 `AgentSessionEvent`(§5.1.1 清单全集),断言 wire/omp 发射序列(含 `session.status{retry}` 字段类型校验——`next/attempt/message` 必须为 number/number/string,sync-context.tsx:535-547 的校验形状);`turn_start`/`turn_end` 命中注释 ignore case。
2. **auto_retry 取代与回收(P1/P2 分离,二轮 H4/M10)**:
   - **P1**:流式一个失败 turn(tool×2)→ `auto_retry_start` → 断言 wire `session.status{retry}` + `omp.retry.started{supersededMessageID}`,且 **`message.part.removed` 零发射**;`auto_retry_end` → 恢复注记按 messageID 注入;重放(re-subscribe since 0)后 reducer 幂等。
   - **P2(门 c,producer 启用前必须全绿)**:失败 turn(tool×2:1 合成 settle + 1 带 result)→ removal 恰好 ×1 且仅命中合成 part(门 a 集合 == removal 事件集);**tool-part-only 消息**全部 part 移除后**壳保留**(error 态、零 part);**last-part** 移除后文本壳完整;**已显示/已锚定** part 的 removal 幂等 splice、无 crash、committed part 零 removal;重放幂等;全程无 `message.removed`(07 §5.8 对本断言的引用按 v3 对账)。
3. **冷热对称**:重试/compaction/irc 场景跑完 live 投影后,同会话走 `live.agentSession.messages` 重投影与文件冷投影(`transcript:true`),按 §5.3.3(重试折叠规则,P1 形态)/§5.5 规则断言 id/part 集合;P2 启用后追加"removal 恰好覆盖 `syntheticSettledPartIds`,壳与文本 part 两路均在"(§5.3.4)(扩展 omp-host.test.js:119-147 模式)。
4. **isTerminal**:`agent_end{isTerminal:false}` → 无 `session.idle`、有 busy 保持;`agent_start` 恢复;terminal end → idle;`awaitingAsync` 超时清除。
5. **tail-sync**:注入无事件的 custom 消息后 `agent_end` → 补发射;`display:false` 三路径(live 事件/重放/冷读)均不产卡。
6. **RingEventBus**:durable-only 重放、缺口 `omp.stream.resync`(scope 正确)、volatile 不进环、envelope 字段齐全(id/type/directory/sessionID?/schemaVersion/createdAt);wire 总线行为回归(现有 events 语义测试不变)。
7. server JS `node --check`(engine/events/endpoints/projection)。
8. **覆盖清单守卫(§5.1.1)**:`check-event-coverage` 在人为删除 manifest 条目 / 注入未登记 SDK 事件 fixture 时 exit≠0;`check-omp-event-names` 对未注册的 `omp.*` 名零容忍(同样失败);**`omp-bootstrap-matrix.json` 漏登任一 durable 条目的 `snapshotEndpoints`(如故意删去 settings/models 行)→ exit≠0(二轮 H5,第 3e 步)**。
9. **capabilities 协商(§5.2.3)**:envelope `schemaVersion` 与 `/api/omp/capabilities` 声明一致;minor 新增 type 对旧 reducer 无副作用(忽略 + syncDebug,不抛错)。
10. **resync 矩阵(§5.2.4)**:注入 gap → 按 scope 恰好一次权威 GET、断流不被当作空状态;queue version 跳变触发 refetch;settings revision 跳变触发 `GET /api/omp/settings?directory=`(二轮 H5);模型域对账 = wire session snapshot + `GET /api/omp/models`(与 01 §5.5 口径一致);tree 事件触发整树重拉;进程重启 id 归零触发 1→7 全序对账。

### 7.2 E2E(dev 栈 5180/3902,浏览器驱动)

1. **重试场景**(mock provider 连续失败后成功):loader 出现(attempt/max/倒计时)→ msg_A 取代标注(dim + "已被重试取代";P1:卡不消失)→ 新 turn 渲染 → 恢复注记 chip → idle 后队列消息恰好派发一次。**P2 启用后追加**:合成失败 tool 卡消失、带 result 的 tool 卡保留、无重影(§5.3.4)。
2. **fallback**:触发 model fallback → toast "Fallback: A -> B" → 会话徽章/后续消息模型 = fallback 模型 → registry/session.updated 真值。
3. **过早 idle**:注入 `isTerminal:false` settle → 侧栏活动点不灭 → 队列不派发 → 唤醒后正常收口。
4. **compaction**:触发 auto-compaction → loader → 完成后 transcript 出现折叠分隔线(展开见 summary)→ compaction 期间入队的消息在 end 后 flush。
5. **irc/notice**:live irc 卡即时出现;info/warning notice toast 各一;断开 SSE 5s 重连 → durable 事件补齐、无重复卡、无过期 loader;断开超过环容量 → `omp.stream.resync` 后 mode/model(含 `GET /api/omp/models`)/dialogs/settings/agents/jobs/queue/tree/transcript 按 §5.2.4 全域对账(1→7)。
6. **usage/cache-miss**:构造 cacheRead 跃迁 fixtures → divider 恰出现一次;每 turn usage 行字段齐全(无 cost);弱 omp 通道时 tokens-only 降级。
7. **todo_auto_clear**:todo 清空后 WorkStatusTasksSection/live 行同步清空。
8. **鉴权穿透(R4)**:直连/桌面/VS Code/relay 四通道分别验证普通 GET、SSE 建连、`Last-Event-ID` 续传三件套(Basic auth 头经 R4 转换表)。
9. **capabilities 降级矩阵(R2)**:mock 旧 engine(无 capabilities)→ UI 自动 wire-only 不报错;mock relay 旧 bundle → 同降级。

### 7.3 TUI 对照行为(验收基准)

| 行为 | TUI 基准 |
|---|---|
| retry loader 文案 `Retrying (n/max) in Xs…` | event-controller.ts:1972 |
| 回收=仅未 commit 的合成失败卡;壳保留 | event-controller.ts:1944-1955 |
| 恢复注记按 persistenceKey 应用 | event-controller.ts:2002-2012 |
| 终端失败文案 `Retry failed after N attempts: …` | event-controller.ts:2024-2029 |
| fallback toast `Fallback: A -> B` / `Fallback succeeded on M` | event-controller.ts:2039/2045 |
| compaction loader 四种 action 文案 + esc 提示 | event-controller.ts:1834-1854 |
| ttsr 连续合并 | event-controller.ts:2054-2063 |
| todo_auto_clear = 重载 todos(→空) | event-controller.ts:2074-2076;acp 映射 entries [] |
| usage row 字段与阈值(duration>100ms) | usage-row.ts:19-42 |
| cache-miss 判定四条件 | cache-invalidation-marker.ts:49-66 |
| notice 三级分流 | event-controller.ts:976-985 |
| partial 结果不抢终态 | event-controller.ts:1450-1456 |

---

## 8. 开放问题

1. ~~`session.next.*` 是否可作 omp 轨载体~~ **已裁决(总纲 D1 修订)**:不采纳;wire 面唯一例外 = `message.part.removed`(R5;首产延后 P2 门控,§5.3.4)。omp 轨自立 `/api/omp/events`;07 章整族裁剪照删,本章不消费。
2. **irc 卡 TTL/数量上限的 Web 语义**:TUI 的 TTL 回收 + `MAX_LIVE_IRC_CARDS` 上限依赖"未 commit 块才可移除"的终端 scrollback 模型(event-controller.ts:869-914)。Web transcript 是持久 DOM。**建议:不移植 TTL(卡片持久留在 transcript,历史语义);仅保留渲染层"最近 N 张卡高亮"的视觉上限。** 与"TUI 为准"原则的这处偏离理由:回收条件在 Web 无对应物,强行定时删消息会与冷读(卡片在 transcript 文件里)冲突。
3. **superseded 失败尝试在文件冷读的默认可见性**:§5.3.3 选了"保留壳+折叠 tool parts+注记"(方案 B)。若用户反馈历史噪音,备选是冷读默认整条折叠为一行注记(不删数据)。需产品确认。
4. ~~compaction end 与 wire 分隔线消息的双轨时序~~ **已采纳**:`omp.compaction.ended` 携带 `wireMessageID` 预绑定(§5.5/注册表 E 行);UI 仍容忍乱序(divider 先到/后到均正确渲染)。
5. ~~`/api/omp` 端点组的多目录与鉴权口径~~ **已裁决(R3/R4)**:路径复数规约 + 路由只注册在 omp-host(Basic auth),web server 仅代理;`/api/omp/events` 的 directory 参数与 wire `/event` 同 `directoryFromRequest` 归一化(否则重连对账串目录)保留为验收断言(§7.2-8)。
6. **telemetry 的 per-message cost**:SDK 不报 per-message cost;若上游(usage report 反推)未来可估算,`/telemetry` 加 `costUsd?` 字段即可,UI 行预留位。等待上游。
7. **进程内全局单调 id vs per-directory 序列(§5.2.1)**:本章取全局单序列(实现简单、`Last-Event-ID` 单值语义);代价是单目录会话的 durable 事件可能被其他目录流量挤出 512 环(缺口由 resync 兜底,不丢状态)。若实测高频目录频繁打穿,再分目录序列或扩环。**建议:先全局,留监控指标(环挤出率)。**
8. **eventSchema major 双发窗口长度(§5.2.3)**:建议 = 一个 minor 周期(或两周)+ UI 版本渗透率达标后删除 N-1 双发;阈值需产品确认。
9. **jobs/tree 快照端点的精确形状**:注册表 D 行暂引 R3 集合名(`/api/omp/jobs`、04 章 getTree);04 章修订版定稿查询参数后回填 §5.0.3/§5.2.4(不影响通道与注册表结构)。
10. **P2 回收的启用与上游标记(二轮 H4/M10)**:门 a 现以投影侧记账(`syntheticSettledPartIds`,§5.3.4)实现;若上游未来在 SDK 消息/事件上提供显式 synthetic/uncommitted 标记,应优先上游标记并复核门 a。门 a/b/c 全绿后是否默认启用 removal producer(而非维持 overlay-only)需产品确认——重影(保守)与删证据(激进)之间的展示取舍。

---

## 9. 依赖

**前置(硬)**:
- 00-MASTER(D1–D6;本章落位 R1/R2/R3/R4/R5/R6/R12);
- ui-api-decoupling 管辖的 `RuntimeAPIs`/`runtimeFetch` 通道(§5.2 端点组挂载点);
- sync-state-invariants(omp-event-reducer、重连对账、队列闸门改动的纪律);
- relay-transport(SSE + `Last-Event-ID` 过隧道的 allowlist/鉴权/重连分支,R4 验收项);
- 04 章 D04-1 端点底座(omp-host 进程内 `/api/omp/*` 路由组 + Basic auth,web server 仅代理)。

**前置(软/并行)**:
- 01/02 章:模型/模式域事件的 producer case 与快照端点(注册表 A/B 行;两章修订版已按本章命名对齐);
- 03/04/06/08 章:dialog/agents/jobs/tree/settings/queue 的 producer 与快照端点(注册表 C/D/F/G 行;各章修订版落地后回填精确形状,§8-9)。

**后置(消费本章产出)**:
- 01/02/03/04/06/08 章:全部 omp 原生事件**只引用本章注册表**(R1;01/02 修订版已对齐,03/04/06/08 修订版跟进);
- 02 章:goal/thinking_level 事件消费面;plan mode 的 mode_change 条目渲染;
- 04 章:irc/async-result/live-delegation 卡片的实体链接(agentId/jobId 跳转)基于本章 T1 卡与 `custom-messages` 结构;
- 07 章:A8 的 `message.part.removed` 按 R5 从删除守卫与 DAG 移除(reducer 保留;**首产延后 P2 门控,§5.3.4——07 对"05 §7-2 测试断言"的引用需按本章 v3 §7.1-2 升级为壳保留断言**)、A9(retry status 形状)按总纲 §7.6 改"保留"(P1a 点亮);**G08 HOLD 的 `session.error` 解锁:本章不生产(§5.11),终态/flush/通知/reducer 四处替换清单已列**;`message.removed` 删除与本章 P2 门 b 的 reducer 壳保留修改互不冲突(07 §5.8 对账);
- 08 章:WorkStatusPanel 的 context/cost 聚合与 usage row 共用 `info.tokens`/telemetry 数据源;queue 方案 B 以 §5.2.4 矩阵 + requestId/ack/outbox 完备为启用门(R12);错误/完成通知以 terminal `agent_end` 为唯一权威、notice 仅即时 toast(§5.11,与 08 章通知去重同口径)。
