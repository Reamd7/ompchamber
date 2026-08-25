# 07 · 域 G:OpenCode 残留清除(Residue Removal)

状态:设计阶段(2026-08-20 修订二轮:落位二轮评审 M7/M8 相关裁决 —— `session.error` 终裁删除、G08 HOLD 清零、G10 终局随 04 章 descope 收敛、vendored gen 全书归一;标注 REVISED/R2 的块为本轮改动)
日期基线:2026-08-20(行号以当日工作区复核为准,引用格式 `file:line`)
上游契约:遵守 `00-MASTER.md` D1–D6(含 D6 冻结契约 R1–R12,冲突以总纲为准);本章是 P3 大扫除的执行清单,同时给出各删除项的前置依赖与回滚单元。

---

## 1. 域概述与边界

**本章管什么**:把 OpenChamber 中"OpenCode 产品骨架"遗留、且 omp/TUI 没有对应概念的 13 组残留,逐项给出删除清单(精确到文件与行)、omp-host 桩的过渡处置、vendored wire 类型的处置、i18n 清理、用户可见变化、风险,以及跨 P0–P3 的删除顺序 DAG 与回滚策略。

**本章不管什么**:
- **替代品的设计**。删 permission/question 之后审批/ask 桥长什么样 → 第 03 章;`!` bash 执行面 → 第 02 章;MCP 真实状态 → 第 04 章;retry/compaction/error 事件映射 → 第 05 章;roles 取代 build/plan → 第 01 章;设置面迁移 → 第 06 章。本章只定义"什么时候删、删哪些、怎么回滚"。
- **OpenChamber 原创面的取舍**(WorkStatusPanel、multirun、magic slash commands 等)→ 第 08 章。本章删除组若触及原创面(如 WorkStatusSubagentsSection 的 blocker 读出),只删 OpenCode 协议挂钩,面板本体保留。
- **vendored wire gen 的机械修改**。按 D1,wire gen 不手改;本章对 gen 的全部处置是"停止消费 + 契约守卫",见 5.0。

**与其他章的接口**:本章每个 GAP 标注"解锁条件"(哪章落地后才能删);第 05 章的事件处置表(D2:每个 SDK 事件必须有显式处置)反过来决定本章 G08/G09 中"看似死代码、实为潜在载体"的消费者去留(**R2 补记:05 章 v3 已定稿,该反向依赖全部收敛,见 5.8**)。删除项若需事件回填,一律引用 05 章唯一事件注册表(`OmpEventBus → /api/omp/events`,R1),本章不定义任何事件通道;`message.part.removed` 是唯一的"残留面复用"例外(05 章 retry 超越撤回,R5),见 5.8。

---

## 2. 现状分析

### 2.1 残留的形态学

OpenCode 残留以三种形态存在,删除动作各不相同:

1. **host 桩**:`packages/web/server/lib/omp-host/endpoints.js` 中一批 501(`unsupported`)或"权威空响应"路由 —— share(326-331)、upgrade(168)、shell(294-296)、permission(338-341)、question(342-344)、`GET /diff` 恒 `[]`(332)、`/command` 恒 `[]`(393)、`/lsp` 恒 `{servers:{}}`(395)、`/formatter` 恒 `{}`(396)、`/mcp` 全家空/no-op(424-430)。
2. **UI 消费链**:reducer case、store、组件、tray/VS Code 桥、i18n 键,消费着**从未被生产**的事件或**永远为空**的端点(证据见各组)。
3. **纯契约死重**:vendored `types.gen.d.ts` 中零生产者、零消费者的类型 —— `session.next.*` 约 30 个事件(639-978)、`tui.*` 桥事件(1159-1187)、`permission.v2.*`(4603-4639)、`integration.*`/`catalog.*`(571-583)。

**生产者基线(事实)**:omp-host 只发出 `session.created/updated/deleted/status/idle`、`todo.updated`、`message.updated`、`message.part.updated/delta`(engine.js:579/594-595/604 与 projection.js:407/460-503)。wire Event union 里其余一切事件类型在 OpenChamber 运行时中都没有生产者。**例外(规划中,R5;R2 改相)**:`message.part.removed` 将由 05 章 **P2 门控首产**为 retry 超越撤回载体(05 §5.3.4 三道门:投影侧 syntheticSettledPartIds 记账 / reducer 壳保留不变式 / 三类测试;P1a 仅点亮 `session.status{retry}`,overlay 走 `omp.retry.started`,零 wire 变更)—— 首产后移出"未生产"集合(5.8)。

### 2.2 删除候选分组的现状证据(概览)

| # | 组 | 现状要点(证据) |
|---|---|---|
| R1 | 会话分享 share | 端点 501(endpoints.js:326-331);Header 菜单+复制链接(layout/Header.tsx:1100-1101,1155-1186);侧栏菜单(SessionNodeItem.tsx:938-953);SDK 调用 session-actions.ts:1207-1229;`Session.share` 字段(types.gen.d.ts:94-96);自动清理豁免 useSessionAutoCleanup.ts:48;11 个语言包的 share/unshare 键 |
| R2 | OpenCode 升级面 | wire `/global/upgrade` 501(endpoints.js:168)+ sdk.gen.js:626-629(零调用方);**活的** OpenChamber 托管升级链:`/api/opencode/upgrade(-status)`(web/server/lib/opencode/routes.js:162-174)+ OpenCodeUpdateToast.tsx(runtimeFetch :57)+ `opencodeUpdate.toast.*` i18n;gen `Config.share/autoshare/autoupdate`(types.gen.d.ts:1557-1562) |
| R3 | permission 协议 V1+V2 | 端点空答(endpoints.js:334-341,注释明言"until the approval bridge lands");V1 事件消费 event-reducer.ts:521-547;V2(`permission.v2.*`)连消费者都没有;全链 UI:permissionStore.ts + `/api/permission-auto-accept` 服务端运行时(web/server/lib/permission-auto-accept/runtime.js:249-263)、PermissionCard、auto-accept 按钮(ChatInput.tsx:2436-2440)、VS Code 桥(vscode-permission-auto-accept.ts)、tray 审批(useTraySync.ts:595-600)、eviction 阻塞豁免(eviction.ts:15-24)、`Session.permission`(types.gen.d.ts:114)、agent 权限编辑器与摘要(AgentPermissionsEditor;ModelControls.tsx:1438-1447) |
| R4 | question 协议 | 端点空答(endpoints.js:342-344);reducer event-reducer.ts:549-576;QuestionCard、question-recovery、发送时自动拒答(session-actions.ts:1725-1785)、toasts(sync-context.tsx:1573+) |
| R5 | shell 会话 | 端点 501(endpoints.js:294-296);client.shellSession(client.ts:1011-1031);`inputMode:'shell'` 路由(session-ui-store.ts:140-148);`UserShellActionPart` 渲染(MessageBody)。**注意:omp TUI 有 `!` 本地执行,语义不同,见 3.2** |
| R6 | tui-bridge 事件 | `tui.command.execute/toast.show/prompt.append/session.select` 仅存在于 gen(types.gen.d.ts:1159-1187),grep 全库零消费 |
| R7 | session.next.* durable stream | ~30 事件类型 + `SessionHistory`/`SessionDurableEventStream`(types.gen.d.ts:639-978,2253-2257),零生产者零消费者 |
| R8 | 未生产 wire 事件 | `session.compacted`(1264)、`session.error`(997;消费链是活代码:event-pipeline.ts:491-496 终态判定、sync-context.tsx:1603-1623 错误通知)、`session.diff`(990;reducer 309-313,`/diff` 恒空)、`message.removed`(617;reducer 386-393)照删(**R2 终裁收敛,HOLD 全部解除**:error 见 05 §5.11 不生产裁决 + 5.8 替换面;compacted 见 05 §5.5 compaction 走 omp 轨 + 合成消息;message.removed 见 05 §5.3.2 明文不用);`message.part.removed`(632;reducer 457-473)**已裁决复用**(D6-R5:05 章 P2 门控首产,05 §5.3.4),非残留、不入守卫;`vcs.branch.updated`(reducer 514-519,事件从未发,UI 走轮询 endpoints.js:468-472)、`lsp.updated`(reducer 577-581)、`project.updated`(reducer 195-197)、`integration.*`/`catalog.*`(571-583) |
| R9 | retry status 形状 | `SessionStatus {type:'retry',attempt,message,next}` 全链:event-pipeline.ts:107-119 归一化、event-reducer.ts:101-106 等值比较、global-session-status、client.ts:1073-1109 类型;omp-host 从不产生(引擎侧 auto_retry_* 在 engine.js:611-612 default 分支被丢弃) |
| R10 | MCP 空面 | `/mcp` 返回 `{servers:{}}` + connect/disconnect no-op(endpoints.js:424-430);UI 跑完整连接/OAuth 流:useMcpStore.ts、McpPage/McpSidebar/McpOAuthCallbackPage/startMcpAuthorization、WorkStatusMcpSection |
| R11 | build/plan 二分 | `/agent` 制造 builtin build+plan(endpoints.js:361-376);engine `planYolo` 映射(engine.js:464)与 `'build'` 默认(engine.js:639);UI 硬回退 ModelControls.tsx:523-525/971/1291-1295、mobileControlsUtils.ts:34-37、agentColors.ts:20 |
| R12 | `/command` 空端点 | 恒 `[]`(endpoints.js:393);UI 侧 `dirState.command` 消费(session-ui-store.ts:156-163);CommandsPage 本身是 OC 原创自定义命令管理(留,归 08 章) |
| R13 | BehaviorPage OpenCode 路径 | `AGENTS_MD_PATH='~/.config/opencode/AGENTS.md'`(BehaviorPage.tsx:33)+ `/api/behavior/agents-md` 读写(127-136,231-233)。**修正:该文件并非死路 —— omp 的 OpenCode 兼容 discovery provider 仍读它**(证据 REVISED,见 3.3) |

### 2.3 与原创面的纠缠(删除时的保护名单)

- `WorkStatusSubagentsSection` 是 OC 原创面板,但其 "needs permission / asked question" 读出(WorkStatusSubagentsSection.tsx:19-38,86-101)挂在 R3/R4 的 state 上 —— 只删读出与 state,不删面板。
- 通知系统的 completion/error 触发是 OC 原创能力(notifications/DOCUMENTATION.md:48),但其 permission 抑制逻辑与 `ompchamber:permission-auto-accept.updated` 广播(runtime.js:65-68)随 R3 删除。
- `usePendingOpenCodeRestartStore`、`reloadOpenCodeConfiguration`、`deferredRestart` 被 R2/R12/R13 三组共享,删除顺序需协调(见 5.14 DAG)。

---

## 3. 目标语义(简)

### 3.1 omp/TUI 侧的"负空间"

omp 没有下列任何概念,TUI 源中无对应实现(这是本章的规格基线):

- **share/公开链接**:TUI 无;master D3 明确 share(cloud)删除,omp 自有加密快照 /share 另议(低优,不在本章)。
- **API 驱动升级**:omp 随宿主安装升级 —— omp-host 501 文案已陈述此语义(endpoints.js:168 "The omp host upgrades with the OpenChamber application")。
- **外部 permission/question 协议**:审批是引擎内部策略(approvalMode/tier + tool_approval),不经 wire 协议外化(总纲 D3;第 03 章建桥)。
- **tui-bridge / durable event sourcing**:不存在;omp 的事件面就是 `agent-session-events.ts` 的 union,投影由宿主决定。
- **MCP 外部管理面**:TUI 侧 MCP 经配置加载(SDK `discovery/opencode.ts:9-13` 能力声明、`:240-243` 从 `opencode.json(c)` 的 `mcp` 键跨层深合并加载 —— 引擎内可以有真实 MCP server;原生面另有 `discovery/builtin.ts:236-239` 的 `.omp/mcp.json`),但无 OpenCode 式 connect/disconnect API;真实状态暴露与可执行端点归第 04 章(R12:04 章定义前 UI 只读、开关禁用)。

### 3.2 `!` 模式的 omp 真实语义(R5 的关键修正)

omp TUI **有** `!` 前缀输入,但它是**本地执行**,不是 OpenCode 的 model-中介 shell 会话:

- 输入判定:`input-controller.ts:797-813` —— `!cmd` 普通执行、`!!cmd` 排除出上下文;运行中去重保护(:802-806)。
- 执行:`command-controller.ts:1138-1191` → `session.executeBash(command, onChunk, {excludeFromContext, useUserShell:true})`,流式输出进 `BashExecutionComponent`,`cd` 结果可持久化会话工作目录(:1174,1200-1216)。
- SDK 面:`AgentSession.executeBash`(`agent-session.ts:7446-7452`)、`recordBashResult`(:7455)。

因此 R5 的删除目标是 **OpenCode wire 的 `/session/{id}/shell` 通道**,不是 `!` 交互本身;`!` 的 omp 原生重建(经 RuntimeAPIs 包 executeBash)由第 02 章设计,本章只锁依赖边。

### 3.3 AGENTS.md 的 omp 真实语义(R13;证据 REVISED 2026-08-19)

> 原稿以 `config/opencode.ts:104-125` 为据,该路径不存在;真实文件 `discovery/opencode.ts` 的 104-125 区间是 `{env:}`/`{file:}` 配置变量展开,与 AGENTS.md 无关(D6-R12 作废原证据)。以下为重新查明后的 omp 上下文文件发现顺序,全部经源码复核。

omp 的 AGENTS.md 不是单一文件,而是一组 context-file discovery provider,按 priority 竞争、按 scope 去重(`capability/context-file.ts:31-36`:user 级全局仅保留一份、project 级每深度一份,同 scope 内高优先级遮蔽低优先级):

| provider | 路径 | priority | 证据 |
|---|---|---|---|
| omp 原生(builtin) | 用户级 `getAgentDir()/AGENTS.md` + 项目级最近 `.omp/AGENTS.md` | **100** | `discovery/builtin.ts:910,923`(注册 `:941-945`) |
| Claude Code | `.claude/CLAUDE.md`(user+project) | 80 | `discovery/claude.ts:530-536` |
| `.agent/.agents` 目录 | 用户级 `~/.agent[s]/AGENTS.md` + 项目走查 | 70 | `discovery/agents.ts:290-315`(PRIORITY `:28`) |
| OpenAI Codex 兼容 | 用户级 only `~/.codex/AGENTS.md` | 70 | `discovery/codex.ts:54-62`(注册 `:484-489`) |
| **OpenCode 兼容** | **用户级 only `~/.config/opencode/AGENTS.md`** | **55** | `discovery/opencode.ts:163-182`(loader),注册 `:470-476`;基路径 `.config/opencode` 见 `discovery/helpers.ts:55-59,92-99` |
| GitHub Copilot | `COPILOT_CUSTOM_INSTRUCTIONS_DIRS` 内 AGENTS.md | 30 | `discovery/github.ts:81-88` |
| 独立 AGENTS.md | cwd 向上走查(不含 home 自身) | 10 | `discovery/agents-md.ts:43-49,101-105` |

两点对 R13 有决定性影响:

1. **omp 原生用户级文件是最高优先级(100),存在即遮蔽 OpenCode 兼容文件(55)与 `.agent/.agents`(70)/codex(70)的同 scope user 文件**(`capability/context-file.ts:31-36` 遮蔽语义)。产品主文件指向原生面与总纲裁决原则一致。
2. **`getAgentDir()` 不恒等于 `~/.omp/agent`** —— 它是 profile-scoped:`setProfile(name)` 后解析为 `~/.omp/profiles/<name>/agent`,未激活 profile 时为默认 `~/.omp/agent`(`packages/utils/src/dirs.ts:452-484,495-498`;`discovery/builtin.ts:65-67`、`discovery/helpers.ts:93-95` 注释明确该语义)。UI 不得硬编码路径,必须经服务端解析(见 5.13)。

所以 BehaviorPage 编辑的 `~/.config/opencode/AGENTS.md` 今天仍被引擎消费(OpenCode 兼容 provider,用户级 only),R13 的正确处置维持"改指向 omp 原生路径 + 一次性迁移",不是裸删;目标文件的终裁与 06 章设置面联动(开放问题 6)。

---

## 4. 差距清单

| GAP | 内容 | 分类 | 优先级 | 风险 | 解锁条件 |
|---|---|---|---|---|---|
| GAP-G01 | 删会话分享全链(菜单/端点/字段/清理豁免/i18n) | 删 | P3(可提前至 P1) | 低 | 无(零耦合) |
| GAP-G02 | 删 OpenCode 升级面(wire 桩 + 托管升级路由 + toast + i18n) | 删 | P3(可提前) | 低 | 确认桌面打包不再托管外部 OpenCode 二进制(开放问题 4) |
| GAP-G03 | 删 permission 协议链 V1+V2(host 桩、reducer、store、服务端 auto-accept 运行时、卡片/toast/tray/VS Code 桥、agent 权限编辑器、`Session.permission`) | 删 | P3 | **高**(触及 ~30 文件、zustand persist、VS Code/tray 双运行时、eviction 不变量) | 第 03 章审批桥落地 |
| GAP-G04 | 删 question 协议链(端点、reducer、卡片、恢复、发送时自动拒答) | 删 | P3 | 中 | 第 03 章 ask 对话框桥落地 |
| GAP-G05 | 删 OpenCode shell 会话通道(端点 501、client.shellSession、inputMode 路由、UserShellActionPart) | 删 | P3 | 中(`!` 交互暂缺替代) | 第 02 章 `!` 本地执行面(executeBash 经 RuntimeAPIs)落地 |
| GAP-G06 | tui-bridge 事件契约(tui.command/toast 等):停止消费 + 守卫 | 删(消费侧为空,纯守卫) | P1 | 极低 | 无 |
| GAP-G07 | session.next.* durable stream 类型:裁决"不采用",加契约守卫 | 删(裁决+守卫) | P1 | 极低 | 无 |
| GAP-G08 | 未生产 wire 事件分诊(REVISED R2:HOLD 清零):diff/message.removed/vcs/lsp/project/integration/catalog/**compacted/error** 全删;`message.part.removed` **不删**(D6-R5:05 章 P2 门控首产) | 删+留 | P3(diff 等可提前;error 链排在 05 章 P1 投产后) | 中(error/message.removed 消费链是活代码) | 05 章 v3 已定稿:error 不生产(05 §5.11)、compaction 走 omp 轨(05 §5.5)、retry 回收不产消息壳删除(05 §5.3.2);`session.error` 链删除的运行时前置 = 05 章 P1 事件面投产;`message.part.removed` 归 05 章注册表管辖 |
| GAP-G09 | retry status 形状:**改用途**为第 05 章 auto_retry 的 wire 载体(D6-R12/总纲 §7.6 已裁决,开放问题 1 关闭),不删 | 改(留) | 裁决已落定;接线随 05 章 P1a | 低 | 无(总纲 §2/§7.6 已按复用修订) |
| GAP-G10 | MCP 空面:P1 只读化 + 全部操作开关禁用(capabilities 门控,R2/R12);**REVISED R2:04 章 descope 后恒只读/禁用为长期稳态,UI 面保留**;"管理启用 or 全删"归 MCP 专项轮次(04 §5.7.5),本轮无该分叉 | 改(只读化;本轮不删) | P1(只读化即终态) | 中(误导性 no-op 交互) | 无(04 章 R2 已裁决 descope) |
| GAP-G11 | build/plan 二分残迹清除(`/agent` 制造、planYolo、`'build'` 回退、配色、移动端工具);plan 合成文本协议的停产停用随 02 章模式端点上线(同发布),P3 仅清扫 flag/文件/i18n(D6-R12) | 删 | P3 | 中(默认 agent 解析级联改动) | 第 01 章 roles + 第 02 章 custom agents/模式端点落地 |
| GAP-G12 | 删 wire `/command` 空端点与同步命令源;CommandsPage(OC 原创)保留归 08 章;新命令语义走 `/api/omp/commands`(08 章,R3/R12),wire 端点保持空返直至本章删除 | 删 | P3 | 低 | 第 06 章确认 `/config` PATCH 去向(开放问题 6);08 章 `/api/omp/commands` 上线 |
| GAP-G13 | BehaviorPage 指令文件改指向 omp 原生用户级 `getAgentDir()/AGENTS.md`(服务端解析,profile-scoped,见 3.3 REVISED),旧路径一次性导入;清 OpenCode 重启机器(共享部分随 R2/R12 协调)。**2026-08-24:改指向+取值语义已落地,`optimizeSystemPrompt` 死开关整链删除;重启机器仅剩 agents-md 保存路径一个 `behavior` 消费者(增量见 §5.13)** | 改 | P3(尾款:重启机器清扫) | 低 | 第 06 章设置代理定稿(目标文件终裁) |

分类说明:G06/G07 的"删"发生在契约纪律层(守卫),不是文件删除;G09 是全章唯一"留"(总纲 §7.6 已裁决复用,开放问题 1 关闭);`message.part.removed` 同为"留",但它是 05 章 P2 门控投产的活 wire 契约(D1 例外/R5,05 §5.3.4),不是本章 GAP 的处置对象;G08 的 compacted/error 由"删+留"收敛为纯"删"、G10 由"删(两段)"收敛为"改(恒只读)"(R2)。

---

## 5. 设计方案

### 5.0 总策略:三步走、wire gen 不动、桩先诚实后消失

**三步走(每组通用)**:

1. **Step A「UI 停消费」**(纯 UI commit):删组件/store/reducer case/i18n 键。此时 host 桩还在,回滚 = revert 单个 UI commit,服务端零风险。
2. **Step B「host 桩降级」**(纯 server commit):删 omp-host 路由,未注册路由自然 404。过渡期语义:**Step A 落地前保持现状的 501/空答(诚实)**,不新增 410 之类中间态 —— 501 文案已说明原因(如 endpoints.js:327),期间任何漏网调用方得到的是明确拒绝而非静默空数据。Step A 与 Step B 同一发布内先后落地,不留长期"UI 已删但端点还在"的窗口(P3 收尾提交统一扫描)。
3. **Step C「契约守卫」**:wire gen **保持 vendored 原样**(D1:手改不可维护;裁剪收益仅是类型噪声,成本是下次同步冲突)。配套加 CI 守卫(见 5.15),禁止被删命名空间重新进入消费代码。

**备选:prune vendored gen**(在 gen 内删 `session.next.*`/`tui.*`/`permission.v2.*` 等)——
取舍:优点是 IDE 补全干净、`Event` union 收窄使 reducer exhaustiveness 检查更强;缺点是(1)与上游 diff 永久化,每次 vendored 刷新都要重放裁剪;(2)gen 是三份重复 union(types.gen.d.ts 内 Event 定义出现 3 次,如 tui 事件在 1159-1187、2152-2176、2347-2373),手改遗漏即类型错乱。**裁决:leave + 守卫**,并回答总纲开放问题 2。若上游 OpenCode 未来自己删除这些类型,随 vendored 刷新自然消失。

**i18n 通用规则**:所有键集中删除于 Step A(键是 `I18nKey` 类型派生,删键即编译检查);11 个 locale 文件(en/de/es/fr/ja/ko/pl/pt-BR/uk/zh-CN/zh-TW)同 commit 清理,以 en.ts 为基准逐文件对照。

### 5.1 GAP-G01 会话分享

| 位置 | 动作 |
|---|---|
| `packages/ui/src/sync/session-actions.ts:1207-1229` | 删 `shareSession`/`unshareSession` |
| `packages/ui/src/sync/session-ui-store.ts` | 删暴露的 `shareSession`/`unshareSession` action(Header.tsx:1100-1101 消费点) |
| `packages/ui/src/components/layout/Header.tsx:1155-1186` 及菜单项区(~2046-2052) | 删 shareCurrentSession/copyCurrentSessionShareUrl/unshareCurrentSession 与对应 DropdownMenu Item |
| `packages/ui/src/components/session/SessionSidebar.tsx:471-472,845-925` | 删 share 相关 props/回调传递 |
| `packages/ui/src/components/session/sidebar/SessionNodeItem.tsx:86,282,938-953,1651` | 删菜单分支(share/copyLink/unshare)与 memo 比较项 |
| `packages/ui/src/components/session/sidebar/hooks/useSessionActions.ts:37-38,137-173` | 删 handleShareSession/handleCopyShareUrl/handleUnshareSession |
| `packages/ui/src/hooks/useSessionAutoCleanup.ts:48` | 删 `if (session.share) return false` 豁免 |
| `packages/ui/src/stores/useGlobalSessionsStore.ts:122-126,152-153,167-168` | 删 share 合并/dedupe 键;test `useGlobalSessionsStore.test.ts:16-54,146-147` 同步删 |
| `packages/ui/src/components/layout/Header.tsx:476-477,519-520` | 删 `SessionLike.shareUrl` 与 `session.share?.url` 读取 |
| `packages/web/server/lib/omp-host/endpoints.js:326-331` | Step B 删两条 501 路由 |
| i18n(11 locales,基准 en.ts:509-513,586-591) | 删 `sessions.sidebar.session.menu.share|copied|copyLink|unshare`、`sessions.sidebar.session.share.*`、`sessions.sidebar.session.unshare.*` |

- **gen 处置**:`Session.share`(types.gen.d.ts:94-96,1820-1822)与 tui command 枚举中的 `'session.share'`(:1167)leave;守卫禁止新消费。
- **用户可见变化**:会话菜单不再有 Share/Copy link/Unshare;无公开链接能力(omp 无对应,总纲 D3)。
- **风险与备注**:低。数据兼容:omp-host 的会话存储本就无 share 字段,`useSessionAutoCleanup` 豁免删除后不存在"该留的会被清"的存量(omp 会话从未 share 过)。VS Code 运行时共享同一 UI 包,无需分支处理。

### 5.2 GAP-G02 OpenCode 升级面

| 位置 | 动作 |
|---|---|
| `packages/web/server/lib/omp-host/endpoints.js:168` | Step B 删 `/global/upgrade` 501 |
| `packages/web/server/lib/opencode/routes.js:162-174` 及 `routes-upgrade.test.js` | 删 `/api/opencode/upgrade`、`/api/opencode/upgrade-status` 与 `getOpenCodeUpgradeCapability`/`readOpenCodeCurrentVersion` 依赖(确认无其它调用后) |
| `packages/ui/src/components/update/OpenCodeUpdateToast.tsx`(全文件)与 `openCodeUpdateDedup.ts` | 删 |
| `useUIStore.showOpenCodeUpdateNotifications`、desktop 设置 `openCodeUpdateToastDismissedVersion`(OpenCodeUpdateToast.tsx:124) | 删状态与持久化键(persist version bump 清存量) |
| i18n `opencodeUpdate.toast.*`(11 locales,基准 en.ts:2889-2901) | 删 |
| `sdk.gen.js:626-629` `global.upgrade` | leave(零调用方,守卫) |

- **用户可见变化**:"OpenCode 更新可用" toast 与升级进度 toast 消失;引擎升级随应用发布(501 文案语义)。桌面应用自身的 updater 不受影响(desktop-shell 管辖,独立通知链)。
- **风险**:低,但需先确认开放问题 4(是否还有外部托管 OpenCode 运行时);若多运行时模式仍可切换到外部 OpenCode,则 `/api/opencode/upgrade` 延后到该模式退役。
- **gen 备注**:`Config.autoupdate/share/autoshare`(types.gen.d.ts:1557-1562)属 OpenCode 配置面死键,leave + 守卫;设置 schema 侧的清理归第 06 章。

### 5.3 GAP-G03 permission 协议链(V1+V2)

**解锁条件(D6-R12 三段式)**:P0 原子审批桥(C3+C4+C5,03 章)落地并默认启用 → P1 消费者切换(通知、待决读出、tray/VS Code 改读 03 章桥面)→ **观察期**后 P3 删除。桥落地前现状的"权威空答"是诚实过渡(endpoints.js:334-337 注释即此意),**保留至 Step A**。

Step A(UI + OpenChamber server):

| 位置 | 动作 |
|---|---|
| `packages/ui/src/sync/event-reducer.ts:521-547` | 删 `permission.asked/replied` case;`State.permission` 字段、`cloneField("permission")`(sync-context.tsx:1699-1702)、事件门控(sync-context.tsx:737-743)同删 |
| `packages/ui/src/stores/permissionStore.ts` + `stores/utils/permissionAutoAccept.ts` + 两个测试文件 | 删;zustand persist 的 localStorage 键在 persist version bump 中显式 remove |
| `packages/web/server/lib/permission-auto-accept/runtime.js`(routes 249-263、广播 65-68、reconcile 229-231)+ `feature-routes-runtime.js:11` + `core-routes.js:1067` 白名单项 | 删路由与运行时 |
| 通知系统 permission 抑制(notifications/DOCUMENTATION.md:48 所述触发路由) | 删抑制分支;completion/error 触发保留 |
| `packages/ui/src/sync/permission-toast.ts`(+test)、sync-context.tsx:1306-1310,1527-1569、`vscode-permission-auto-accept.ts` | 删 |
| `packages/ui/src/components/chat/PermissionCard.tsx`、ChatContainer.tsx:373-378,637-639 | 删卡片与 `useScopedBlockingPermissions` |
| `composer/ui/PermissionAutoAcceptButton.tsx`、ChatInput.tsx:74-75,386,2436-2440 | 删 |
| `packages/ui/src/sync/session-actions.ts:1548+`(respondToPermission)、1598-1617(发送时自动拒绝) | 删 |
| `session-ui-store.ts:762-766`、NewSessionDraftState.permissionAutoAcceptEnabled、ScheduledTaskEditorDialog.tsx:569 | 删 |
| `packages/ui/src/hooks/useTraySync.ts:12,29,63-64,92-95,113-117,271-277,595-600` | 删 permission 审批桥(question 部分随 G04) |
| `WorkStatusSubagentsSection.tsx:19-38,86-101` | 删 `permissions`/`questions` 读出与 needs-permission/asked 徽标;面板与 busy 状态保留 |
| `packages/ui/src/sync/eviction.ts:15-24` | `hasPendingBlockingRequests` 改为恒 false 或整体删除(与 G04 同一提交;见下方不变量说明) |
| `apps/runtimeEndpointReset.ts:9,63` | 删 reset 钩子 |
| `settings/sections/agents/AgentPermissionsEditor.tsx`、ModelControls.tsx:1438-1447,2449-2458 | 删 agent 权限规则编辑与摘要图标(归 03/02 章的 omp approval 语义取代) |
| `@/types/permission` | 删 |
| i18n:`chat.permissionCard.*`、auto-accept、`chat.workStatus.subagent.needsPermission` 等 | 删 |
| 测试:event-reducer.test.ts:276-310、issue-2039、issue-2903、session-switch-resync 的 permissionStore mock | 删/改写 |

Step B(host):`endpoints.js:338-341` 四条路由删除。

- **gen 处置**:V1 事件(:1134,4849,5892)、**V2 `permission.v2.*`(:4598-4639,本就零消费者)**、`Session.permission`(:114)、`PermissionRuleConfig`(:1347-1352)全部 leave + 守卫。
- **用户可见变化**:后台会话不再出现"需要权限"toast;tray 菜单不再有审批项;agent 编辑器无 allow/ask/deny 规则;审批改走 03 章桥的弹窗/tier 语义。
- **风险(高)与不变量**:(1)eviction 的"有未决阻塞请求的目录不得驱逐"不变量(eviction.ts:3-14 注释)随协议一起消失 —— 删除后 `pickDirectoriesToEvict` 的过滤退化为纯 pin/TTL,这是**语义正确**的(omp 无外置阻塞协议),但要在 sync-state-invariants 的文档 `sync/DOCUMENTATION.md:148-149,368-371` 同步删表项;(2)VS Code 与 web 双运行时的 auto-accept 策略 broadcasting 全部随运行时删除,注意扩展侧 `snapshot.sessions` 恢复路径(sync-context.tsx:1427-1431);(3)`sending-while-permission-open 自动拒答逻辑(session-actions.ts:1598-1617)删除后,发送行为只由 busy/idle 决定 —— 与 omp 语义一致(无外置许可队列)。

### 5.4 GAP-G04 question 协议链

**解锁条件:第 03 章 ask 对话框桥落地,且 permission/question 消费者切换(P1)完成并过观察期(与 G03 同批删除,D6-R12)。**

| 位置 | 动作 |
|---|---|
| `endpoints.js:342-344` | Step B 删三条空答路由 |
| `event-reducer.ts:549-576`、`State.question`、cloneField(sync-context.tsx:1703-1706) | 删 |
| `QuestionCard.tsx`(ChatContainer.tsx:373-375 挂载)、`question-recovery.ts`、sync-context 恢复路径(:1179-1183 注释所述 candidate 逻辑)与 toast(:1573+) | 删 |
| `session-actions.ts:1725-1744,1781-1785` | 删发送时自动拒答 |
| `@/types/question`、useTraySync question 审批、i18n question 键 | 删 |

- **gen**:question 事件(:1237,5055,5973)、`PermissionRuleConfig.question`(:1349)leave + 守卫。
- **用户可见变化**:无(现状本就永不触发);03 章桥提供真实 ask 交互。
- **风险**:中 —— `question-recovery` 与 reconnect 恢复逻辑交织,删除时须保留其余 reconnect 恢复路径不受影响;`ToolPart` 的 question **工具**渲染器(ToolPart.tsx:1409-1447)是 agent 工具调用面,不在本组,勿误删。

### 5.5 GAP-G05 OpenCode shell 会话通道

**解锁条件:第 02 章 `!` 本地执行面落地**(建议形状:`POST /api/omp/sessions/{id}/bash`(R3 复数路径;路由注册于 omp-host,R4),body `{command, excludeFromContext?}`,SSE 流式 chunk + 终态 `{exitCode, cancelled, output}`,直接包 `AgentSession.executeBash`(SDK agent-session.ts:7446-7452);`cd` 持久化对齐 command-controller.ts:1200-1216)。

| 位置 | 动作 |
|---|---|
| `endpoints.js:294-296` | Step B 删 501 |
| `client.ts:1011-1031` shellSession | 删 |
| `session-ui-store.ts:124-148` | `routeMessage` 删 `inputMode==='shell'` 分支与参数 |
| composer `inputMode` 状态与 `!` 切换 UI、`triggers.ts` 的 shell-mode 禁用逻辑 | 按第 02 章新交互重写(删除旧分支) |
| `MessageBody` 的 `UserShellActionPart`/shellAction 渲染分支 | 删(omp-host 存储中不存在该 part;OpenCode 时代的会话本就不可被 omp SessionManager 读取,无存量渲染需求) |
| i18n shell 输入模式键 | 删 |

- **用户可见变化**:过渡期(02 章端点未落地时)若必须先删,`!` 前缀输入暂时按普通 prompt 处理 —— **不推荐**;标准顺序是 02 章先行,本章 P3 删旧通道,用户无感切换。
- **风险**:中。注意 `buildOutgoingMessage`/tokenizer 测试对 inputMode 的引用需同步。

### 5.6 GAP-G06 tui-bridge 事件契约

现状:`tui.prompt.append/tui.command.execute/tui.toast.show/tui.session.select` 仅存在于 gen 三份 union(types.gen.d.ts:1159-1187,2152-2176,2347-2373,4893-4945),全库零消费(grep 验证)。

- **动作**:无代码可删;在契约守卫清单(5.15)登记这四个类型 + `permission.v2.*` 为禁用命名空间。`tui.command.execute` 的 command 枚举本身包含 `'session.share'`(:1167)—— 残留中的残留,仅作文档注记。
- **用户可见变化**:无。风险:极低。

### 5.7 GAP-G07 session.next.* durable stream

现状:约 30 个 `session.next.*` 事件类型(types.gen.d.ts:639-978)+ `SessionHistory`/`SessionDurableEventStream`(:2253-2257),零生产者零消费者。

- **备选方案与取舍**:
  - (a)**采用为投影目标**:把 engine/projection 的事件改造成 session.next 事件溯源流。否决:违反 D1(wire 不扩张;omp 原生概念走 RuntimeAPIs),且 SessionDurableEventStream 要求 durable 序列化基础设施,omp-host 无此存储层,引入成本远超收益。
  - (b)**prune gen**:见 5.0 总裁决,否决。
  - (c)**leave + 守卫 + 文档裁决**:选定。在 `packages/ui/src/lib/opencode/wire/` 的说明(或 5.15 守卫文件)中记录"session.next.* 不采用,omp 事件经 RuntimeAPIs/既有 wire 面投影"。
- **用户可见变化**:无。风险:极低。

### 5.8 GAP-G08 未生产 wire 事件分诊

逐事件处置(生产者基线见 2.1;**每条删除都是 D2 合规的显式处置**):

| 事件 | 消费链现状 | 处置 |
|---|---|---|
| `session.diff`(types.gen.d.ts:990) | reducer 309-313 → `state.session_diff`;`/diff` 恒 `[]`(endpoints.js:332);文档自认非聚合(work-status DOCUMENTATION.md:153-156;WorkStatusPrimaryGroup.tsx:163-166 注释:会话级 totals 恒 0,**消息级 `summary.diffs` 才是活数据**) | **删**:reducer case、`session_diff` state、`/diff` 路由、cloneField(sync-context.tsx:1674-1676)、`stripSessionDiffSnapshots` 中 session 级分支。保留消息级 summary.diffs 消费(TurnChangedFilesDropdown) |
| `message.removed`(:617) | reducer 386-393、routing-index 清理(sync-context.tsx:1151-1154)、cloneField(:1688-1695) | **删(终裁 R2,HOLD 已解除)**。omp SDK 无删除事件;host 的 revert 是整读重放;05 章 retry 回收明确不产消息壳删除(整消息回收方案已否决,05 §5.3.2 方案 A;测试锚点升级为 05 §7.1-2:"全部 part 移除后消息壳仍存在" —— gate b 不变式)。随 G08 P3 删除 |
| `message.part.removed`(:632) | reducer 457-473(按 partID splice;:465-466 空 part 集即删消息壳)、routing-index 清理(:1151-1154)、cloneField | **REVISED(R5):不删、不入守卫**。总纲 D1 例外/D6-R5 裁决:复用为 05 章 auto_retry 超越撤回的 wire 载体(**R2 改相:P2 门控首产**,05 §5.3.4 门 a/b/c;P1a 只点亮 `session.status{retry}`,overlay 走 `omp.retry.started{supersededMessageID}`,零 wire 变更)。整条消费链保留为活契约,归 05 章事件注册表管辖;5.15 守卫禁入,守卫脚本须对其放行。注:reducer :465-466 的"空则删壳"分支与 05 章 gate b(壳保留不变式)冲突,由 05 章 P2 落地时改造,不属本章删除面 |
| `vcs.branch.updated`(reducer 514-519) | 事件从未发出;分支状态走 `/vcs` 轮询(endpoints.js:468-472) | **删 case**;轮询保留为唯一 vcs 数据源(04 章 R2 未建统一事件面,无并入对象;若专项轮次未来重建事件面,以 04 章裁决为准) |
| `lsp.updated`(reducer 577-581)+ `/lsp`(:395)+ `/formatter`(:396) | `onLoadLsp` 回调触发重取,恒空;edit 工具渲染器的诊断条渲染空数据 | **删** case、端点、回调注册;toolRenderers 的诊断条组件在数据恒空下本就不渲染,删除其数据源即可(组件本体按 08 章原创面共存规则保留;omp 侧诊断的流内呈现归 05 章 customType 层 —— `lsp-late-diagnostic` 已在其 45+ 类型清单,05 §5.8 —— 本组不重建事件源) |
| `project.updated`(reducer 195-197) | 无生产者;`/project/current` 轮询存活(endpoints.js:459-467) | **删 case** |
| `integration.*` / `catalog.*`(:571-583) | 零消费 | 同 G06:守卫登记。注:`https://models.dev` **API** 用法(useConfigStore.ts:26-27 元数据、useProviderLogo.ts:63,125 logo)是 OC 活功能,不属于本组,保留 |
| `session.compacted`(:1264) | 无 case、无生产者 | **删(终裁 R2,HOLD 已解除)**:05 章 compaction 分隔线走 wire 合成消息投影(`projectCustomMessage` 通道,05 §5.5),进度与死端警告走 omp 轨 `omp.compaction.started/ended`(05 §5.0.3)—— `session.compacted` 不是载体,随 G08 P3 删除并入 5.15 守卫 |
| `session.error`(:997) | 消费链是**活代码**:event-pipeline.ts:491-496 将其列为流终态之一(:493);session-event-router.ts:67-69 据此 flush;sync-context.tsx:1603-1623 生成 error 通知(:1619-1621);event-reducer.ts:344-352 复位 idle | **删(终裁 R2:05 §5.11 裁决 omp-host 不生产该事件;评审 M7 三处冲突收敛为唯一结论)**。错误沉降面(05 章 v3):消息 error 态(wire 消息面)+ `omp.notice.raised{level:'error'}` 即时 toast(05 §5.1 行 10,去重键 (level,source,message))+ `omp.retry.ended{success:false}` 终端失败横幅(05 §5.3.2);终态/flush 权威 = terminal `agent_end` → wire `session.idle`;完成/失败系统通知唯一权威 = terminal `agent_end`(与 08 章二轮 M9 对齐)。删除的**同步替换面(四处,同批落地)**:① event-pipeline.ts:491-496 终态列表移除 `session.error`,终态收敛为 idle/created/deleted —— SSE 错误路径收尾 = 既有 `session.idle`,无专属终态事件;② session-event-router.ts:67-69 的 error flush 分支删除(flush 由 `session.idle` 同构承担);③ sync-context.tsx:1603-1623 的 error 通知分支(:1619-1621)删除,错误通知改挂 terminal settle(idle 边沿 + 消息 error 态)与 notice,去重对齐 08 章;④ event-reducer.ts:344-352 case 删除,状态复位由 `session.idle` case(:334-342)承担。**运行时前置:05 章 P1 事件面(`omp.notice.raised`、terminal agent_end 语义)先投产**,否则错误可见性断供;前置未满足前仅冻结不删 |

- **用户可见变化**:除 `session.error` 链(错误提示改由 notice toast + terminal 通知承担,形态变化但能力不降)外均无(其余消费的是永不发生的事件)。
- **风险**:中,集中在 `session.error`(终态/flush/通知三处活代码,须按上表四处替换面同批切改,单处遗漏即错误静默)与 `message.removed`(reducer 与 `message.part.removed` 的移除路径相邻),删除须带 event-pipeline 终态单测回归,且不得误伤 `message.part.removed` 消费链(05 章 P2 门控首产的活载体,R5)。

### 5.9 GAP-G09 retry status 形状 —— 改用途而非删除(REVISED:裁决已落定)

现状:`{type:'retry',attempt,message,next}` 机器(event-pipeline.ts:107-119 归一化、event-reducer.ts:101-106 等值、global-session-status.ts:16-44、client.ts:1073-1109)只为 OpenCode 的 retry status 事件而建,omp-host 从不生产;引擎侧 `auto_retry_start/end` 在 engine.js:611-612 被丢弃,UI 重试不可见(05 章 B2)。

- **方案 A:repurpose**。05 章把 `auto_retry_start/end` 映射为 `session.status {type:'retry',attempt,message,next}` / `session.idle`,现有归一化、等值比较、排序 rank、快照测试全部直接复用。依据 D1:retry 状态与 OpenCode 语义重合("与 OpenCode 语义重合的部分"走 wire),且 TUI 的 retry 展示(#retryPending、loader、撤回卡片,event-controller.ts:1942-2000)语义同源。
- **方案 B:删 + RuntimeAPIs 重建**。一致性更强(所有 omp 原生可见性都走自有面),但抛弃已测试的归一化/排序/快照链,重复造轮子。
- **裁决(已定)**:A。原与总纲第 2 节"retry status 形状(应删)"的冲突已关闭:总纲 §7.6 裁决"复用现有 wire SessionStatus retry 形状承载 auto_retry",§2 残留清单改列例外,D6-R12 再次确认。开放问题 1 标记已解决。
- **动作(本章范围内)**:无删除;守卫清单不登记(它是活契约)。接线在 05 章 P1a(`session.status{retry}` 首产;R2 拆分后 `message.part.removed` 首产移至 05 章 P2 门控,§5.3.4,不再与本接线同批),接线后补充 `bun:test`:engine 发 retry status → reducer 状态翻转。

### 5.10 GAP-G10 MCP 空面(REVISED R2:恒只读为长期稳态,终局归专项轮次)

现状:`/mcp` 恒 `{servers:{}}`、connect/disconnect no-op(endpoints.js:424-430,仅 auth 三条是诚实 501);UI 跑完整连接/诊断/OAuth 流(useMcpStore.ts:34-83 经 scoped api client;McpPage、McpSidebar、McpDropdown、McpOAuthCallbackPage、startMcpAuthorization、WorkStatusMcpSection)。引擎侧事实:SDK 的 OpenCode 兼容 discovery provider 会从 `opencode.json(c)` 的 `mcp` 键加载 server(`discovery/opencode.ts:9-13,240-243`),原生面另有 `.omp/mcp.json`(`discovery/builtin.ts:236-239`)—— **引擎内可以存在真实 MCP**,缺的是状态暴露与管理 API。

- **Step 1(P1,只读化 + 操作禁用;D6-R12)**:不做"看似成功"的 UI 操作 —— connect/disconnect、OAuth 三件套等一切针对 no-op/501 端点的调用路径禁用(disabled + 说明文案);McpPage/WorkStatusMcpSection 在无真实数据源时渲染明确空态("MCP server 经引擎配置加载,状态暴露与管理面归 MCP 专项轮次")。入口与只读态的门控经 `GET /api/omp/capabilities` 的 feature 状态承载(R2:服务端裁决,覆盖 新UI+旧engine/旧UI+新engine/relay 旧 bundle 三矩阵),**不使用本地 feature flag**。
- **Step 2 终局(REVISED R2)**:04 章二轮裁决本轮不建设 MCP 可执行端点,定义权移出至 **MCP 专项轮次**(04 GAP-17/§5.7.5)→ 只读 + 禁用是**长期稳态**而非"落地前临时态",useMcpStore 数据源维持现状空面;「管理启用」或「全删 UI(host :424-430 随 Step B)+ 守卫登记」均归专项轮次裁决,本轮不执行、不等待、无依赖边。
- **用户可见变化**:P1 起 MCP 面恒只读、所有操作控件禁用并说明原因(长期稳态);入口保留、渲染明确空态 —— 不隐藏入口,避免掩盖引擎内真实加载的 server(3.1 证据;开放问题 3 已定案)。
- **风险**:中;McpSidebar/Dropdown 与 ChatInput 的 `/mcp` 提及补全若存在挂点需同批禁用。i18n `mcp.*` 键保留(只读面仍在用;若专项轮次裁决全删则随彼时一次清理)。

### 5.11 GAP-G11 build/plan 二分残迹(执行者:01 章)

**主体设计(roles 取代 build/plan、`/agent` 面重定义)归第 01/02 章;本章负责残迹清单与顺序。** 解锁条件:01 章 roles 链 + 02 章 custom agents 面落地。

| 位置 | 动作 |
|---|---|
| `endpoints.js:361-376` | 按 01/02 章新 `/agent` 语义重写(删 builtin build/plan 制造与 `mode:'subagent'` 强制) |
| `engine.js:451-453`(custom agent 过滤豁免 build/plan)、`:464`(planYolo 映射)、`:639`(`'build'` 默认) | 随 01 章:plan 语义并入会话模式(omp plan mode,TUI interactive-mode.ts:536 + keybindings.ts:222-224 alt+shift+p),agent 默认改读 roles/自定义 agent |
| `ModelControls.tsx:503-526,971,1291-1295` | 删 `'build'` 硬回退与 'Build' 标签回退 |
| `mobileControlsUtils.ts:34-37`、`agentColors.ts:20` | 删 build 特判 |
| `useConfigStore.ts:282-293` 默认 agent 级联中的 build 档 | 按 01 章级联重写 |
| GitHubIssuePickerDialog.tsx:249、NewWorktreeDialog.tsx:439 | 删 build/plan 预选 |
| composer/tokenizer 测试中的 build/plan  fixture(composerLanguage.test.ts:10、tokenize.test.ts:12、mentions.test.ts:114) | 换成自定义 agent 名 fixture |
| Header.tsx:1338-1355 计划 tab 门控 + `planModeExperimentalEnabled` 实验面 | 归 02 章 plan mode 合并设计。**REVISED(D6-R12 拆分)**:合成文本协议的生产与消费(usePlanDetection.ts 的 marker 扫描、计划 tab 对合成协议的依赖)在 02 章模式端点上线的同一发布**停产停用** —— 不等 P3;本章 P3 只做遗留清扫(实验 flag、检测文件、i18n 文案) |
- **gen**:`tui.command.execute` 枚举含 `'agent.cycle'`(:1167)—— 注记。**用户可见变化**:agent 选择器不再默认 'build';'plan' 从 agent 列表消失,由会话级 plan 模式取代。**风险**:中(默认解析级联与乐观消息渲染的 agent 徽章依赖 ChatMessage.tsx:281-363 的回退链,须与 01 章同步重测)。

### 5.12 GAP-G12 `/command` 空端点

| 位置 | 动作 |
|---|---|
| `endpoints.js:393` | Step B 删(恒 `[]`) |
| `session-ui-store.ts:156-163` | 删 `dirState.command` 同步源;slash 路由保留 skills store 与 OC commands store 两源(magic prompts 是 OC 原创,归 08 章) |
| commands 同步 store 的 `/command` hydration | 删 |
| `CommandsPage`/`useCommandsStore` | **保留**(OC 原创自定义命令管理,数据走 OpenChamber host 路由,非 wire `/command`) |
- **终态一致性(D6-R12/R3)**:wire `/command` 在本章删除前**保持空返**(诚实桩,endpoints.js:393),禁止扩张其语义;新命令语义(omp 内建/引擎展开命令的发现与元数据)唯一落点为 `GET /api/omp/commands`(08 章设计,集合复数路径),经 RuntimeAPIs 访问。本章删除 wire 端点后,slash 管线两源(skills + OC commands)不变,`/api/omp/commands` 消费者不受影响。
- **用户可见变化**:无(skills/自定义命令路径不变)。**风险**:低;确认 slash 触发补全(triggers.ts)不依赖同步命令列表的空数组以外的行为。相邻:`PATCH /config` 只存 custom agents(endpoints.js:348-356)是 02 章范围,本章不动。

### 5.13 GAP-G13 BehaviorPage OpenCode 路径(REVISED:证据重验后维持方向,细节修正)

- **改指向**:`AGENTS_MD_PATH`(BehaviorPage.tsx:33)与 `/api/behavior/agents-md` 服务端读写目标改为 omp 原生用户级 `getAgentDir()/AGENTS.md` —— **路径必须由服务端解析**(`getAgentDir()` 是 profile-scoped:默认 `~/.omp/agent`,激活 profile 后 `~/.omp/profiles/<name>/agent`;`packages/utils/src/dirs.ts:452-498`,加载点 `discovery/builtin.ts:910`),UI 不得硬编码。原生文件 priority 100,存在即遮蔽兼容文件(3.3);**一次性迁移**:首次打开时若旧路径 `~/.config/opencode/AGENTS.md` 存在且有内容且新路径不存在,提供服务端导入(copy)并提示。OpenCode 兼容 provider 仍读旧路径(`discovery/opencode.ts:163-182`),不迁移引擎也能用,但产品面对齐 omp 原生路径(总纲裁决原则)。
- 警告文案(:328-330)随路径更新;`recordDeferredOpenCodeRestart`/`noteDeferredRestartFromPayload`(:23)与 `usePendingOpenCodeRestartStore` 若 G02/G12 删除后无剩余消费者则一并删除(共享机器,DAG 中排在二者之后)。
- **用户可见变化**:行为设置页的全局指令文件位置变为 omp 原生;存量用户获导入提示。**风险**:低;目标路径终裁若第 06 章另定(如 rules 文件体系),以 06 章为准并回写本节 —— 已在开放问题 6 标注。**阶段**:维持 P3(06 章设置代理定稿后执行);3.3 的发现顺序重验不改变阶段,只修正证据与 profile 解析要求。

**2026-08-24 增量(已落地,commit eacb00c)**:

- 改指向落地:`/api/behavior/agents-md` 读写目标为服务端解析的 `getAgentDir()/AGENTS.md`(经 omp-host `GET /agent-dir`),且**每请求重解析**——原服务端对 agentDir 的进程级永久缓存已删除:profile 切换后写入正确目录、omp-host 瞬时不可达不再钉死静态回退(回归:`routes-behavior.test.js` 5 测,含 profile 切换与回退不缓存两例)。
- 取值语义对齐引擎:BehaviorPage 编辑器**文件存在即权威**(含空文件),`globalBehaviorPrompt` 副本降级为"文件从未创建时"的种子(迁移/新装机路径),外部修改(TUI/编辑器/profile 切换)不再被陈旧副本遮蔽或覆盖(契约:`ui/lib/behaviorPrompt.ts` `resolveInitialPrompt` + 5 测)。
- `optimizeSystemPrompt` 死开关**整链删除**:自引擎切换到 omp 起 `getManagedOpenCodeEnv` 恒返 `{}`(index.js),插件注入零生产调用点,开关为静默 no-op。已删:BehaviorPage section、DesktopSettings 键、settings 搜索注册项、i18n 5 键 ×11 locale、`sanitizeSettingsUpdate` 分支(旧键自更新中丢弃)、`server/lib/system-prompt/` 模块整目录。
- **G13 尾款(未做,仍列 P3)**:①重启机器——`behavior` scope 现仅剩 agents-md 保存路径一个消费者(`noteDeferredRestartFromPayload`,BehaviorPage `handleSave`),删除仍依赖 G02/G12 共享机器协调;"Restart OpenCode to apply" 文案对 omp 引擎语义失真(新会话即生效,无进程重启概念);②老安装 `<openchamber-data-dir>/system-prompt/` 物化目录不再生成,存量为惰性孤儿(无消费者),可与尾款同批回收;③老 settings.json 残留的 `optimizeSystemPrompt` 值惰性(无消费者,后续设置写入经 sanitize 即消失)。

### 5.14 删除顺序 DAG(对齐 D4/D6;REVISED:审批桥 P0 化 + 消费者切换插入)

```mermaid
graph TD
  subgraph P0["P0 概念迁移"]
    BRIDGE[ch03 审批桥 C3+C4+C5 原子交付(D6-R10)]
  end
  subgraph P1["P1 可见性桥(提前批,零耦合)"]
    G06[G06 tui-bridge 守卫]
    G07[G07 session.next 裁决+守卫]
    G09[G09 retry 复用(裁决已落定,总纲 §7.6)]
    G01e[G01 share 删(可提前)]
    G02e[G02 upgrade 删(可提前,须开放问题4确认)]
    G10r[G10 MCP 只读化+禁用(终态: 04 descope R2)]
    PCONS[permission/question 消费者切换(改读 03 章桥面)]
  end
  subgraph P2["P2 实体面解锁"]
    CH02[ch02 '!' executeBash 端点] --> G05
    CH02M[ch02 模式端点上线] --> PLANSTOP[plan 合成文本协议停产停用(同发布)]
    CH05P1[ch05 P1 事件面投产: omp.notice.raised + terminal agent_end 终态]
    CH0102[ch01 roles + ch02 agents] --> G11
  end
  subgraph P3["P3 大扫除(观察期后)"]
    G05[G05 shell 通道删] --> SWEEP
    G03[G03 permission 链删] --> SWEEP
    G04[G04 question 链删] --> SWEEP
    G08[G08 可删项: diff/vcs/lsp/project/message.removed/compacted] --> SWEEP
    G08ERR[G08 session.error 链删(四处替换面, 5.8)] --> SWEEP
    G11[G11 build/plan 残迹] --> SWEEP
    PLANSTOP --> PLANSWEEP[plan 遗留清扫: flag/文件/i18n] --> SWEEP
    G12[G12 wire /command 删] --> SWEEP
    G13[G13 BehaviorPage 改指向] --> SWEEP
    SWEEP[收尾提交: 端点批量 StepB + 守卫上线 + i18n 终扫]
  end
  BRIDGE --> PCONS
  PCONS --> G03 & G04
  CH05P1 --> G08ERR
```

硬依赖边(为什么不能更早):
1. **G03/G04 三段式(D6-R12)**:P0 原子审批桥(C3+C4+C5 单一能力整体落地,桥未整备不开 `hasUI`)→ P1 消费者切换(通知、待决读出、WorkStatus blocker、tray/VS Code 全部改读 03 章桥面)→ **观察期** → P3 协议删除。删除不得与消费者切换同发布,更不得先于桥落地 —— 否则审批/通知/WorkStatus 断供。
2. **G05 ← ch02**:否则 `!` 输入退化为普通 prompt,功能净损失。
3. **G08 `session.error` 链 ← ch05 P1 投产(R2 收敛后唯一残余依赖边)**:05 章 v3 §5.11 已终裁不生产,HOLD 全部解除;但删除前错误沉降面(`omp.notice.raised{error}`、terminal `agent_end` → `session.idle`、`omp.retry.ended` 横幅)必须已投产,否则错误可见性断供 —— 故 G08ERR 排在 05 章 P1 之后。compacted/message.removed 无活消费链语义,随 P3 批量删除。`message.part.removed` 已出清单(R5:05 章 P2 门控首产,守卫与 DAG 均不含)。
4. **G11 ← ch01+ch02**:默认 agent/model 级联重写后,build/plan 残迹才可安全移除;plan 合成文本协议的**停产停用点 = 02 章模式端点上线**(D6-R12,同发布),P3 仅清扫 flag/文件/i18n。
5. **G10 终局 ← ch04 R2 descope 裁决(已决,无等待边)**:本轮恒只读/禁用,只读面是长期稳态;管理端点建设与「是否全删 UI」归 MCP 专项轮次(04 §5.7.5),本轮 DAG 无 G10 等待节点。**G13 ← ch06**(目标文件终裁);**G12 ← ch06**(开放问题 6 的 `/config` 去向)+ 08 章 `/api/omp/commands` 上线。
6. **G02 ← 打包确认**(开放问题 4)。
7. 组内顺序:G02/G12 的共享重启机器(`usePendingOpenCodeRestartStore` 等)最后删;G03 与 G04 的 eviction/门控改动必须同一提交(二者共享 `hasPendingBlockingRequests` 与事件门控列表)。

**门控 vs 硬删裁决(REVISED)**:仅 G10 有门控,且经 `capabilities`(R2)承载而非本地 feature flag —— 04 章 R2 descope 后该门控为**长期稳态**而非过渡态(管理启用不在本轮发生);plan 实验面的切换点是"02 章模式端点上线"本身,不另设 flag;G03/G04/G05/G11 不设 flag —— 它们的"替代品"本身就是开关(03/02 章的桥与模式),删除发生在替代品默认启用且消费者切换完成之后的观察期末尾,flag 只会制造第三种中间态;G01/G02/G06/G07/G08-可删项/G12 直接硬删(零用户可见损失或纯守卫)。

### 5.15 契约守卫(Step C 的具体形态)

新增一个 CI 脚本(建议 `packages/ui/scripts/check-wire-usage.mjs`,或并入现有 lint 流水线),对 `packages/ui/src` 与 `packages/web/server/lib/omp-host`(排除 `wire/gen/`)断言**零引用**:

```
session.next.            (G07)
tui.command.execute | tui.toast.show | tui.prompt.append | tui.session.select   (G06)
permission.v2.           (G03)
session.share( | unshareSession | shareSession    (G01,UI 调用面)
global.upgrade           (G02)
session.diff | vcs.branch.updated | lsp.updated | project.updated | message.removed | session.compacted | session.error   (G08 可删项,删除后启用;error/compacted 终裁 R2,见 5.8 与 05 §5.11)
question.asked | permission.asked   (G03/G04 Step A 后启用)
```

命中即 fail,消息指向本文件对应 GAP。**禁入清单(R5)**:`message.part.removed` 不得加入守卫 —— 它是 05 章 P2 门控投产的活 wire 契约(retry 超越撤回),守卫脚本须对其合法消费放行(见 §7 单测)。该脚本同时充当第 05 章事件处置表的机器化旁证:被守卫的类型若有新映射需求,必须先改守卫清单(= 显式修订处置);删除项的事件回填只能引用 05 章唯一事件注册表(R1),守卫不豁免私建通道。**R2 补记(M8 关闭)**:03 章二轮已删除其"整段裁剪"备选并收敛至本裁决 —— vendored 处置全书归一(总纲/03/05/本章):gen 保持原样,守卫只约束消费侧;本守卫清单与 5.14 DAG 均不引用任何裁剪路径。

---

## 6. 迁移与兼容

**存量数据**:
- omp-host 会话存储(`SessionManager` 格式)从不含 share/shell/permission/question 字段 —— G01/G03/G04/G05 无服务端数据迁移。
- UI 持久化清理:`permissionStore` 的 zustand persist localStorage 键、desktop settings 的 `openCodeUpdateToastDismissedVersion`(G02)在各自 Step A 中通过 persist `version` bump + `migrate` 显式删除;`useGlobalSessionsStore` 的 share 合并逻辑删除不影响存量会话对象(字段自然缺失)。
- G13 迁移:旧 `~/.config/opencode/AGENTS.md` 保留原地(OpenCode 兼容 provider 仍读,`discovery/opencode.ts:163-182`),新编辑目标为服务端解析的 omp 原生路径;一次性导入提示(条件:旧文件存在且有内容、原生文件不存在);不自动删除旧文件 —— 导入后旧文件被原生文件遮蔽(user 级仅保留一份,`capability/context-file.ts:31-36`),成为惰性残留。

**并发会话/多运行时**:删除全部经正常发布流程;运行中的旧版 UI 对新版 host:已删路由得 404 —— 影响面为零(这些路由本就 501/空答)。反向(新 UI + 旧 host):UI 不再调用,无影响。VS Code 扩展与 web 共用 UI 包,同发布。

**阶段性开关**:见 5.14 —— 仅 G10 经 `capabilities` 门控(R2;descope 后为长期稳态);permission/question 以"P0 桥 → P1 消费者切换 → 观察期"为切换链(R12);plan 合成协议以 02 模式端点上线为停产点;其余以"替代品默认启用"为切换点。

**回滚策略(每阶段一个 revert 单元)**:
- 每个 GAP 的 Step A(UI)与 Step B(host)是独立 commit;回滚 = `git revert`,无数据反迁移需求(删除不销毁服务端数据)。
- P3 收尾提交(端点批量删除 + 守卫上线)单独成 commit,保证"一键恢复全部端点"的回滚粒度。
- G03 的 i18n/store 删除量大,revert 时 locale 文件可能冲突 —— 缓解:Step A 拆成「逻辑删除」与「i18n 键清理」两个相邻 commit,后者冲突成本最低。
- G09(repurpose)回滚 = 05 章映射的 revert,不涉及本章。

---

## 7. 验证方案(设计,不执行)

**omp-host(bun:test,`packages/web/server/lib/omp-host/`)**:
- 端点删除后:被删路由(share/shell/permission/question/command/diff/lsp/formatter)返回 **404 而非 500**;`/mcp` 空面桩(424-430)随 G10 只读面**保留**(终局归 MCP 专项轮次);`/event` SSE 流上不再出现守卫命名空间的事件类型(现状本就没有,回归断言防引入)。
- 删除前过渡期:501 文案与状态码快照测试锁定(Step B 删除时该测试随删)。
- 每个改动文件 `node --check` 通过(D5)。

**UI(bun:test)**:
- reducer:删除 case 后 `applyDirectoryEvent` 对旧事件类型返回 false 且不改 state(用守卫清单事件做穷举表驱动测试,`session.error` 随 G08ERR 删除后加入该表);`session.error` 终裁删除(5.8 四处替换面):event-pipeline 终态单测改为断言仅 idle/created/deleted 触发收尾、`session.error` 不再 flush/收尾;SSE 错误路径以 `session.idle` 收尾的回归;错误通知断言改挂 terminal settle(idle 边沿)+ `omp.notice.raised`(与 08 章通知改键同批验证,断言无错误静默窗口)。
- eviction:G03/G04 删除后,含空 `permission`/`question` 键的目录恢复为正常驱逐候选(eviction 单测改写)。
- G01:`useGlobalSessionsStore` 删除 share 合并后,upsert/merge 快照测试更新。
- 守卫脚本自身单测:对故意引用禁用命名空间的 fixture 必须报错;对 `message.part.removed` 合法消费的 fixture(05 章 retry 回收 reducer)必须放行(R5)。

**E2E(dev 栈 5180/3902,浏览器驱动)**:
- 会话菜单/Header/侧栏右键不再出现 Share/Unshare/Copy link;执行旧快捷路径(如有)无 console 错误。
- composer `!` 输入(G05 删除后、02 章新面启用)走新 RuntimeAPI,流式输出可见、`cd` 持久生效(对照 TUI command-controller.ts:1138-1191 行为)。
- MCP 面恒只读:全部操作控件禁用且带说明,无 no-op 成功交互;入口保留 + 空态文案(capability 存在即显示只读空态面,不隐藏;三矩阵降级行为不变,R2)。
- 发送消息、队列、abort 全流程在 permission/question state 删除后行为不变(重点:busy 时入队、idle 自动发送)。
- 设置 → Behavior:新路径显示服务端解析的 omp 原生路径(激活 profile 时指向 `~/.omp/profiles/<name>/agent/AGENTS.md`),旧文件存在时出现导入提示。
- 升级:不再出现 OpenCode 更新 toast(模拟 version 变化)。

**TUI 对照点(D5)**:
- share/upgrade/API-级 permission:无对照(负空间,3.1)。
- `!`:本地执行 + `!!` 排除上下文 + cd 持久化(input-controller.ts:797-813;command-controller.ts:1138-1191)。
- AGENTS.md:多 provider 竞争 + scope 去重遮蔽(`capability/context-file.ts:31-36`);用户级主文件 `getAgentDir()/AGENTS.md`(`discovery/builtin.ts:910`,profile-scoped);OpenCode 兼容用户级 only(`discovery/opencode.ts:163-182`)。完整顺序表见 3.3。

**静态检查**:`grep -rn "permission.asked" packages/ui/src packages/web/server/lib --exclude-dir=gen` 归零(其余守卫项同理,5.15 列表)。

---

## 8. 开放问题

1. ~~G09 与总纲冲突~~ **已解决(2026-08-19 修订轮)**:总纲 §7.6 + §2 已裁决"复用现有 wire SessionStatus retry 形状承载 auto_retry",D6-R12 再次确认;原建议(总纲改为复用表述)已被采纳,G09 维持"改用途不删除"(5.9)。
2. **wire gen prune-vs-leave(总纲开放问题 2 的本章答复)**:建议 leave + 5.15 守卫;裁剪的唯一收益(类型噪声)不值得永久化 vendored diff。**总纲 §7.2 已按此裁决**(vendored 生成物不裁剪,停止消费 + 契约守卫防回归)。**R2 补记(M8 关闭)**:03 章二轮已删除其"裁剪"备选并收敛至本裁决 —— vendored 处置全书归一,gen 零修改;若未来 exhaustiveness 检查需求强烈,可在**上游 OpenCode 删除后**借 vendored 刷新自然收窄。
3. **MCP 终局与入口策略(R2 收敛,已决)**:04 章二轮裁决本轮不建设 MCP 可执行端点、定义权移出至 **MCP 专项轮次**(GAP-17/§5.7.5),只读 + 禁用是**长期稳态**而非过渡态 → 本章 G10 终局(本轮)= 恒只读/禁用,UI 面保留(不全删)。入口策略随之定案:**显示明确空态的只读面,不隐藏入口** —— 引擎内可真实加载 MCP server(3.1/5.10 证据),长期隐藏会掩盖其存在;空态文案明示"经引擎配置加载,状态暴露与管理面归 MCP 专项轮次"。「管理启用 or 全删 UI」的唯一剩余裁决权 = MCP 专项轮次(经 04 §5.7.5 存根),本轮无待决事项。
4. **托管 OpenCode 运行时是否仍存在(G02 前置;开放,所有者:产品确认/desktop-shell 域)**:`/api/opencode/upgrade` 的 capability 已恒 409(routes.js:162-168),需产品确认桌面打包是否还内置/下载外部 OpenCode 二进制(多运行时切换模式)。若存在,G02 的 OpenChamber 路由部分延后至该模式退役;wire 501 与 toast UI 仍可删。
5. **`!` bash 端点归属(开放,所有者:02 章;02 章修订版尚未认领该面,兜底 08 章)**:建议第 02 章(输入模式域);若最终归 08 章(原创面适配),依赖边等价改挂,不影响本章清单 —— 这是 G05 解锁所依赖的剩余开放项。
6. **G13 目标路径与 `/config` PATCH 去向(开放,所有者:06 章)**:BehaviorPage 最终目标(`getAgentDir()/AGENTS.md` vs 06 章的 rules 文件体系)与 `PATCH /config` 只存 agents 的处置,均以第 06 章定稿为准;本章清单在 06 章落定后执行,冲突时以 06 章为准并回写本表。**注**:3.3 的发现顺序与遮蔽语义已重验(证据 REVISED),默认裁决指向 `getAgentDir()/AGENTS.md`(服务端解析,profile-scoped),除非 06 章另定。
7. ~~`session.error`/`message.removed` 的最终处置~~ **已解决(2026-08-20 修订二轮,R2/M7)**:05 章 v3 §5.11 终裁 `session.error` **不生产** —— 错误经消息 error 态 + `omp.notice.raised{level:'error'}` + `omp.retry.ended{success:false}` 横幅沉降,终态/flush 权威 = terminal `agent_end` → `session.idle`,完成/失败通知唯一权威 = terminal agent_end(对齐 08 章 M9);`message.removed` 05 §5.3.2 明文不用(整消息回收方案已否决);`session.compacted` 非 compaction 载体(05 §5.5:分隔线走合成消息投影、进度走 omp 轨)。G08 三项 HOLD 全部解除,终裁照删;替换面、删除前置与守卫登记见 5.8/5.15,DAG 边见 5.14(G08ERR ← 05 章 P1 投产)。

---

## 9. 依赖

**前置(阻塞本章删除)**:
- 第 01 章(roles 取代 build/plan)→ 解锁 G11。
- 第 02 章(custom agents 面、`!` executeBash RuntimeAPI、模式端点)→ 解锁 G05、共同解锁 G11;模式端点上线即停产 plan 合成文本协议(D6-R12),P3 仅清扫。
- 第 03 章(审批桥 C3+C4+C5 原子交付,P0;ask 对话框桥)→ 解锁 G03、G04 的三段式:P0 桥 → P1 消费者切换 → 观察期 → P3 删除(D6-R12)。
- 第 04 章(R2:本轮 descope MCP 可执行端点建设,§5.7.5)→ G10 终局(本轮)= 恒只读/禁用(长期稳态);「管理启用/全删 UI」归 MCP 专项轮次,本章本轮无等待边。
- 第 05 章(处置表 + 唯一事件注册表,R1;v3 已定稿)→ G08 三项 HOLD 已全部解除(R2:`session.error` 不生产 §5.11、compaction 走 omp 轨 §5.5、`message.removed` 不用 §5.3.2);`session.error` 消费链删除(G08ERR)的运行时前置 = 05 章 P1 事件面投产;G09 接线在 05 章 P1a;`message.part.removed` 由 05 章 P2 门控首产(§5.3.4),不在本章删除面(R5)。
- 第 08 章(`/api/omp/commands`)→ G12 删除的前置确认(wire `/command` 空返维持至删除,D6-R12)。

**后置**:无 —— 本章是各域迁移的终点清障;本章产出的契约守卫(5.15)反过来服务第 05 章的事件处置纪律(D2 的机器化旁证)。

**对 08 章的约束**:G10/G12/G13 中保留的 OC 原创面(WorkStatusSubagentsSection 本体、CommandsPage、BehaviorPage 框架)按第 08 章的共存规则适配;本章删除不得触及 08 章标记保留的面板骨架。
