# omp-host 内存优化计划（v4.3，OpenChamber-owned）

状态：阶段 0-4 已实施（阶段 0 的 benchmark/process-tree 采样、阶段 1 replay 止血、阶段 2 live 生命周期、阶段 3 冷读释放、阶段 4 双写检测）；§7.2 的两遍扫描 reader 也已落地（`cold-transcript-page.ts`：`getMessagesPage`/`getSession`/`getEntries` 的冷臂走 metadata pass + 按字节区间回读 fed 窗口；用自有 scanner 而非 `visitEntriesFromFileStream` 是为了记录每条 record 的字节区间，第二遍只读窗口——§7.2 允许自有 parser 承担单条 record 上限；`#entryTreeFor`/`getTranscriptContext` 仍是标注过的 full-materialization fallback）。阶段 5（稳定 wire ID）为**已记录的停机状态**：`wireIdOverrides` 现在在 session 驱逐/删除时清理（必要条件），但长活跃会话中仍随消息数增长——设计一个稳定 wire ID 公式需要长会话压测、跨目录同 ID、冷读/实时/重连与多客户端 optimistic 矩阵的全部证据（阶段 5 验收），未完成前该表是**已知的长会话 retained-memory 风险，总体内存目标未完成**（决策 D5）。本文只描述 OpenChamber 侧的改动。这里的 "OpenChamber-owned/host-only" 意味着**不修改 `@oh-my-pi/pi-coding-agent`**；阶段 1 仍会触及 `packages/web` 的 event hub 和 `packages/ui` 的传输消费者，因为 replay 的边界跨进程。所有"至多一个 writer""已释放"等硬保证都只在**单个 omp-host 进程、已解析到同一 canonical transcript path**的范围内成立；外部 omp TUI 或另一个 host 进程不受本地 registry 控制。**不打补丁，不改依赖，不访问 SDK 私有字段。**

## 一、边界与承诺

### 1.1 允许改动

- `packages/web`、`packages/ui`、测试、性能脚本和本计划。
- 只使用当前 SDK 已公开且已导出的 API：`AgentSession.beginDispose()`、`dispose()`、`isDisposed`、`waitForIdle()`、公开活动状态 getter，`SessionManager` 的公开方法，以及 `visitEntriesFromFileStream`、`loadSessionMessagesReadOnly`。
- session loader 通过 `@oh-my-pi/pi-coding-agent` 的公开导出使用，不从 `node_modules` 绝对路径导入。
- OpenChamber 可以在自己的 domain 中增加清理句柄、诊断计数器、有限缓存和状态机。

### 1.2 禁止事项

- 不修改 `node_modules/@oh-my-pi/pi-coding-agent`，不修改其源码、生成类型、构建产物或 package export。
- 不使用 SDK 私有字段，不 monkey-patch，不用 `patch-package`，不添加依赖。
- 不把一个主观的 RSS 下降写成成功条件。
- 不用固定大小 LRU 淘汰仍然承担 wire 身份语义的映射。

### 1.3 结果分级

下列结果是硬保证：

1. OpenChamber 自己拥有的 replay、subscriber、bridge、parser buffer 和诊断等**传输/临时缓存**都有明确的条目或字节上限，或者有明确的生命周期清理；与持久化数据一一对应的权威索引必须显式标注为 data-proportional，并有删除、重载和大小观察，不能把它伪装成 bounded cache，也不能把正常数据增长称为 leak。
2. 在单个 host 进程内，对同一个 canonical transcript path 至多有一个 live `AgentSession`；驱逐完成前不允许该 path 建立新的写者。`(directory, sessionID)` 只是路由键，必须先解析并核对实际文件身份。
3. 正常路径、可落定的失败路径和 shutdown 都**尝试**执行 host 侧清理。`dispose()` reject 或永不落定时，record 必须进入可观测 quarantine，禁止同文件新 writer，不能把它写成已释放。只有存在独立、可验证且有 deadline 的 host restart/manual-recovery 策略时，才能声称进程级内存有上界；否则这个 heavy reference 仍是未解决的 retained memory，不属于硬保证。
4. 冷读不创建 `AgentSession`，也不创建可写的 `SessionManager`；只读 full-materialization fallback 必须单独标注其峰值成本。
5. replay、parser 和 outbound subscriber queue 发生缺口或超限时必须显式进入 resync/close，不能静默返回缺口之后的半段事件，也不能让慢客户端在 `ReadableStream.enqueue` 后无限积压。
6. 现有 wire message ID 语义不因内存优化而改变；进程重启的 epoch/restart 必须有明确的协议表示，不能靠“旧 numeric id 恰好大于当前 id”猜测。

下列结果只能是尽力而为：

- 没有跨进程锁时，不能保证某个任意瞬间的全局最新，只能检测版本、吸收已观察到的更新，并在竞态后重检。
- Bun、Windows 和 native allocator 可能保留已经释放的页。RSS 不需要回到启动值。

在不改 SDK 的前提下无法解决：

- 一个仍在运行的超长 `SessionManager` 仍然持有完整 `#entries` 镜像。
- SDK 写者本身没有窗口化。Host 只能控制同时存在的写者数量和寿命，不能把一个活写者改成窗口写者。

## 二、问题模型与证据

先区分四个词。无界强引用是 leak。逻辑上已经不用但仍保留的是 retention。请求结束后对象短暂存在、分配器不还页是 churn。活跃会话的完整历史是 SDK 写者的成本，不自动等于 leak。

| 观察 | 判定 | 证据和限制 |
|---|---|---|
| `WireEventBus.replay` 只 push 不裁剪 | 已确认的无界强引用 | `events.ts:87-89`；`capacity` 未参与 `emit`。`OmpEventBus.publish` 在 `:153-156` 有裁剪。此前 `capacity=8` 发送 10000 条后 `replay.length` 仍为 10000。 |
| `global-hub.replay` 只按条数且 `replayAfter` 静默返回空 | 已确认的跨层 gap/留存缺口 | `packages/web/server/lib/event-stream/global-hub.js:17-18,87-95,149-155`；requested id 被裁掉、进程重启和确实无后续都返回 `[]`，不能完成端到端 resync。 |
| `wireIdOverrides` 没有删除路径 | 已确认的无界留存 | `engine.ts:222`、`handleEngineEvent` 写入映射；任意大小 LRU 可能破坏 live/cold ID 合流。 |
| idle session 只有 live 数超过 16 才扫 | 已确认的回收策略错误 | `engine.ts:642-653`。16 个或更少的死会话不会因 TTL 回收。 |
| 当前驱逐先 fire-and-forget dispose，再删 Map | 已确认的并发竞态 | `engine.ts:649-650`。新请求可能在旧 SDK teardown 未完成时重新 materialize 同一文件。 |
| `OmpEventBus.replayState` 空 ring 和 restart 判定 | 已确认的状态机 bug | `events.ts:174-180` 在空 ring 的 gap 分支仍读取 `replay[0].eventId`；`lastEventId >= nextEventId` 只能发现部分重启，旧 cursor 小于新进程当前 id 时会误报 `ok`。 |
| 冷读每次 `SessionManager.open` | 已确认的分配成本 | `engine.ts:876-882`、`:1029-1065` 等。benchmark 测的是 open，不是完整 GET。 |
| `#entryTreeFor` 冷路径未显式关闭 manager | 已确认的资源清理缺口 | `engine.ts:383-387` 打开 manager 后返回；`domain-uri.ts:1672-1675` 直接消费，没有 `close()`/`releaseRetainedEntries()`。不能把该路径当成安全的冷读。 |
| `moveSession`、`deleteSession`、`shutdown` 的 live 清理 | 已确认的生命周期缺口 | 当前 `moveSession` 会在已有 live session 时再次 `SessionManager.open`；`deleteSession` 和 `shutdown` 调用不等待 `dispose()` 就删 Map/清空 sessions。它们可能制造同文件双 writer、Windows 文件占用和 late-event retention。 |
| 解析后 RSS 不回落 | 不是 leak 证明 | 分配器高水位、native memory、child worker 和 page cache 都可能造成同样曲线。 |
| worker 子进程数量持续增长 | 当前代码有 `worker-dispatch.ts` 的 selector 防误启动修复；是否仍有运行时 worker 未退出必须由 process-tree 采样证明 | 不是凭 RSS 推断的 host leak；阶段 0 只验证 spawn、disconnect、shutdown 和失败重试后数量是否回落。 |
| 1.4 GB WS 或 3.4 GB private/commit 的组成 | 未确认 | 单次进程指标不能区分 replay、live sessions、projector、worker 和 allocator。 |

这组隔离实验只证明 SDK 活对象确实持有可释放的 retained state。它没有测出当前 host 各容器的 retained size，也不能把 RSS 或 commit 台阶归因给某一个容器。当前工作树已经有局部措施，例如 `OmpEventBus` 和 web `global-hub` 的条目裁剪、URI token 的 issue-time sweep 和 mode tracker 的 release；这些措施不改变 `WireEventBus`、web hub gap 和 live session 的未解决问题。阶段 0 必须以当前工作树重新测量，不能把历史 stash 或计划文字当作已落地修复。

### 2.1 截至当前代码的结论

- `WireEventBus.replay` 是确认存在的无界强引用。它可以称为逻辑内存泄漏，但这还不能证明它单独造成了观测到的 65 GB commit。
- `wireIdOverrides` 是确认存在的无界 retained map。它承担 live/cold ID 合流语义，不能用未经测试的固定 LRU 粗暴截断。
- `live <= 16` 时完全跳过 idle sweep，确认是回收策略错误。闲置的 SDK writer 因此可能长期留在 host Map 中。
- 当前驱逐在 `dispose()` 完成前删除 Map，确认有同文件双 writer 和 late-event 竞态。它也使“Map 变小”不能证明对象已经释放。
- `#entries` 全量驻留是活跃 SDK writer 的设计成本，不单独等于 leak。只有活跃结束后 host 仍保留它，或 host 自己的引用链阻止释放，才是 retention bug。
- 冷读的重复解析和 RSS 不回落目前只能判为 churn 或 allocator 行为，必须用 retained heap、峰值 heap 和容器计数继续拆账。

### 2.2 Leak 证明标准

- `WireEventBus.replay` 的源码不变量已经足够证明“事件数无界时强引用无界”：每次 durable `emit` 都追加，且当前没有删除路径；不需要把它和 RSS 绑在一起。
- 对 live session、web hub、projector、worker 等其余候选，必须在固定 workload 下重复“创建/使用 → 停止写入 → 清理/GC → quiescence”，并同时观察 owner 容器数量、retained estimate、heap dominator/heapUsed 和子进程数。没有活跃 lease、inFlight、subscriber 或未落定 disposal 时，容器 retained 值仍跨轮增长，才称为运行时 leak。
- 只看到 RSS、Private Bytes 或 commit 不下降，最多证明 allocator/native/page-cache 的高水位，不能单独证明对象仍被引用；反过来，RSS 不降也不能否定容器已正确释放。

## 三、Host-owned topology

### 3.1 两条完全分开的路径

`ColdTranscriptReader` 只使用 SDK 已导出的 read-only loader。历史 GET、列表、telemetry、entries、tree 和 URI 读取都优先走这里。它不能返回 `SessionManager` 或 `AgentSession`，也不能把冷读结果放入 live Map。当前这些入口仍有若干 `SessionManager.open`，本段描述的是迁移后的目标，不是当前实现。

当前代码里的 `getSession`、`#projectedMessages` 和 `entryTreeFor` 仍会打开 `SessionManager`；因此“冷读不创建可写 manager”是阶段 3 完成后的验收，不是 v4.3 文档发布时已经成立的事实。

### 3.2 统一 key 和 live record

所有按 session 索引的 host Map 使用：

```text
SessionKey = normalize(directory) + "\\0" + sessionID
```

即使 UUIDv7 在正常生成路径上几乎全局唯一，也不把这个概率当作目录隔离的正确性保证。迁移必须覆盖 `sessions`、`materializeInFlight`、`wireIdOverrides` 和各 domain 的 session map；全局 personas、event bus 等不按 session 索引的容器不强行套这个 key。所有 `sessions.get(sessionID)` 都要迁移为带目录的 lookup。

canonical transcript identity 不能只是一段拼接字符串。实现必须在 materialize、move、delete 和 reload 的边界重新解析规范化绝对路径，并核对 session header/file signature；旧路径失效后，record 的 ownership 不能凭旧 key 继续存在。这个 key 只保证本进程内的路由和去重，不是跨进程锁。

每个 live record 至少包含：

```text
state: materializing | live | evicting | failed
agentSession
sessionManager
disposePromise
inFlight
lastUsedAt             // monotonic clock
leaseCount
activitySnapshot
cleanup: event/name/dialog/mode handles
fileSignature
lastKnownTailEntryId   // only a fast hint, not the authority
```

调用者只得到操作结果，不得到可跨操作保存的 manager 引用。`withLiveSession(key, fn)` 在调用体期间增加 `inFlight`，并在 `finally` 中减少。成功命中 live record 的**用户可见读操作**和 lease heartbeat 才调用 `touch(key)`；list、diagnostic、内部重试和 sweeper 自己的观察不能把死 session 无限续期。冷读不刷新 live record，也绝不 materialize。materialize、reload、move、delete 和 eviction 必须经过同一个 per-key operation gate，不能让两个异步调用各自通过 signature 检查后同时操作同一文件。

### 3.3 materialization 失败必须可回收

`#materializeNow` 打开 manager、创建 agent、安装 event subscriber、初始化扩展和 UI context 的整个过程必须在一个 host `try/finally` 中。任何一步失败都要：

1. 调用已安装的 event unsubscribe。
2. 调用 `onSessionNameChanged` 返回的 unsubscribe。当前代码丢弃了这个返回值，SDK 已公开该清理函数。
3. 对已经创建的 `AgentSession` 调用公开 `dispose()`。
4. 对还没有 agent 的临时 manager 执行最终 close/release。
5. 清理 modes、dialogs、chrome 和 in-flight 记录。
6. 清理动作必须幂等并独立落定（等价 `Promise.allSettled`）；某个 unsubscribe、`dispose` 或 domain release 失败不能跳过其余清理，原始 materialization 错误要保留给调用方和诊断。

`#materializeInFlight.finally()` 只负责删除去重 Promise，不能代替资源清理。

### 3.4 驱逐状态机

```text
cold -> materializing -> live -> evicting -> cold
                         \-> failed
```

驱逐开始时先把 record 设为 `evicting`，并拒绝新的 live 操作。调用方只能等待同一个 `disposePromise` 后从磁盘重新 materialize，或收到明确、可重试的 `session-evicting`/`busy` 结果；禁止第二个 materialization，也禁止静默回退到旧 manager。然后：

1. 同步调用 `agentSession.beginDispose()`。它是 SDK 的公开入口，必须发生在 host 第一次 await 之前。
2. 拆掉 host 自己的 event、name、dialog、mode、chrome 和 lease 连接。之后任何 late event 都只能落到已标记的 record，不能启动新工作。
3. 在 host 已经禁止新操作后发送一次 `session.idle`。不要等一个可能变慢的 SDK drain 才让客户端知道 session 已退出 live 状态。
4. 调用并保存唯一的 `agentSession.dispose({ drainTimeoutMs })` Promise。不要在 host 里重写 SDK 的内部 drain、seal、close 顺序，SDK 的 public dispose 已经负责这些步骤和 retained memory release。
5. 等待该 Promise 落定后清空 record 中的大对象。只有当 live Map 仍指向同一个 record 时，才从 Map 删除；这只证明 host ownership transition，不证明 RSS 或 retained heap 已立即下降，诊断必须允许 SDK/allocator 的 quiescence 延迟。
6. dispose 失败时保留带原因的 `failed` tombstone，禁止同文件复活，并发出诊断。不能用“删除 Map”伪装释放成功；需要显式的重启或人工恢复路径处理永久失败。

超时只允许让 sweeper 不等待当前 record，不能让 host 删除 record 后马上建立新写者。dispose Promise 仍在后台落定时，record 必须保持 `evicting`。shutdown 先关闭新请求入口并同步调用所有 `beginDispose()`，再在全局 shutdown deadline 内等待 disposal 和 domain cleanup。deadline 到期时，未落定 record 进入 quarantine 并继续禁止新 writer；只有进程确实退出，或独立 recovery supervisor 接管，才能释放可能仍被持有的 SDK 对象。`sessions.clear()` 先于这些步骤不算 graceful shutdown。

`failed` tombstone 不能只是把大对象换个名字：若 `dispose()` reject/永不落定，旧 `AgentSession` 可能仍需被强引用以阻止同文件复活。该状态必须单独记录 heavy reference、重试次数、占用估算和 restart/manual-recovery deadline；达到阈值时只能 fail closed 并触发受控 host restart/人工处理，不能丢引用后偷偷允许第二个 writer。阶段 2 的“清理”验收必须区分成功释放与失败隔离，不能把失败 tombstone 当作已回收。

## 四、活动判定和 idle TTL

### 4.1 活动判定

sweeper 只在下列条件全部不成立时考虑 TTL：

- `isStreaming`、`isAborting`。
- `isRetrying`、`isCompacting`、`isGeneratingHandoff`。
- `isBashRunning`、`isEvalRunning`、`hasPendingBashMessages`、`hasPendingPythonMessages`。
- `hasPostPromptWork`、`hasPendingAsyncWork()`、`queuedMessageCount > 0`。
- `pendingDialogs > 0`、host `inFlight > 0`。
- TTSR abort、advisor、正在运行的 speculative compaction 等经过 call-site 审计后确认仍会触碰 session 的状态。

sweeper 选中候选和调用 `beginDispose()` 之间不能有无保护的 await。它必须在同一个 per-key gate 内重新读取 lease、`inFlight` 和所有 SDK 活动状态。lease acquire 与驱逐相撞时只能让 acquire 赢并取消驱逐，或让 acquire 收到明确的 `session-evicting` 后重试；不能一边建立 UI context，一边拆 session。

### 4.2 idle TTL 语义

- lease 心跳、写操作、成功命中 live record 的用户可见读操作刷新 `lastUsedAt`。后台 list、diagnostic、内部 retry 和 sweeper 观察不续期。
- TTL 初值保留 30 分钟，sweep 周期保留 60 秒。
- 删除 `live <= 16` 的数量闸。数量不是内存上限。
- 这是一套 idle TTL reaper，不是按容量淘汰的 LRU。它限制死 session 的寿命，不限制活 session 数量，也不提供固定总内存上限。
- 冷读不刷新 live record，也不把 session 读活。
- 使用 monotonic clock 计算间隔，wall clock 只用于日志和诊断。
- 不强行驱逐活跃会话。没有 SDK 窗口写者时，aggregate byte budget 只能先做压力告警，不能假装是硬上限。

因此本计划保证“满足定义的 idle session 最终进入清理流程”，不保证每个失败或永不落定的 `dispose()` 都能在同一进程内释放，也不保证“任意数量的活跃大历史仍低于固定 RSS”。失败清理进入 quarantine 和受控恢复；后一个目标若产品必须要，需另立允许拒绝新 materialization 或改变查看语义的决策。

## 五、事件环

### 5.1 条目和字节双上限

`RingEventBus` 增加 host-owned 的 `replayBytes` 和显式 `maxBytes`。每次 durable append 后按以下条件淘汰最旧条目：

```text
replay.length <= maxEntries
replayBytes <= maxBytes
```

只限制条目数不够。`toolPartial` 的每次 `message.part.updated` 携带累计 output，单个事件和最后 2048 个事件都可能很大。超过单项预算的事件不截断 payload，直接不进入 replay，并把对应 event id 记入 gap 状态。live subscriber 仍可收到它。
单项事件被跳过会在 ring 内部形成 hole，不能只用最老条目推断 gap。对于本身有单调数值语义的 host wire ID，可以记录被淘汰、被预算拒绝和进程重启造成的连续 ID 区间；对于 web hub 的 opaque upstream ID，只能记录 epoch、requested ID 是否仍在 ring 以及明确的 evicted-before 状态，不能按字符串或数值臆测连续性。gap 元数据本身也必须有上限：合并相邻区间，优先保存 monotonic floor 和 bounded hole 集合，超过 hole 上限就折叠成“需要 full resync”的不确定状态，不能每个被拒绝事件追加一个永久区间。`subscribeSince` 只要请求范围与任一区间相交，或命中不确定状态，就必须进入 resync。

使用 deque 或环形数组，避免每条高频事件都对数组做线性 `shift`。`sizeOf` 必须是保守且可测的估算，不把估算值冒充 JS heap。估算器不能对不受信任的 payload 做无界 `JSON.stringify` 或递归遍历；必须有 traversal/byte cutoff，并在超限时把事件判为 over-budget。ring 保留的 envelope 也必须遵守 payload ownership，不能让调用方后续修改一个已保留对象，把额外的大对象图挂进 ring 而不更新计数。

初始配置只作为可调参数，不作为性能结论：`maxEntries=2048`，`maxBytes` 由阶段 0 的事件大小分布确定。验收同时检查两个上限。

### 5.2 Gap 语义

`WireEventBus` 必须有和 `OmpEventBus` 等价的 replay 状态：`ok`、`restart`、`gap`。`/event` 不能在 ring 已经淘汰旧 id 时继续静默发送 suffix。

默认采用 in-band wire control：`/event` 在 gap 或 restart 时返回 200 SSE，发送无 `data` 的 `event: omp.stream.resync`，并发送 `id: currentBusTail`；若 tail 为空必须发送 `id: 0` 清掉客户端旧 cursor，不能省略 `id`。control 发出后服务端订阅基线也必须切到该 tail，不能再按原 requested id 重放旧 suffix。生成 client 必须在 `onSseEvent` 识别该 control，重新建立 cursor、触发全量对账并继续订阅；WebSocket bridge 和直接 SSE consumer 必须提供同等处理。`/omp/events` 保留自己的 JSON envelope control，但 envelope 的 `lastEventId` 必须是可重连的实际 tail/sentinel，native client 收到 control 后必须更新 cursor，不能用未进入 ring 的 phantom `nextEventId` 导致每次重连重复 resync；空 ring 不能读取 `replay[0]`。两个契约分别测试。不能返回 HTTP 409 后让生成 client 带旧 `Last-Event-ID` 重试，因为当前 client 会把非 2xx 变成通用错误并进入自旋。

resync control 是协议控制帧，不是普通 durable replay 条目。它不能占用 ring 容量、不能制造一个未进入 ring 的可重连 id，也不能让客户端把旧 cursor 当作新 epoch 的确认。control 必须携带实际 tail/sentinel 和 epoch 状态；客户端在完成 authoritative reconcile 前不能把 control 的展示 id 当成已消费的业务事件。
事件 ID 是 bus 级序列，而订阅还可以按 directory 过滤。gap 判定必须明确采用全局保守语义，或维护按目录的可重放 floor；不能因为其他目录的事件被淘汰就误称某个 scoped stream 完整，也不能把 scoped stream 的稀疏事件当作连续序列。

#### 5.2.1 重启身份

`nextEventId` 在进程重启后会重新从初值开始。仅比较“旧 cursor 是否大于当前 `nextEventId`”不能可靠识别 restart：旧 cursor 可能比新进程当前 id 小，随后会误进入 `ok`。Wire 和 omp 两条流都必须通过显式 boot epoch、restart control 或等价的握手状态表达进程身份；不能用未进入 ring 的 phantom id 冒充 cursor。若旧 wire 客户端不能理解 epoch，安全降级是丢弃 resume 并做一次明确的 full reconcile，而不是猜测旧 cursor 属于当前进程。

`message.part.delta` 是否标 volatile 延后决定。先保留 live delivery 和 durable replay 的现有语义，先把 replay 限制和 gap 处理做对。若改 volatile，必须补完整 snapshot bootstrap，不得只删 replay。
Ring cap 只能限制当前 replay 的 retained bytes，不能修复 `StreamProjector.toolPartial` 把累计 output 反复拼接并反复放进完整 snapshot 的 O(n²) 分配 churn。阶段 0 必须单独测量这条路径。运行中的 tool output 还必须有独立的 per-tool 字节上限、spill/截断策略或明确的慢消费者关闭策略；只统计 replay bytes 不会限制 projector 自身的累计字符串。若保留完整 running snapshot，就把它作为独立的 churn/单事件预算问题处理；若改成增量 delta 或最终快照，必须补齐重连 bootstrap 和旧客户端兼容测试，不能把 replay cap 当成这个问题的修复。
Gap 发生在 streaming turn 中时，普通持久化 transcript 可能尚未包含最新 partial；resync 不能用空 GET 覆盖已观察状态，也不能声称已经恢复完整 delta。必须从 live/authoritative snapshot 或 terminal 后的受控 tail refresh 补齐，失败则保留旧状态并标记待重试；这是事件环验收的一部分，不因 replay 已有界而豁免。

### 5.3 subscriber 清理

SSE controller enqueue 失败、request abort、WS close 三条路径都必须删除 subscriber。当前 `send()` 捕获 enqueue 异常只设置 `closed`，没有确定性 unsubscribe。subscriber 数量进入观察口。
`subscribeSince` 的“先遍历 replay、再加入 subscriber”还要覆盖可重入发送测试。replay 回调本身可能同步触发新的 emit，当前顺序会让新事件落在 subscriber 加入之前。实现必须建立明确的序列边界，不能只靠数组裁剪声称没有 gap。

### 5.3.1 subscriber queue 和帧缓冲

`ReadableStream.controller.enqueue()` 不提供网络背压信号。host 的 wire SSE、omp SSE、web hub subscriber 和 upstream SSE parser 都必须有单客户端待发送字节上限和单个 SSE block 上限。生成的 UI SSE client 也必须限制未完成 block 的字符串长度；`visitEntriesFromFileStream` 还需要单个 JSONL record 的字节上限，因为 `maxRecords` 只限制记录数。UI `createEventPipeline` 的 `directories` Map、每个目录的 `queue`/`buffer`/`coalesced` 也必须按已知目录身份、目录字符串长度、事件数和字节设上限，并在 `cleanup()` 清空定时器和目录状态；否则任意目录事件可以在浏览器进程内制造另一条无界留存链。超过上限时关闭该连接并要求 resync，或丢弃可重建的 volatile 数据；不能继续把编码后的 chunk、未完成 block 或重放结果留在闭包和 stream queue 中。WS 的 `bufferedAmount` 上限不能替代 SSE queue 上限。验收要覆盖慢 SSE 客户端、永不结束的 SSE block、超大单事件、超大 JSONL record、任意目录 fan-out、abort/close 后的 timer 和 subscriber 计数。

### 5.4 跨层 replay

`packages/web/server/lib/event-stream/global-hub.js` 还有一份独立的 count-only `replay`，不能因为 host ring 有界就当作安全。它必须同时有 retained-byte 上限、明确的 `ok/gap/restart` 状态和可区分的 `replayAfter` 结果；“找不到 requested id”不能和“没有后续事件”都返回空数组。全局 hub 与 directory bridge 的 gap 必须向每个受影响 WS client 发送 control，不能只让共享上游 reader 自己重连。

hub 的 `stop()`/上游重启不能把旧连接 epoch 的 replay 与新 epoch 混在一起。重启时要清空或隔离旧 replay、递增 epoch，并向现有 client 广播 restart/resync；旧 reader 的 late event 也必须被 generation token 丢弃。`stop()` 后立即 `start()` 不能并行保留两条 upstream reader；要么等待旧 reader 的终止，要么让新 generation 使旧 reader 完全失效。

`upstream-reader` 必须保留 SSE 的 `event` 名和 `id`，不能只转发带 JSON payload 的块；同时要在 control frame 上推进自己的 cursor。每次 response body 结束、abort 或 parse failure 都必须 cancel/release reader、清空 parser buffer 和 stall timer。host wire 的 `omp.stream.resync` 经过 global hub、directory bridge、WS frame 后，UI 的 SSE/WS 两种消费路径都必须触发同一套全量对账。桥接层的 replay、subscriber、per-client cursor、epoch 和 control fan-out 都纳入阶段 0 观察口及阶段 1 验收，不能把“host 端点已发 control”当成端到端完成。

这份 web hub replay 只承担传输恢复，不改变 wire message ID 合约；若跨层无法安全传递 gap，宁可丢弃 resume 并做明确 full reconcile，也不能静默返回 suffix。

## 六、host-owned 缓存和句柄

`#sessions` 不是唯一驻留事实。阶段 0 必须盘点并在后续阶段处理这些容器：

| 容器 | 当前风险 | Host-only 处理 |
|---|---|---|
| `wireIdOverrides` | 无界，且承载 ID 合流语义 | session delete 清理只是必要条件，不是长期上界。稳定 ID 或可证明的紧凑/持久化映射完成前，这个表仍会随长会话增长，不能把阶段 2 的回收称为内存问题已解决；阶段 5 是内存目标的阻塞项。 |
| `dialogs.bridges` | 每个 session 建 bridge，正常 session dispose 没有 delete | 给 dialogs domain 增加 `releaseSession`，驱逐和删除时调用；shutdown 保留 `dispose`。 |
| `domain-chrome.directories` | 目录和 dropped counters 没有 per-directory release | 增加目录生命周期或有界 TTL，保留仍有 lease/live session 的目录。 |
| `SessionMetaRegistry.cache` | 每个访问过的目录进程常驻；单个目录内的 session map 是与持久化元数据成比例的权威索引，也可能很大 | 先限制目录级驻留并在无 live/lease/in-flight 时丢内存副本；`entries()` 不得把可变 Map 借给长期调用者。单目录内条目/字节上限需要 sidecar 格式或索引设计，阶段 0 未证明前只报告 data-proportional，不宣称全局 bounded。 |
| URI tokens | 目前只在 issue 时 sweep，没有独立定时清扫 | 增加轻量定时清扫和 shutdown dispose。 |
| parked-agent descriptors | revive closure 可能持有 registry/runtime | 按 owner session 的 kill/delete/dispose 清理；仍然 parked 的运行不能误删。 |
| `syncedEntryKeys`、projector partial maps、turn result maps | 随活跃转录或工具输出增长，terminal 事件可能缺失 | `syncedEntryKeys` 不能在 terminal 后盲目清空，否则下一轮 tail-sync 会重复发出整段历史；用已落盘 entry watermark、可验证的 tail signature 或等价的紧凑边界替代逐条集合。projector/turn maps 在 terminal、abort、dispose、超时和 owner 删除后清理；长工具输出必须有独立字节上限和计数，不能靠 replay cap 解决。 |
| `unknownEventCounts` 和其他诊断计数 | 未知类型或攻击性输入可制造无限 key | 使用固定 key/总量上限，超出后归入 bounded `other`，并在诊断中报告丢弃数。诊断容器也必须遵守本计划的生命周期和关闭归零规则。 |

每个容器都要有 owner、生命周期和观察指标。若某容器被明确设计为进程级缓存，必须记录容量和理由。

因此阶段 2 的 session 回收不能宣称已经解决全部内存增长。只要长会话持续产生需要合流的消息，`wireIdOverrides` 仍可能按消息数增长；阶段 5 的稳定 ID、紧凑映射或持久化索引必须在“内存问题已修复”的出口条件中，不能在兼容性困难时无限期保留为默认完成状态。

## 七、冷读

### 7.1 入口规则

以下路径禁止 `SessionManager.open`：历史消息、session info、telemetry、structured entries、tree、URI history 和普通列表投影。它们使用 `ColdTranscriptReader` 或明确的只读 adapter。当前 `#entryTreeFor` 仍向 URI domain 提供 manager-like 对象，迁移时必须改成只读 snapshot/adapter；在迁移完成前它属于显式例外，不能把当前实现当成安全的冷读。

需要写者的路径仍通过 `LiveSessionRegistry` 获取 manager。`fork`、`move`、真正写入 transcript 的操作不能偷偷改成只读。

任何暂时无法改成 streaming 的旧路径只能退回**只读的 full-materialization fallback**（例如 `loadSessionMessagesReadOnly`），并在 `finally` 中释放调用方持有的结果引用；不得退回 `SessionManager.open`/可写 manager 后仍把它标成 cold read。SDK fallback 自身会把所有 entries 放入数组，并可能为所有 blob refs 建立 `Promise.all`，因此 host 的 `finally` 不能把它改写成 bounded 路径。若某 route 只能依赖 writer，必须明确列为迁移未完成的例外，不得进入“冷读无 writer”的硬保证。

### 7.2 streaming reader 设计

使用 `visitEntriesFromFileStream` 逐条处理：

- 保留 turn-state 的当前折叠状态，而不是全部 entries。
- 只保留满足 `limit`、`before` 和 projection 需要的尾部窗口。
- telemetry、entries、tree 等路径只累计标量或**请求明确限制的**结果；全历史 `entries`/tree 响应本身的输出大小可以随历史增长，不能把响应 payload 的必要大小写成固定工作集上界。若 HTTP 层一次性 `JSON.stringify`/`json()` 整个结果，序列化本身仍会产生 O(output) 峰值；bounded working-set 的声明必须与响应编码峰值分开，或改用受控 streaming/显式输出上限。
- 只为最终选中的消息解析或读取 blob，不能对整个文件的 blob refs 做 `Promise.all`。
- 文件在读取期间发生变化时，用开始和结束 signature 比较；不一致则丢弃结果并有限重试一次，或返回明确的 partial/changed 结果。这不是线性一致性快照，结束 signature 之后的外部写入仍可能落在结果之外。
- `before` 仍可能需要从头扫描。`visitEntriesFromFileStream` 的 `maxRecords` 只限制记录数量，不限制单条 JSONL record 或 parser remainder 的字节；在不改 SDK 的前提下，host 必须为超大 record 设 preflight/自有 parser 限制，或把该查询标为非 bounded fallback。验收比较 CPU、IO 和峰值 live heap，不承诺亚秒级，也不承诺 RSS 回到基线。

### 7.3 读结果正确性

新 reader 必须和现有 projection 在固定 fixture 及大单条记录、深嵌套 payload、超大 blob、重复 malformed record fixture 上逐消息比较：角色、顺序、parent、turn-state、divider、wire ID、分页 cursor、blob 和 malformed record 行为都要一致。性能通过后才能删除旧 fallback。

## 八、无 SDK 改动条件下的双写防护

### 8.1 检测层级

`lastKnownTailEntryId` 只做快速提示，不能声称能识别所有 rewrite。TUI 的 `rewriteEntries()` 可以修改中间内容而保留最后 entry id。

每次落定点按以下顺序检测：

1. 比较文件 size、mtime 和已知 tail signature。
2. 发生变化时读取尾部并分类为 append、truncate 或 rewrite。
3. 需要确认时使用有限窗口 fingerprint；仍无法证明相等就判 dirty，不判 clean。
4. dirty 的 GET 走 cold reader。dirty 的 prompt 只在 session 不活动时执行 reload。

这仍不是跨进程 CAS。检测到 reload 前后的 signature 必须再次比较，最多有限重试一次。第二次仍变化时返回明确的 conflict/busy 结果或让调用方重试，不把旧上下文当成最新。reload 必须经过 record 的 per-key operation gate，且有独立的最大文件/峰值预算；SDK `reload()` 自身可能复制或重建大历史，OpenChamber 无法仅靠外层 `try/finally` 给它设内存硬上限。

### 8.2 reload 使用限制

`AgentSession.reload()` 实际调用 `switchSession()`，会 abort、flush、捕获旧上下文并重建状态。SDK 源码已经警告大历史的 same-session reload 可能产生 heap blowup。

因此：

- 只在 signature 确实变化且 session 不处于任何活动守卫时调用。
- 不把 reload 放进每次 GET，也不把它作为无条件 prompt 前置动作。
- reload 前后重新检查 file signature。
- streaming、retry、compaction、handoff、bash、eval 或 async work 中不 reload；这些路径只走已有 steer/queue 语义，并把“无锁下无法保证最新”写入契约。

### 8.3 前端门

复用 `refetchSessionMessages` 作为 UI 的减少旧数据显示的优化。它不是服务器一致性保证，也不能替代服务端 signature/reload。busy、steer、queued、移动端 API 和第三方客户端必须依赖服务器规则。

## 九、观察口和测量

### 9.1 观察口

增加 debug/admin gated 的诊断入口，默认不暴露给普通客户端。只返回计数和大小估算，不返回 transcript、prompt、工具输出、token、路径内容或凭据。

至少记录：

- live record 数、每个 record 的 state、entries 数、磁盘 bytes、估算 retained bytes、lastUsed、lease、inFlight、活动位图。
- materializing、evicting、failed 数和持续时间。
- wire replay entries、估算 bytes、eviction 数、gap 数、subscriber 数。
- ID override、dialog bridge、chrome directory、settings clone、URI token、parked descriptor 数。
- `heapUsed`、`external`、`arrayBuffers`、RSS、Windows private bytes、Working Set、handle count 和 child process 数。
- 观察口自身的计数和 retained estimate 必须是 O(1) 或有界采样；禁止为了计算 estimate 遍历、复制或 JSON 序列化完整 live object。

观察口读取不能调用 `getEntries()` 复制完整数组，也不能 `JSON.stringify` 活 session。所有“bytes”字段必须注明是 disk bytes、serialized estimate 还是 retained-object estimate。

### 9.2 可重复实验

修正 benchmark：warmup 和每次 open 都显式 close/release；重复同一个大文件；分别测 open、完整 projection、冷 reader、live materialize/dispose 和事件环。每个场景至少有 baseline、steady-state 和结束后的 quiescence 区间。benchmark 结果只证明被测场景的分配/保留行为，不自动证明生产 host 的 dominator 归因。

Windows 记录字段名称必须和含义一致。不要把 `PagedMemorySize64`、private bytes、virtual size 和 Working Set 混称为 commit。

## 十、阶段计划

| 阶段 | 变更 | 验收与回滚 |
|---|---|---|
| **0 证据与清单** | 诊断计数、process-tree 采样、修正 benchmark、盘点 host `RingEventBus`、web `global-hub`、subscriber、bridge cursor/epoch 和 domain handle。不改变用户行为。 | 能分别回答 host replay、web hub replay、live session、冷读 churn、projector、worker、data-proportional registry 的 retained/temporary 贡献。没有拆账就不能宣称根因已确认。诊断入口可完全关闭。 |
| **1 replay 止血** | host `RingEventBus` 与 web `global-hub` 的条目+字节上限、gap/restart/epoch 状态、wire/omp 分开的端点处理、SSE parser/upstream reader/control 转发、WS bridge fan-out、subscriber 清理。暂不动 `wireIdOverrides`。 | 大 payload 和 10000 条事件测试同时满足两个上限；host、web hub、旧 Last-Event-ID 都不收到静默 suffix；UI、生成 SSE client、upstream reader、WS bridge、直接 SSE consumer 完成对账；streaming 中的 gap 不覆盖未持久化 partial。协议兼容性异常时只回退到“强制 resync/全量对账”，不能恢复无界 replay。 |
| **2 live 生命周期** | `LiveSessionRegistry`、materialization failure cleanup、evicting state、awaited disposal、完整活动守卫、dialog/chrome/mode/descriptor 清理、目录 key、move/delete/shutdown 串行化。 | fake clock 下符合活动定义的 idle session 在 TTL 后从 live Map 和 per-session containers 消失；并发 prompt/attach 不建立双实例；失败 setup 不留 subscription/name callback；active getter 和 pending dialog 不被驱逐；dispose 未落定时不能复活同文件；失败 dispose 进入可观测 quarantine，受控 restart/manual-recovery 路径有界且可测试。回滚只关闭新 sweeper，不恢复 fire-and-forget delete。 |
| **3 冷读拆分** | `ColdTranscriptReader` 接管已经证明可窗口化的普通 GET；`getSession`、`getEntries`、`getTranscriptContext`、`#entryTreeFor` 和普通列表投影分别改用只读 adapter、两遍扫描/临时 offset index 或明确标注的 full-materialization fallback。先移除 writer 依赖，再优化峰值内存，不能假设一个 visitor 足以复现所有 branch/transcript 语义。 | 新旧 projection 逐消息一致，包含 branch、compaction、malformed record、blob 和分页边界；任何冷 GET 都不创建 writer/AgentSession；tree/URI 路径显式关闭临时资源；只有通过 bounded proof 的查询才报告内存上界，其余 fallback 单独报告峰值 heap、CPU 和 IO。 |
| **4 双写收敛** | size/mtime/tail signature、dirty 分类、有限 reload/recheck、move 的 ownership transfer、前端 refetch 门。 | TUI append 被 GET 看见；保留尾 ID 的 rewrite 至少被 signature 判 dirty，否则测试必须把它标为已知 best effort；reload 竞态不丢 entry；streaming 不触发 reload。无锁下的“绝对最新”从硬承诺中删除。 |
| **5 ID 合约** | 仅在前面证据和测试完成后设计稳定 wire ID。可选 host sidecar 或新 ID 公式，但必须处理多客户端 optimistic message。 | 通过长会话压力、跨目录同 ID、冷读/实时/重连和多客户端 optimistic 矩阵后，旧映射可被删除或有界化。若不能证明兼容，阶段停止并明确记录“内存目标未完成”；保留 process-lifetime mapping 是兼容性停机状态，不是通过验收，也不能把它称作 bounded。 |
| **6 收敛与清理** | 删除旧冷读路径、移除临时诊断开关以外的死代码，更新 owning docs 和 benchmark。 | 运行 focused tests、package checks、实际 host smoke 和重复内存场景。确认没有依赖 SDK 私有实现。 |

顺序：**0 → 1 → 2 → 3 → 4 → 5 → 6**。阶段 1 和阶段 2 可以分别回滚。阶段 3 依赖阶段 2 的 canonical path、资源所有权和诊断接口；阶段 4 依赖阶段 2 的 per-key operation gate；阶段 5 依赖阶段 1/3/4 的事件和投影对账证据。阶段 1 的回滚只能关闭 replay resume 并强制对账，不能回到无界数组。阶段 5 不得作为阶段 1 的顺手改动。
### 10.1 可判失败的验收

- **事件环**：在固定 `maxEntries`/`maxBytes` 下发送 10000 条普通事件、多个超预算单事件和累计 `toolPartial`；断言 host ring 与 web hub retained 条目/估算字节都不越界，所有被淘汰、被拒绝、epoch 重启造成的 ID 区间都触发 gap/restart；空 ring、内部 hole、目录过滤和可重入 `subscribeSince` 不得静默漏事件或抛异常。四类消费者（UI、生成 SSE client、WS bridge、直接 SSE）都必须看到 control 后完成全量对账，cursor 能推进到 tail/sentinel，且不会带旧 cursor 自旋；streaming 中的未持久化 partial 不得被空快照覆盖。这里的“字节”必须是实现声明的 serialized estimate，不得误称 JS heap。
- **live 生命周期**：fake clock 覆盖 `live <= 16`、lease 心跳、每个活动 getter、pending dialog、inFlight、materialization 失败、dispose 超时、late event、同文件并发 attach、move/delete 和 shutdown。断言失败 setup 不留 callback/subscriber，`beginDispose` 后不再创建新 writer，唯一 dispose Promise 落定前不删除或复活 record；成功释放、失败 quarantine、受控 restart 三种结果分别可观测。shutdown 关闭新请求入口后在全局 deadline 内等待 disposal；超时记录 quarantine 并继续禁止新 writer，不能把未落定对象声称已释放。测试还要断言不同 directory 下的同名 `sessionID` 不互相命中。
- **冷读**：在含 branch、compaction、malformed record、blob 和分页边界的 fixture 上逐消息比较新旧 projection；用构造/打开计数断言冷 GET 不创建 `AgentSession`/可写 manager；对 tree/URI、只读 fallback 和异常路径断言 `close`/`releaseRetainedEntries` 在 `finally` 执行。只有有 proof 的查询才断言 bounded peak，其余同时记录峰值 heap、CPU、IO；registry 等 data-proportional 索引单独报告，不冒充 bounded cache。该条在阶段 3 完成前保持为未通过的目标，不得用当前 `SessionManager.open` 路径冒充已验收。
- **双写与 ID**：模拟 append、保留尾 ID 的 rewrite、truncate、并发 reload、streaming 期间外部写入、跨目录同 ID 和多客户端 optimistic message；断言 dirty 不被误判 clean、活动 turn 不 reload、有限重试耗尽后返回明确 conflict/busy、live/cold/reconnect 不重复或错误合流。
- **观察口**：诊断请求本身不得调用全量 `getEntries()`、序列化活 session 或返回 transcript/路径/凭据；关闭诊断开关后新增 retained counters、timers、subscribers 和 hooks 必须归零或回到基线，既有 data-proportional registry 不要求归零但必须可观测且有 owner。

### 10.2 回滚边界

- 阶段 0 只移除 gated 观察口和采样，不改变运行语义。
- 阶段 1 若协议不兼容，只关闭 resume、改为明确的 full reconcile；保留 replay 上限和 gap 记录，**绝不**恢复无界数组。in-band control 未被所有消费者支持前不得宣称已回滚完成。
- 阶段 2 可以停止新 sweeper，但已进入 `evicting` 的 record 仍必须保持唯一 disposal Promise 和禁止新 writer；shutdown 只等待到全局 deadline，超时进入 quarantine，不能通过删 Map 恢复旧的 fire-and-forget 行为。
- 阶段 3 可把受影响查询切回标注过的 full-materialization fallback，但必须保留 `finally` close/release 和峰值诊断；不能直接恢复无清理的 `SessionManager.open`。
- 阶段 4 可关闭 reload 优化或前端 refetch 门，但保留 signature dirty 检测和 conflict 语义；不能恢复“绝对最新”承诺。
- 阶段 5 未通过 ID 压力与重连矩阵前不发布新公式；兼容性回滚可以暂时保留旧 process-lifetime mapping，但必须明确标记内存目标未完成，不上线未经证明的 LRU。
- 阶段 6 只删除已有等价实现且通过回归矩阵的旧路径；发现回归时恢复旧调用点必须同时保留资源关闭、诊断和 gap 安全边界。

任何阶段的回滚都必须先验证旧客户端、活跃 writer 和 shutdown 的安全性；“指标暂时下降”不能抵消静默丢事件、双 writer 或未释放句柄。

## 十一、决策台账

| 编号 | 决策 | 当前值 |
|---|---|---|
| D1 | SDK 边界 | 只消费现有 public/exported API，不改 SDK |
| D2 | live TTL | 30 分钟，60 秒 sweep，无 `<=16` 闸 |
| D3 | 活跃内存上限 | 先做观测和压力告警，不强杀活跃 session；固定上限需要另行决定产品降级语义 |
| D4 | host wire 与 web hub replay | 两者都做条目和字节双上限，gap/restart 明确；具体字节值按阶段 0 的各自事件分布确定 |
| D5 | wire ID map | 禁止任意固定 LRU；session-delete 清理是必要但不充分。稳定 ID、可证明的紧凑映射或持久化索引未完成前，wire map 是已知的长会话 retained-memory 风险，不能宣称总体内存目标完成。 |
| D6 | 最新性 | 无跨进程锁时是检测、吸收、重检的 best effort，不宣称绝对最新 |
| D7 | 查看语义 | 现有需要扩展/UI context 的 attach 仍可 materialize；普通历史 GET 不 materialize |
| D8 | rewrite 检测 | tail ID 是快速提示；signature/fingerprint 才能触发 dirty，不能把 tail ID 写成完整证明 |
| D9 | 诊断入口 | debug/admin gated，只返回计数和估算，不返回内容 |

## 十二、明确不做

- 不修改 omp SDK，不改上游 writer，不做 SDK 内部窗口化。
- 不做跨进程写锁。双活 turn 的最终一致性只做到 host 能观察和重检的范围。
- 不把 RSS 回到基线作为验收，不把 allocator 高水位当成 retained object。
- 不把冷读解析结果作为无界缓存。
- 不为省几 MB 给 wire ID 合约加未经证明的 LRU。

## 十三、相关文档

`docs/memory.md` 已同步为本计划的证据分级：明确 wire replay、ID map、session 回收策略、tree manager 清理和 per-domain containers 的已确认留存；把冷读 RSS 台阶、reader 的 branch/index 限制和 1.4/3.4 GB 归因保留为未确认；把“无 SDK 改动下不能解决的活跃超长 writer”写成非目标。

## 十四、修订记录

- v4.3（本轮恶意审查）：把 host-only 保证限定到单进程和 canonical transcript；把 dispose reject/永不落定、慢消费者、parser 单条记录、诊断自耗、目录队列、restart epoch、move/delete/shutdown 清理和 wire ID map 未闭合作为明确边界；删除重复阶段条款。
- v4.2（host-only 审查收紧）：明确当前已确认的无界留存与未确认的 GB 归因；加入 `#entryTreeFor` 未关闭 manager 的证据；不再把 streaming visitor 误写成任意 transcript 的固定内存方案；确定 wire in-band resync control；补充内部 ID hole、目录过滤、reentrant subscribe、累计 tool partial churn、目录 key 覆盖范围和 dispose 前 idle 通知顺序。禁止用回滚恢复无界 replay。
- v4.1（host-only 审查收紧）：明确当前已确认的无界留存与未确认的 GB 归因；区分 wire/native gap 契约；补充生成 SSE client 的非 2xx 重试陷阱、reentrant subscribe gap、累计 tool partial churn、目录 key 覆盖范围和 dispose 前 idle 通知顺序。禁止用回滚恢复无界 replay。
- v3（代码评审后）：事实基线补 wire 环无界、`loadSessionMessagesReadOnly` 非流式、`wireIdOverrides` 无界、活跃信号、pending dialogs、revert、move 和驱逐竞态。
- v2：整合版。