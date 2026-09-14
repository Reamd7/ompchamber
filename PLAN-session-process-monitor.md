# Session 进程监控 — 实施计划

## 需求

在现有 work-status 面板内，按 session 维度监控进程：

- 追踪 session 的 `bash` 与 `eval` 工具调用产生的进程。
- 进程的出现与退出/消失实时可见。
- 覆盖所有后代：fork 子进程、shell `&` 后台任务、`nohup`、detach 进程、
  eval kernel（python `Bun.spawn` kernel、`__omp_worker_js-eval` 自启动进程）。
- 点击进程/调用可看输出回放（只读）。
- 每个进程/调用显示 CPU 与内存占用。
- 提供 kill 操作（TERM → 宽限 → KILL，支持进程组/树）。
- 归不了属的进程照常显示、标"归属不确定"、仍可 kill——不静默丢弃
  （已确认接受 best-effort 标记）。
- 作为 `WorkStatusPanel` 的新区块集成，不做独立页面。

## 摸底结论（决定设计的硬事实）

- 所有 bash/eval 执行都发生在 **omp-host 进程内**（`@oh-my-pi/pi-natives`
  的 brush `Shell`、python kernel 走 `Bun.spawn`、`pty:true` 走 PtySession）。
  所有 spawn 出来的进程都是 omp-host 的后代 → 枚举完备。
- SDK **不暴露** spawned pid。`AsyncJobManager` 只挂到第一个 top-level
  session（R12，`jobs.v1` 目前是结构化 501）。主要泄漏路径——前台命令里
  `nohup x &` 的残留——SDK 层看不到，必须从 OS 层观察。
- `#handleEngineEvent` 已能收到 `tool_execution_start/update/end`，携带
  `{hostSession(sessionId,directory), toolCallId, toolName, args,
  partialResult, result, details}`。
- `toolName === 'bash'` 的 args 带 `command/cwd/env`；
  `toolName === 'eval'` 覆盖 python/js eval。两者都在工具调用窗口内
  spawn → 同一套机制覆盖。
- pi-natives 的 `Process` 在 omp-host 代码里按包名 import 不到
  （isolated 依赖布局），但可以用 `createRequire` 锚定到 SDK 的
  package.json 拿到——`omp-host-natives.js` 的 `resolvePackageDir`
  已有同款 store 遍历模式。无需改 package.json。
- 传输先例：`agentRuns` —— directory 粒度快照 GET + revision bump 事件 +
  能力位 + UI store 按 `runtimeKey::directory` 键控。

## 架构

```
SDK tool events ──► ProcessLedger（调用账本 + 归属判定）◄── Poller（进程快照 diff）
                        │
                        ▼
        domain-processes（GET 快照 / output / kill action）
                        │
        omp.processes.updated（directory scope，500ms 合并）
                        │
        useOmpProcessesStore ◄── processesRevision（reducer）
                        │
        WorkStatusProcessesSection + 详情 Dialog
```

## Server：`packages/web/server/lib/omp-host/process-ledger.ts`（新，纯逻辑）

### 数据模型

- `TrackedInvocation`：`{key, sessionID, directory, kind: 'bash'|'eval',
  command, cwd?, startedAt, endedAt?, exitCode?, isError?,
  outputTail（≤64KB）, outputTruncated, jobId?}`
- `TrackedProcess`：`{pid, ppid, pgid?, argv[], cwd?, firstSeenAt, exitedAt?,
  rssBytes, cpuPercent, invocationKey, attribution, killedByUser}`
- `attribution` ∈ `window | argv | parent | job | unattributed`

### 归属规则（优先级序）

1. ppid 命中已归属 pid → `parent` 继承（fork 链全覆盖）。
2. firstSeen 落在唯一打开的调用窗口内
   （start → end + ~3s 宽限，兜 `&` 尾部 spawn）→ `window`。
3. 多窗口重叠 → argv↔command token 匹配消歧 → `argv`；POSIX 再叠
   `/proc/<pid>/cwd` == 调用 cwd。
4. managed job（`details.async.jobId`，仅首个 session 有 manager）：
   running 状态的 job 视为持续开窗 → `job`。
5. 都归不进 → `unattributed`，记 `candidateSessionIds`（当时开窗的会话
   集合）。照常展示 + 标"归属不确定" + 可 kill。

### 排除项

- 窗口外出生的 spawn（MCP/LSP/daemon broker）不进账本。
- js-eval kernel 在 eval 窗口内 spawn → 进账本，按 kind 标注。
- 采样器自身 spawn 的进程（`ps`/`powershell`）会被自己看见 → 按 reader
  pid + argv 模式自排除。

### 轮询

- 仅当「打开的窗口 ∪ 存活被追踪进程」非空时跑；全空闲自停——
  "只有一次性命令"的场景零开销。
- 间隔 ~2s（POSIX）/ ~4–5s（win32）。
- pid 消失 → 打 `exitedAt`，条目保留一段时间让"消失"可见。

### 枚举

- 树遍历：pi-natives `Process.children()` 递归，经 `createRequire` 锚定
  `@oh-my-pi/pi-coding-agent/package.json` 导入（复用 `resolvePackageDir`
  的 store 遍历模式）。全平台零 spawn。
- 仅对**已追踪 pid** 采资源：
  - Linux：读 `/proc/<pid>/stat` + `status`。
  - macOS：`ps -o pid,rss,time -p <pids>`（一次 spawn）。
  - win32：`Get-Process -Id <list>`（一次 spawn）。
- CPU%：Δ(utime+stime) ÷ 墙钟 ÷ 核数，归一 0–100；内存 = RSS bytes。

### Kill

- POSIX `kill` → 宽限 → `kill -- -<pgid>` / SIGKILL；
  win32 `taskkill /PID /T /F`。
- managed job 额外走 `manager.cancel(jobId)`（经
  `hostSession.agentSession.asyncJobManager`）。

### 输出

- `tool_execution_update` 的 partialResult 持续写入 invocation 的
  outputTail；`tool_execution_end` 写终态。
- detach 后的新 stdout 不可达（fd 在原 shell 手里）；`nohup` 目标
  尽力 tail `nohup.out`。

## Server：传输与接线

- `domain-processes.ts`（新，照 `createUriDomain` 模式）：
  - `GET /omp/processes?directory=` → `{revision, entries[]}`
  - `GET /omp/processes/output?directory=&key=` → `{output, truncated, live}`
  - `POST /omp/processes/{sessionID}/{key}` `{kind:'kill', pid?}`
- `engine.ts`：ctor `createProcessDomain({ledger, publish, features})`；
  `#handleEngineEvent` 三个 tool 事件 case 各加一行；dispose 接线。
- `endpoints.ts`：`engine.processDomain.mount(route)`。
- `omp-parity.ts`：`'processes.v1': true`（运行期探测：watcher 初始化
  失败则关闭能力位，而不是假装空数据）。
- `omp-event-registry.json`：
  `"omp.processes.updated": {durable:true, scope:'directory',
  snapshotEndpoints:['/api/omp/processes'], since:'1.0'}`；
  `omp-bootstrap-matrix.json` + `event-dispositions.json` 对应条目；
  `bun run check:events` 兜底。
- `omp-host/DOCUMENTATION.md` 加模块段。

### 为什么 directory 粒度而非 session 粒度

账本内部本来就是 `directory\0sessionID` 键。传输层选 directory 因为：

1. unattributed 行的归属是"会话集合"不是单 session——session 粒度下
   无处安放（在多个 session 响应里复制同一行 = 多个权威源）。
2. omp SSE 通道（`/omp/events?directory=`）与 web hub 按 directory
   路由；`agents.updated`、`settings.updated`、`dialog.*` 都走它。
3. 一份快照喂饱 directory 下所有 session 面板
   （`runtimeKey::directory` store + `filter(sessionID)` 先例）。

代价：session B 的变动会触发 A 的一次 refetch——被 500ms 合并兜住。
可选优化（不进 v1）：payload 带 `sessionIDs[]`，reducer 只 bump 相关会话。

## UI

- `lib/api/omp.ts`：`OmpProcessEntry`/`OmpProcessMember` 类型 + zod
  schema + `createOmpProcessesAPI` + `OMP_ENDPOINTS` 加项。
- `stores/useOmpProcessesStore.ts`（新）：照 agentRuns store——
  `runtimeKey::directory` 键、快照 revision 对事件 revision 强制
  refetch、失败不落空数据（不装权威空态）。
- `sync/omp-event-reducer.ts`：`OmpDomainTracking` 加
  `processesRevision`，`case 'omp.processes.updated'`。
- `WorkStatusProcessesSection.tsx`（新）：id `processes`、icon `pulse`。
  行 = invocation：命令截断 + `n` 进程 + 聚合 CPU%/RSS + tone
  （running=info / exited=muted）+ 归属不确定 badge。行内
  `WorkStatusRowAction` 快捷 kill；点击开详情。
- `WorkStatusProcessDialog.tsx`（新）：成员进程表（pid/argv/rss/cpu/
  状态/单独 kill）+ 输出回放（output 端点，开启期间 2s 轮询）+ kill 全部。
- `sections.ts` 加 id 与 label key；`WorkStatusPanel.tsx` 挂载（tasks
  之后）；presence 上报；`useOmpFeatureEnabled('processes.v1')` 门
  （非 omp 运行时自动隐藏）。
- i18n：en/zh-CN/zh-TW 三套键 + sections dialog `settingsItem` 注册。
- `work-status/DOCUMENTATION.md` 更新。

## 诚实边界（写进模块文档）

- `setsid`/双 fork 守护进程 POSIX 下 reparent 出树 → 显示"已消失"但
  实际活着（cgroup 才能抓，不做）。
- detach 后的新 stdout 不可达；`nohup.out` tail 仅尽力而为。
- `async:true` job 运行中的输出 SDK 不暴露公开读口 → 完成时才有终态文本。
- 非 omp 运行时：能力位关闭。
- host 重启账本清空（进程多数也随 kill-on-drop 死）。

## 预计性能损耗

空闲 ≈ 0；活跃 ≈ 可忽略——除了 Windows，成本大头是 PowerShell 启动，
用更慢的轮询节奏压住。

| 项 | 空闲时 | 活跃时（有开窗调用或存活被追踪进程） |
|---|---|---|
| 树枚举 | 不跑 | 每轮询一次 `children()` 递归，native，几十~几百节点 <1ms |
| 每 pid 资源读取 | 不跑 | Linux `/proc` 文件读（µs/pid）；macOS 一次 `ps -p` spawn ~10–25ms；win32 一次 `Get-Process` spawn ~150–350ms |
| 事件 | 无 | 仅账本变化时发，节流 ≥1s，payload 几百字节 |
| 工具事件钩子 | O(1) map 操作 | 同左 |
| 内存 | 0 | 每 session ≤50 条 invocation（LRU 淘汰已退出项）；outputTail ≤64KB，仅在有存活进程或结束后 ~10min 内保留 → 每 session ~3MB 上限 |

量级感受：Linux 活跃轮询 <0.1% 单核；macOS ~1%；win32 ~4–8% 单核
（所以 win32 用 4–5s 节奏）。数字是估算——落地时给 `/omp/diagnostics`
加 `pollMs`/`pollCount`/`spawnCount` 计数器实测，真超了再调。

降级诚实性：枚举/采样连续失败 → 条目标 `statsStale` 而非编数字；
轮询器退避，不允许拖死 host；watcher 初始化失败 → 该实例
`processes.v1` 关闭，而不是假装空。

UI 侧：revision bump → 每个受影响 directory 一次小 GET；详情 Dialog
开着才 2s 轮询输出端点，关闭即停；行数有界、选择器 memo，不往
渲染/同步热路径加东西。

## 文件清单

新增：
- `packages/web/server/lib/omp-host/process-ledger.ts`
- `packages/web/server/lib/omp-host/domain-processes.ts`
- `packages/web/server/lib/omp-host/process-ledger.test.ts`
- `packages/web/server/lib/omp-host/domain-processes.test.ts`
- `packages/ui/src/stores/useOmpProcessesStore.ts`
- `packages/ui/src/components/chat/work-status/WorkStatusProcessesSection.tsx`
- `packages/ui/src/components/chat/work-status/WorkStatusProcessDialog.tsx`
- store/reducer/section 的 focused 测试文件

修改：
- `packages/web/server/lib/omp-host/engine.ts`、`endpoints.ts`、`omp-parity.ts`
- `omp-event-registry.json`、`omp-bootstrap-matrix.json`、`event-dispositions.json`
- `packages/web/server/lib/omp-host/DOCUMENTATION.md`
- `packages/ui/src/lib/api/omp.ts`
- `packages/ui/src/sync/omp-event-reducer.ts`
- `work-status/sections.ts`、`WorkStatusPanel.tsx`
- `packages/ui/src/lib/i18n/messages/{en,zh-CN,zh-TW}.ts`（+ settings item）
- `work-status/DOCUMENTATION.md`

## 验证

- `bun test server/lib/omp-host/process-ledger.test.ts`：注入假 snapshot，
  覆盖四种归属、退出标记、kill 路径、合并节流。
- `domain-processes.test.ts`：路由/501 门/快照形态（照 domain-uri.test
  写法）。
- UI vitest：store、reducer、section。
- `bunx oxlint` 新文件 + `tsc -p tsconfig.server.json` + UI type-check +
  `bun run dead-code`（新增文件）。
- 手动：`sleep 600 &`、`nohup python -m http.server &`、dev server、
  python eval 各跑一遍——看出现/消失/输出/kill/CPU 内存。
- 实施时先实测验证 brush 子进程是否 omp-host 直接子孙（Rust 侧无源码；
  若中间有 supervisor 层，`parent` 继承规则照样兜住，只调 pgid 分组）。

## 开工前需加载的 skills

`openchamber-change-discipline`、`ui-api-decoupling`、`sync-state-invariants`、
`relay-transport`、`theme-system`、`locale-ui-patterns`、
`performance-engineering`。
