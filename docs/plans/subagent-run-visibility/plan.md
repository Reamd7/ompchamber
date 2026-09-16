# 子代理（task run）在界面上消失、点不开：修复计划

状态：阶段 0 已实施（2026-09-16，engine.ts + omp-host.engine.test.ts，含 DOCUMENTATION.md 条目）；阶段 0 缺口（持续驻留期间 TTL 掉行）、阶段 1 与阶段 2 的交互项已实施（2026-09-16）；阶段 2 的 wire 缺 id 根因调查仍开放。基线证据采集于 2026-09-15，采集对象是当时在跑的引擎进程（13:41 启动，早于 f773c998 提交）。f773c998 修的是"被查看会话 30 分钟 idle 蒸发"，与本计划互补，不覆盖下面的任何一条。

## 设计原则（2026-09-16 与维护者确认）

消费时重新准备 SDK 资源，而不是维护一个降级的 fallback 投影。宿主代码不感知 SDK 何时回收子代理：所有消费点先查 registry，miss 时再水化（从 transcript 重注册 ref）后重试。SDK 本来就有这条入口——`registry/persisted-agents.ts` 的 `registerPersistedSubagents`，TUI 的 Agent Hub 打开时就是这么做的；omp-host 没用它，自建了弱一等的 historical 行投影（无 tokens、永不 revive、缓存失效bug），本计划改为回到 SDK 的再水化路径。

## 用户看到什么

主代理用 task 工具派出子代理后：

1. 右侧会话面板的"子代理"分区不出现，或出现一会儿又没了。
2. 聊天里 Agent Task 卡片的行点不动。有 ⧉ 图标时点了没反应，没有图标时整行就是死文本。

## 基线证据（2026-09-15 实测）

- 引擎启动后，目录级 agent-runs 的 historical 行停止增长：sharkly-codex 最新一条 04:44:15Z，sharkly-next 最新一条 2026-09-14 08:47:56Z。两个目录当天的子代理 transcript 都晚于这两个时刻，一条都没进 rows。
- 受控探针（本仓库会话里派 scout，run 名 VisibilityProbe）：完成后 registry 行为 `idle` 且带 `childSessionID`；7 分钟后（`task.agentIdleTtlMs` 默认 420000）行从快照里消失，只剩 Main。
- fork 会话 01a0a3ff（MachineHub fork）面板为空的同时刻，它的 run ControlMigration 的行存在，挂在祖先会话 01a084e7（"解决本地开发启动问题"）名下。
- 两条已完成 run 的子会话直读 `GET /api/session/<child>` 返回 404 session not found；仍在 registry 里的 aborted run 返回 200。
- `agent://` 解析返回 501 scheme-not-enabled（`domain-uri.ts:92` 只启用了 local）。
- 截图里无 ⧉ 的那行，像素级确认行尾没有图标；对应 wire 记录缺 `id`。这一条的产生环节未定位，见阶段 2 前置调查。

## 病灶

一条 run 在界面上有三个存活来源，每个都有洞：

1. **registry 活引用**。`engine.ts:538-562` 的 `agentsSnapshot` 把全局 registry 的 ref 归到 owner 会话，owner 必须是 live record。run 完成后 idle 7 分钟被 park，ref 离开 registry，行随之消失。SDK 回收的时机（idle TTL、park、corpse 清理）宿主不掌握，也不该依赖。
2. **没有再水化入口**。SDK 的 `registerPersistedSubagents(registry, sessionFile)`（`registry/persisted-agents.ts:328`）能把一个会话工件目录里的子代理 transcript 重注册为 parked ref（带流式重建的 `AgentHistorySummary`：tokens、耗时、modelRole、outputPath；CAS 防护不覆盖活 ref；墓碑注册为 aborted），TUI Agent Hub 消费它。omp-host 没有任何调用。
3. **自建的 historical 投影是坏的**。`engine.ts:1499-1511` 的 `#ensureDiskRows` 每目录只扫一次，缓存永不失效；首次扫描通常发生在会话刚打开、子 transcript 还没落盘的时刻，此后该目录新产生的 run 永远进不了 rows。`#findSubagentSessionFile`（1452-1457）也吃这份缓存，所以子会话 404。

另有两个独立问题：行的归属按 transcript 目录名反推（`engine.ts:265-272`），fork 继承工件目录后行记在祖先会话名下，而面板按 `row.sessionID === sessionId` 严格过滤（`WorkStatusSubagentsSection.tsx:65-70`）；卡片行体没有 onClick，⧉ 图标要求 wire 记录带 `id` 且 registry 行带 `childSessionID`，点击查不到就静默返回（`ToolPart.tsx:1351-1356`）。

## 阶段 0：消费时再水化（engine，P0）——已实施

落地形态（与下文设计的差异）：物化尾部 fire-and-forget `#rehydrateSubagentRefs(file.path)`（SDK `registerPersistedSubagents` → `#warmChildSessionIds` → `aggregator.refresh`）；全局 registry `onChange` 只对 `registered` 事件调 `#invalidateDiskRowsFor`（按 live 目录的 sessions root 归属删缓存）。触发点二（消费点 miss 重试）由触发点一覆盖：查看即物化，子会话解析走 registry ref，不再需要独立重试层。测试：物化再水化（parked 行 + history.tokens + childSessionID + 子会话 parentID）、registered 事件失效缓存（旧缓存无新行 → 事件后 historical 行出现）。

目录：`packages/web/server/lib/omp-host/engine.ts`

把"行从哪来"改回 registry 单一来源，transcript 通过 SDK 的再水化入口回到 registry：

- **触发点一（主路径）：会话物化时**。物化流程（f773c998 加 model 种子的那段，约 2204 行附近）末尾 fire-and-forget 调 `registerPersistedSubagents(AgentRegistry.global(), <该会话的 transcript 文件>)`，完成后 `aggregator.refresh()`。被查看的会话由此拿到全保真行（parked + history）。
- **触发点二（消费点虚拟化）：miss 时重试**。`#findSubagentSessionFile` 与 aggregator 快照对某个 id miss 时，先对该会话做一次再水化再查（每会话每次消费至多一次，去抖）。这让"打开子会话聊天"这类点式消费不依赖列表刷新时机。
- **触发点三（长尾兜底）：agent-runs GET**。`ensureDirectory` 保留现有磁盘扫描，但补失效：收到 registry `registered` 事件（新 transcript 落盘）时清对应目录缓存。从未被查看的会话靠这层看到存在性行（无 tokens）。若阶段 0 落地后确认触发点一二已覆盖实际使用，这层可以降级或移除，另行评估。
- 不接 `ensureLive`/revive：只读消费（行、子会话冷读）不需要，revive 属于"追问子代理"功能，超出本计划。

已知约束，写进实现说明：

- registry 是进程级扁平命名空间，id 取 transcript 文件名。fork 复制的工件目录里同名 run 先到先得，行归属第一个注册者。不会 clobber 活 ref（SDK 内有 CAS），但归属错位要靠阶段 1 解决。
- `readPersistedAgentHistory` 是流式有界读（前缀 64 行 + 尾部），不物化会话，符合 omp-host-memory 方针；一次调用最多 4 个并发 worker，`shouldContinue` 支持中断。

测试（`omp-host.engine.test.ts`）：

- 磁盘上有子 transcript 的会话，物化后行出现：status parked、history.tokens 非空、childSessionID 来自文件头；子会话 `getSession` 200。
- 再水化不覆盖活 ref：先注册一个 running ref，再跑再水化，断言 ref 原样。
- `registered` 事件后磁盘扫描缓存失效、新 historical 行出现（触发点三）。

验收（手动 smoke，dev app）：派一个 task 子代理，等 7 分钟 TTL 过去，面板仍显示该 run（parked、带 tokens），⧉ 能打开只读子会话；`GET /api/session/<child>` 200。

## 阶段 1：fork 场景的行归属（UI，P1）——已实施

落地形态：`ancestorLineageOf`（纯函数，环/断链容错）+ `primeSubagentLineage`（按 directory+session 缓存，失败不落缓存、不给出权威空集）+ 组件内 ownRuns 为空时才 prime 的 effect 与同步缓存读（capabilityGate 模式，兼容 renderToStaticMarkup 测试）；runs 过滤改为 own 优先、祖先兜底。测试：`WorkStatusSubagentsSection.test.tsx` 的 fork-lineage describe（prime 后祖先行渲染、失败不收养）+ lineage 走链/成环/断链三个纯函数用例。tree mock 走组件导出的 `__setSubagentTreeSourceForTests` 缝（该测试图下对 api 模块的 mock.module 不生效）。

阶段 0 缺口补丁（同日落地）：全局 registry `removed` 事件（非 advisor）触发 `#rehydrateRemovedRun`——仅当 run 的宿主会话仍 live 时，按宿主文件去抖再水化；SDK 无人观看时照常回收，下次物化再补。测试：注销 Regrow 后行以 parked 回归。

目录：`packages/ui/src/components/chat/work-status/WorkStatusSubagentsSection.tsx`

现状是把行严格框在当前会话。改为：`runs` 为空时，取当前会话的 fork 祖先链（`/api/omp/sessions/{id}/tree` 已有 `nodes[].parentId`），把祖先会话名下的行也纳入。只在空集时兜底拉树，常态路径不加请求。行标签沿用 `runTitleFor(agentId) ?? displayName`。

测试（`WorkStatusSubagentsSection.test.tsx`）：mock 树返回祖先链，祖先名下有行时分区渲染且可点；无树接口时退化为现状。

备选（更根治、改动大，先不做）：engine 在 dispatch 时记录 owner 会话而不是从目录名反推。`AgentRef.parentId` 记的是 agent 父（Main），不是会话，要动 SDK 契约，单独评估。

## 阶段 2：Agent Task 卡片交互（UI，P2）——交互项已实施

落地形态：`resolveTaskRunOpenState`（taskToolModel 纯函数：run id 存在且 registry 行带 childSessionID → open）驱动行渲染——open 行整行可点（role=button、Enter/Space、与 ⧉ 同一 openSubagentRun 通道，data-run-id 保留）；不可打开的行渲染禁用态 ⧉ 加 `chat.toolPart.taskAgent.runUnavailable` 提示（12 个语言文件齐译），不再静默死链。memo 比较器纳入 childSessions 身份。测试：taskToolModel.test.ts 三个 open-state 用例。wire 缺 id 的根因（async job reportProgress 到 host 转发之间哪一层丢）仍待真实 UI 会话抓帧定位；在那之前缺 id 的行落入可见的禁用态。

目录：`packages/ui/src/components/chat/message/parts/ToolPart.tsx`、`taskToolModel.ts`

- 行体可点：registry 行带 `childSessionID` 时整行触发 `openSubagentRun`（与 ⧉ 同一 handler），`data-run-id` 保留供 `revealRunCard`。
- 查不到时不再静默：⧉ 渲染为禁用态加 tooltip（"运行记录不可用"），点击行无动作但状态可见。
- 行标识：`title`/`aria-label` 用 dispatch 标题（`readTaskRunTitle` 链路已有），行首 agent 类型名保留。

前置调查（阻塞本阶段，不阻塞 0/1）：从 UI 发一次真实 task 调用，抓 `message.part.updated` 帧，确认 progress 记录在哪一层丢了 `id`（eval 桥派发不落 transcript part，复现不了；怀疑在 async job 的 `reportProgress` 到 host 转发之间）。阶段 0 落地后顺带验证。

## 非目标

- 不改 `task.agentIdleTtlMs` 默认值。再水化后 TTL 过期只是行从 idle 变 parked，不再丢。
- 不接 `ensureLive`/revive（只读消费不需要）。
- 不启用 `agent://`（`ENABLED_READ_SCHEMES` 维持 local-only，属 P1 URI 计划范围）。
- 不动 f773c998 的会话驻留逻辑。

## 顺序与验证

0 → 1 → 2，各自独立可合。每阶段：`bun test` 对应测试文件、`bunx oxlint` 改动路径、dev app 手动 smoke（重启后按上文验收步骤走一遍）。阶段 0 是其余两阶段的地基：没有再水化，归属和交互修了也只在 SDK 回收前有效。
