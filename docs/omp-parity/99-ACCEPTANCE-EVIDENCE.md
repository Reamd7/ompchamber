# omp-parity 实施验收证据总录

状态:2026-08-20 实施夜验收完成(用户指定的 `/compact` 压缩样式验收已闭合);同日进度审查补全遗留总账(§5,初版清单漏记约半数到期欠账)
本文档是 `docs/omp-parity/00-MASTER.md` D1-D6/R1-R15 实施后的**全部验收证据索引**:每项能力 → 自动化测试 → live 证据 → 已知边界。验收过程中发现并修复的缺陷单列。

---

## 1. 测试套件(全绿)

| 套件 | 命令 | 结果 |
|---|---|---|
| omp-host 引擎+域模块 | `cd packages/web && bun test server/lib/omp-host/` | **218/218** |
| 共享 UI(隔离运行器) | `cd packages/ui && bun run test` | **275/275 文件** |
| VS Code 扩展 | `cd packages/vscode && bun run test` | **24/24 文件** |
| Electron | `cd packages/electron && bun run test` | **15/15 文件** |
| 事件契约守卫 | `node scripts/check-event-coverage.mjs`(`bun run check:events`) | OK:24 SDK 成员(亲验 .d.ts:10 core+15 session)/24 engine case/25 注册事件 |
| 类型检查 | `bun run type-check`(五包) | 0 错误 |

已知噪声:packages/web 的 vitest 有 34 个**既有**环境性失败(bun:test shim 缺 `setSystemTime`、Windows 路径期望),经逐一归因不在本批触及文件内;omp-host 目录已从 vitest 排除(bun 专用),web `test` 脚本串接双运行器。

## 2. 分域验收矩阵

### 2.1 事件脊柱(05 章,P1)
| 项 | 测试 | live 证据 |
|---|---|---|
| OmpEventBus(durable/volatile/环/重放/gap→resync) | omp-parity.test.js(RingEventBus/OmpEventBus 组) | `/api/omp/events` SSE 建连 `:ok` 帧 + `text/event-stream`(UI 源 fetch 验证) |
| 24 成员显式处置 | omp-host.dispositions.test.js 12 测(retry 零 wire 突变、isTerminal busy 保持、model_changed 双轨、notice、irc display:false 双保险、partial 永不终态、tail-sync 幂等、unknown 不静默) | — |
| `omp.usage.turn` per-turn 遥测 | 同上 message_end 断言 | **真实浏览器回合**:`GET /api/omp/sessions/{id}/telemetry` 返回 input 17446/output 4/ttft 4136ms;UI 用量行渲染 `⤵ 338 ⤴ 1.9K 💾 70K` |
| manifest×3 + CI 守卫五检 | omp-parity.test.js 正负例(SDK 增员→exit≠0) | `check:events` 绿 |
| 挂载冒烟(防 import 级崩溃) | omp-parity.test.js registerEndpoints 桩测(验收轮补,见 §4.1) | — |

### 2.2 模型角色与设置(01/06 章)
| 项 | 测试 | live 证据 |
|---|---|---|
| 每目录 keyed Settings 注入(R6) | domain-models.test.js 20 测 + engine.test.js 注入拓扑断言(`options.settings === settingsFor(dir)` 同目录同实例) | `/api/omp/models` 返回真实 10 角色(default=axonhub/glm-5.3:xhigh,smol,slow=gpt-5.6-sol,vision,plan,designer,commit,tiny,task,advisor) |
| `/api/omp/settings` GET/PUT(R9 脱敏/R6 project 只写 modelRoles) | domain-models.test.js(credential 只回 configured、越界写 400、revision 单调) | live GET 返回 schemaVersion 17.3.7 + agentDir |
| config.model 默认指针(GAP-03) | omp-parity.test.js defaultModelPointer 2 测(真实 Settings.loadIsolated+临时 config.yml;缺省→键省略) | live `/api/config` → `"model":"axonhub/glm-5.3"`(修复前为排序第一的 opencode-go/deepseek) |
| defaultModel 只读检测+导入(R12) | domain-models.test.js(不覆盖已有 role) | — |
| UI 角色选择器 | ompRoleModeSurfaces 11/11 + 能力门矩阵 | **live**:模型标签 `GLM-5.3` + 角色 chip `Default` 真实渲染(截图确认);菜单开合在无头环境受 container-query 布局限制(渲染移动变体),组件级已断言 POST /mode |

### 2.3 审批与对话框(03 章,P0)
| 项 | 测试 | live 证据 |
|---|---|---|
| UiLeaseTable(hasUI 唯一权威,R13) | domain-dialogs.test.js 43 测(TTL/过期/引用计数/幂等/翻转恰一次/SSE 不算 presence) | — |
| PendingDialogRegistry(原子 respond/竞答 409/错目录 403/双 TTL/orphan/R11 settle-all) | 同上(fake timers 全计时分支) | — |
| 引擎接线(hasUI=租约快照) | engine.test.js(无租约 false + **租约 acquire 后 materialize 变 true**) | — |
| always-allow 写先批事务(R10) | domain-dialogs.test.js | — |
| UI 消费面(弹窗渲染) | — | **未落地**(桥/端点/租约全就绪,UI 面属下一批;spec 分阶段) |

### 2.4 模式与 Agent(02 章)
| 项 | 测试 | live 证据 |
|---|---|---|
| planYolo 形状(P0 缺陷 a) | engine.test.js(有 model→`{target}`,无→省略,JSON 永不含 `autoApproveOnResolve`) | — |
| 模式状态机/plan 评审桥/冷恢复 | domain-modes.test.js 47 测(mode_change 持久化、preparePlanForReview 挂接) | `/api/omp/sessions/{id}/mode` GET/POST live 可达(capability on) |
| agent-definitions/personas CRUD | 同上(名称唯一/工具白名单/project 门 409) | live 空表 `{"agents":[]}`/`{"personas":[]}` |
| `/` 命令不生成标题(验收轮修复,§4.2) | 引擎行为(修复后压缩不再污染标题) | live:01a01d1b 压缩后标题为空(由后续 'ok' 消息正常生成) |

### 2.5 协议与实体(04 章)
| 项 | 测试 | live 证据 |
|---|---|---|
| local:// 桥(会话钉扎/零全局变异) | omp-host.domain-uri.test.js(真实 SDK router:containment/traversal/拒绝;无绝对 sourcePath 回显;token 兑换) | `/api/omp/uri/resolve` 等端点 live(capability on) |
| 会话树 | 同上(注册表数据→{leafId,nodes}) | live:真实树 `{"leafId":"01a01904…","nodes":[…]}` |
| agent-runs 聚合(250ms 合并/revision) | 同上 | live:`{"agentRuns":[],"revision":0}` |
| jobs R12 稳态 | 同上(501+ownerSessionID) | live:`{"error":"jobs-unavailable","reason":"sdk-single-manager","ownerSessionID":null}` |

### 2.6 流内元素渲染(05 章 P2)
| 项 | 测试 | live 证据 |
|---|---|---|
| T1-T4 分层 | customTypeTiers.test.ts(T1×10/T2×3/T3 精确+前缀/T4 兜底) | — |
| T2 压缩分隔线 | OmpCustomMessage.test.tsx(SSR 双态 18 测)+ turnCacheMiss/turnUsage 套件 | **✅ 用户指定路径专项(§3)** |
| per-turn 用量行(逐字段 TUI 移植) | turnUsage.test.ts(全阈值分支+降级) | **live**:真实回合渲染 `⤵ 338 ⤴ 1.9K 💾 70K` |
| cache-miss 四条件 | turnCacheMiss.test.ts(全部否定分支) | —(需真实 warm→cold 跃迁,状态级覆盖) |
| transcript 角色投影(bash/python/fileMention/hook) | omp-parity.test.js(user 侧独立段+确定性 id) | —(历史含这些角色的会话冷读可见) |
| retry 取代徽标 | reducer/store 测试(overlay 状态) | 渲染层无专测(需真实失败注入) |
| notice toast | omp-event-pipeline/reducer 测试 | —(需真实引擎通知) |

### 2.7 残留清扫(07 章)
| 项 | 测试 | live 证据 |
|---|---|---|
| session.error 零生产+四处替换 | event-pipeline 终态单测/通知 toast 路径测试 | — |
| wire 残留契约守卫(tui.*/session.next/message.removed) | wire-residue-guard.test.ts | 绿 |
| BehaviorPage→omp 原生 AGENTS.md(G13) | routes-behavior.test.js 3 测(agent-dir 解析链/不可达回退/写路径) | **live**:`/api/behavior/agents-md` 与 `/api/agent-dir`(生产 Node 进程不能载 SDK,由 bun 侧 omp-host 解析——验收轮修复 §4.3) |
| MCP 只读稳态(R15) | 控件清单取证 | capability `mcp.readOnly:true` |

### 2.8 UI 同步层(05 §5.2 + 08)
| 项 | 测试 | live 证据 |
|---|---|---|
| omp 管线/重连/resync 矩阵 | omp.test.ts 16(SSE 帧/Last-Event-ID/退避/降级矩阵)+ pipeline/resync 11(scope 恰好一次 GET、1→7 全序) | 冷加载无控制台错误;capabilities 探测正常降级路径有测 |
| 通知权威=terminal agent_end(M9) | idle 边沿通知测试 | — |
| chat 全链路 | — | **live×2**:真实点击输入→发送→回复"通过"/"ui-turn-ok"→telemetry 行 |

## 3. 压缩样式验收专项(用户指定路径)

会话:`01a01a18`(用户给定)与 `01a01d1b`(复验),操作:会话输入框 `/compact`。

| 证据 | 内容 |
|---|---|
| 压缩执行 | SDK compaction 条目 `tokensBefore: 23223`,entries API `kinds=compaction` 返回全文摘要 |
| 服务端投影 | 消息 API:user `hi` + assistant 回复 + `[omp:compactionSummary]` 分隔消息,`metadata={ompRole, tokensBefore}`,parentID 骑乘 user(§4.4 修复后) |
| UI 折叠态 | 元素 `msg_cmt0xmwqe6ed3`,按钮 `aria-expanded="false"`,文本 `compacted from 23K tokens` |
| UI 展开态 | 点击后 `aria-expanded="true"`,展开 markdown 摘要("No prior history. / Turn Context (split turn) / Original Request…") |
| **视觉判定** | 截图(视觉模型判读):纤细淡色横线+下箭头+弱化灰字标签,展开显示摘要——符合 TUI `SummaryDividerComponent` 语义(05 §3 规格) |
| 标题不污染 | 压缩后 title 保持空/正常(§4.2 修复;修复前被摘要全文污染) |

复现:`/api/session/{id}/message` 三条可见;截图:`%TEMP%\omp-sshots-155e99f0d2c8d8fd.webp`(展开态)。

## 4. 验收中发现并修复的缺陷

1. **宿主启动崩溃**(`ompFeatures is not defined`):endpoints.js 的 omp-parity import 被误删;单测未抓到因无测试调用 registerEndpoints 本体。修复+补挂载冒烟测试(桩引擎断言 6 条核心路由)。
2. **压缩后标题被摘要污染**:`/compact` 文本经 prompt 触发 `maybeStartTitleGeneration`,模型返回长摘要被接受。修复:`/` 前缀命令跳过标题生成(TUI 语义)。
3. **生产 Node 进程载 SDK 崩溃(pi-natives)**:routes.js 顶层 import SDK。修复:agent-dir 由 omp-host(bun)`GET /agent-dir` 权威解析,web 侧缓存+`~/.omp/agent` 回退;补 3 测。
4. **压缩后 transcript 空白**:live 上下文压缩后不含 user 消息,turn 分组全不放行。双修复:引擎读投影在 live 少于文件时退回文件冷读(完整历史);UI turn 分组把无父 T2 divider 归入 ungrouped 通道(补回归测)。
5. **深链冷启动渲染起始页(P1)**:`?session=` 冷加载时消息已拉取、选择已落盘,但 ChatContainer 自动草稿 effect 以**陈旧闭包**判定 currentSessionId(路由应用后仍以旧值 null 重开草稿并清掉选择),叠加侧栏 layout effect 在目录 bootstrap 前抢开草稿。修复:守卫改读 store 即时值 + URL 带 session 参数时禁止自动草稿。验证:冷加载直达 transcript(hi+回复+divider,视觉判读+截图);275/275 与 type-check 复绿。

## 5. 遗留总账(2026-08-20 进度审查补全)

初版本节仅 4 条;同日按「计划 vs 实现」全量对账补全(方法:8 章 ~109 个 GAP/残留项 × 代码 file:line 审计)。总量裁定:

| 章 | ✅ | 🟡 | ❌ | ❌ 中到期欠账焦点 |
|---|---|---|---|---|
| 01 模型角色 | 3 | 5 | 2(+1 未证) | GAP-02 每 prompt 显式 model 未撤(client.ts:896-902 仍恒带);GAP-08 fallback 徽标仅 schema 无渲染 |
| 02 模式与 Agent | 1 | 6 | 7 | B02/B04/B05:persona 选择器、统一 .omp/agents 存储、chips 表单 |
| 03 审批与对话框 | 0 | 6 | 3 | P0 六件套 UI 半边全缺(租约客户端/弹窗/dialog store);C7/C9/C10 零落地 |
| 04 协议与实体 | 5 | 3 | 9 | GAP-05/06 navigate 与 /undo /redo 重定基(P1) |
| 05 事件流 | 10 | 5 | 1 | E05 todo_auto_clear 零命中;E03/E04 渲染尾款 |
| 06 设置 | 4 | 3 | 5 | F2/F3/F9/F12;F9 已过「随 02 上线」时点(App.tsx:410 等三端仍读 flag) |
| 07 残留清除 | 7 | 4 | 2 | G02 升级 toast 仍在场(spec 允许提前删) |
| 08 原创面 | 2 | 1 | 10 | GAP-01 四表单 role 迁移(P0)等 6 项 P0/P1 |
| **合计** | **32** | **33** | **39** | ❌ 39 = 到期 23 + P2/P3 未到期 16;另 03 章 4 项 P3 删除未启动 |

### 5.1 半落地的统一模式
🟡 的 33 项几乎全部同构:**服务端域模块+测试全绿,UI 消费面缺失**。dialogs/settings/tree/agent-runs/jobs/queue 六端点在 UI 的唯一触达 = `omp-resync.ts:150-168` fetch 即丢 + `omp-event-reducer.ts:701-706` no-op 占位;local:// 链接在 transcript 仍纯文本(OMP_ENDPOINTS 无 uri 路径);drafts 服务端端点未建。

### 5.2 到期欠账明细(P0/P1,下一批次输入)
- **03**:C4/C5/C8/C13 的 UI 半边(ApprovalDialog/AskDialogModal/dialog store/租约客户端;UI 侧 grep `leaseId` 零命中)、C7(设置页审批区)、C9(通知/tray)、C10(WorkStatus/eviction 切换)
- **01**:GAP-02(发送体去 model)、GAP-08;尾款 GAP-05(全模型列表「设为角色」)/GAP-06(composer thinking 槽仍 variant)/GAP-10(enabledModels 无过滤与警示)/GAP-11(导入横幅)
- **06**:F2(级联改读 omp)、F3(DefaultsSettings 重构)、F9(plan flag 停读)、F12(omp tab 范围)
- **08**:GAP-01/02/03/04/05/08(四表单迁移、persona 分型、scheduled 字段、subagents 切 agent-runs、goal row 换源、slash 三层管线——omp-host grep `/commands` 零命中即端点未建)
- **02**:B02/B04/B05;**04**:GAP-05/06;**05**:E05(+E03/E04 渲染尾款);**07**:G02

### 5.3 未到期未启动(P2/P3,计划内,不算欠账)
04 章 P2 实体面(树 UI/Agent Hub/parked/尾读/subagent HUD/drafts/artifacts 浏览/P2 schemes);02 章 B09-B14(vibe/loop/prewalk-advisor 面板/btw/tan/goal 续跑);05 章 P2(message.part.removed 回收、ttsr/thinking 完整渲染);08 章 GAP-06/10/12/13;07 章 P3 删除批(观察期未起算)。

### 5.4 已在场、待真实触发/真机验证
- 模式菜单开合:无头环境 container-query 渲染移动变体,无法驱动;组件级 11/11 已断言,待真机手验。
- notice toast / retry 徽标 / cache-miss 分隔线视觉:需真实失败/压缩注入,仅状态级测试背书。
- 01 GAP-09(sidecar meta.model 双写收敛)未验证,不计入上表裁定。

### 5.5 后续批次
见 `00-MASTER.md` §8:批次 1=审批/ask UI 面(含 G02)→ 2=role 闭环+模型事件尾款(含 E05/GAP-09)→ 3=设置页 → 4=agent 与模式面(含 F9)→ 5=会话树与命令管线 → 6=P3 删除列车(前置=观察门)。评审记录(2026-08-20 gpt-5.6-sol:reject → 修订 → 复审 approve-with-changes,5 项必改已落位)见 §8.4。

---

## 6. 实施夜批次进展(2026-08-20 深夜批次 1-5;§6.1-6.3=批次 1-2,§6.4-6.7=批次 3-5 与收尾门禁)

### 6.1 批次 1:审批/ask UI 面(8/8 完成,浏览器验收闭合)

| 项 | 落点 | 证据 |
|---|---|---|
| dialogs API 工厂+store+租约客户端 | `packages/ui/src/lib/api/omp.ts` dialogs 域、`useOmpDialogStore`、`sync/omp-dialog-lease.ts` | UI 隔离套件全绿;`advance(now)` 状态机 fake-timer 测 |
| ApprovalDialog/AskDialogModal/OmpDialogLayer 挂载 | ChatContainer `withOmpDialogLayer` 包裹全分支 | **live**:oh-my-pi 项目 `/review` 命令 → 真实 SDK select 弹窗 → 选择后 `GET /api/omp/dialogs` 空(已结算) |
| ask 答案卡 / C9 tray+通知 / C10 WorkStatus | AskAnswerCard + useTraySync + WorkStatusSubagentsSection(`hasOmpPendingDialogs` 守卫) | 组件测全绿 |
| G02 升级 toast 删除 | 服务端 2 端点+3 文件删除;useUIStore v14→v15 迁移;settings-runtime migration9 | `rejects-retired-fields` 测 + 五包 type-check 绿 |
| 批次门 | — | UI **284/284**+tsc;omp-host **225/225**;VS Code 23+15;Electron 15;`check:events` OK |

批次 1 live 验收修复(8 项):engine `#materialize` 租约竞态(`#setDialogUiContext` 预发布)、ExtensionRunner `initializeExtensions` UI 注入(`extensionUiPromise`)、OmpDialogLayer 早退分支提升、SSE 断流 `onActive` 对账、React #185(store selector 引用稳定性)、`check-event-coverage` 正则误报。

### 6.2 批次 2:role 闭环+模型事件尾款(6/6 完成,live 验收闭合)

| 项 | 落点 | 证据 |
|---|---|---|
| GAP-02 发送链路去显式 model | `client.ts` `omitPromptModel` 捕获于请求起点(promptAsync+command 双通道);engine 侧显式 model 兼容保留(旧 UI 矩阵) | client.test 10 测(legacy/roles 矩阵+in-flight 捕获)+ engine compat 测 |
| GAP-01 四副表面迁 role | GitHubIssuePicker/NewWorktree 双对话框 `resolveOmpDefaults`;ScheduledTask `execution.modelRole:'default'`(类型可选化+校验放宽+runner 省略 model);`worktreeSessionCreator` legacy pin 门控;queue 路径 `useQueuedMessageAutoSend` roles 下无标识符合法派发 | project-config 3 新测(modelRole 持久化/无 modelRole 拒绝/半针定拒绝);scheduled runtime+loops 25/25 |
| 06 F2 级联改读 omp | `useConfigStore.cascade.ts` 抽出共享级联(roles on→仅 roles.default 输入,legacy 层全退役;off→完整 legacy);loadAgents/applyDefaultModelAgentSelection/resolveProviderModelSelection/setAgent/provider-load 五读点门控 | cascade 4 契约测 + 全 UI 套件绿 |
| GAP-06 thinking 槽 | `/omp/models` 新增 `models[]` 投影(thinking.supported/defaultLevel,registry 失败降级 roles-only);`setSessionModel` thinkingLevel(同模型短路 `setThinkingLevel`);ModelControls variant 槽 roles 分支(Inherit/Off/Auto+supported) | domain-models 2 新测;engine thinking-only 短路测;**live**:菜单 [Inherit,Off,Auto,Low,High,Max] → 选 Max → transcript `thinking_level_change configured:"max"` + `omp.thinking.changed` 事件(id 5)→ 重连重放后槽显示 **Max** |
| GAP-05 设为角色+GAP-10 过滤 | 全模型行尾「Set as role」角色菜单(经 `OmpSettingsAPI.putModelRole`→PUT /omp/settings `modelRoles.<role>`);`lib/omp/enabledModels.ts` 模式匹配器(exact/bare/glob/`:thinking` 剥离)过滤双 picker+排除警示行 | matcher 4 测 + ompRoleModeSurfaces 源断言;**live**:tiny 角色 UI 赋值 → 服务器真值 glm-5.3→glm-5.2(GAP-11 检测源 `legacyDefaults` 就绪) |
| E03/E04/E05+GAP-09 | ChatMessage 徽章兜底链 roles 下= message.info→omp badge(localStorage 兜底退役)+fallback 标记(完成于 fallback 之后);StatusRow compaction loader(优先级:aborted>compacting>working);todo_auto_clear→`todo.updated []`(reducer 消费);GAP-09=model_changed registry 同步既有测(dispositions 155-172)断言通过 | i18n fallbackTag/compacting ×11;全套件绿 |

批次 2 live 验收补充证据:草稿模型触发器显示 `GLM-5.3`(=roles.default,F2 级联 live 证明);model-free 提示词真实运行(transcript `model:"glm-5.3", agent` 缺省);wire `Session.model` live 合并(`#wireSessionFromLive`/listSessions)供徽章播种(01 §5.5)。批次门:UI **287/287** 文件+tsc;omp-host **229/229**;`check:events` OK(24/24/25)。

### 6.3 批次 1-2 期间的环境事实
- 全量 `bun test`(单进程)存在既有 mock 泄漏(248 失败,与批次改动无关,基线取证:排除新测试文件后失败数不变);官方门=`bun run test`(scripts/run-isolated-tests.mjs 隔离运行器)全绿。
- `packages/web/server/lib/scheduled-tasks/issue-2710-double-execution.test.js` 的 `vi.hoisted` 在 bun 下不可用(模块加载期失败,既有环境缺口,与本批无关)。

### 6.4 批次 3:设置页(4/4 完成,live 验收闭合)

| 项 | 落点 | 证据 |
|---|---|---|
| F1 schema 驱动设置页 | `OmpEngineSettingsPage`(settings.v1 门控;tabs→groups→typed 控件;per-key PUT + rejected 内联;credential writeOnly 掩码) | **live**:Engine Settings 页渲染 tabs/组;枚举/布尔/只读(数组/record)控件;`tools.approvalMode` 选择 PUT 落盘(revision+1,config.yml 真值) |
| F3 DefaultsSettings 重构 | `OmpModelRolesEditor`(10 角色行 + source 徽章 + defaultThinkingLevel/modelRoleStorage 选择);legacy 三件套 capability-off 保持 | **live**:全部 10 角色带模型与 Global 徽章;Default Thinking Level=High;Role Storage=Global |
| GAP-11 导入横幅 | `shouldOfferLegacyImport` 纯函数(offer/comparison/none)+ 显式导入按钮 | 单测 3 例(offer/comparison/永不覆盖);live:legacyDefaults=null → 横幅正确隐藏 |
| C7 审批设置区 | Approvals 区渲染 schema 暴露的审批键 | **live**:Ask Timeout/Ask Notification/Bash Approval Patterns(只读)/Tool Approval Policies(只读)/Tool Approval(枚举)/Hide Thinking Blocks 等 |
| 批次门 | — | UI **289/289**+tsc;验收修复 1 项:角色 Clear 按钮缺 onClick(补 + 徽章行恢复) |

### 6.5 批次 4:agent 与模式面(5/5 完成)

| 项 | 落点 | 证据 |
|---|---|---|
| B02 persona 选择器 | `OmpPersonaSelector`+`useOmpPersonas`(3-way 分支:modes>personas>legacy;personas 进 mode 菜单 extraSection) | 单测(capability-off 字节等价);live:`/api/omp/personas` CRUD 真值 |
| B03/B04 agent-definitions CRUD | `OmpAgentDefinitionsAPI`(409 conflict/501 unavailable 映射)+ useAgentsStore omp 分支 + agent-manager omp 表单(scope 徽章;UI 零文件写) | **live**:POST global agent → 列表真值;DELETE 清理;无导入端点(服务端未暴露 → 按契约渲染无) |
| B05 chips 表单 | `agentTaskOverrides`(task.disabledAgents/agentModelOverrides/agentPrewalk/agentAdvisor 非破坏性整键写 + rejected 内联) | 服务端 overridesFor join 注入 domain-modes;单测全绿 |
| B07 plan overlay | `OmpPlanReviewOverlay`(omp.plan.review_requested 驱动;approve-execute/compact/keep/refine → POST plan/review) | 端点形状按 domain-modes.js 亲验;reducer 兼容全绿 |
| B08 goal 面板+F9 | `OmpGoalIndicator`(读 omp.goal.updated + mode snapshot 冷启;无写端点=按设计只读);F9 三端 planModeExperimentalEnabled 在 modes.v1 下停读 | 单测;grep 确认 flag 消费点全部门控 |
| 08 GAP-02/03 | 双对话框 personas.v1 下不合成默认 agent;ScheduledTask persona 字段(端点缺 → 字段渲染、不发送,报告留档) | omp-defaults 测 + 对话框测 |

### 6.6 批次 5:会话树与命令管线(5/5 完成)

| 项 | 落点 | 证据 |
|---|---|---|
| 04 GAP-01/02 local:// 查看器 | `internalUri.ts`(纯分类/匹配)+ markdownCore scheme 贯通 + `InternalUriViewer`(markdown/json/代码;错误内联) | 单测 83 绿(capability-off 渲染字节等价断言) |
| 04 GAP-04 树 | `SessionTreeDialog`(DFS 行/叶标记/选分支重拉时间线;`/tree` 命令) | **live**:`GET /api/omp/sessions/{id}/tree` 真实谱系;行构造单测 |
| 04 GAP-06 /undo /redo | handleSlashUndo omp 分支(叶父回退+composer 预填;首消息回退 legacy) | slashUndoTree 3 测 |
| 08 GAP-04/05 | `useOmpAgentRunsStore`(agentsRevision 跳变重取)→ WorkStatusSubagents;goal row 切 useOmpGoalState(只读) | 10 测(legacy 回归+agent-runs+goal) |
| 08 GAP-08 slash 管线 | **新建** `domain-commands.js`:`GET /api/omp/commands`(client-builtin 70 全注册表 + engine 层 headless 聚合,失败降级 builtin-only)+ `commands.v1` 能力;CommandAutocomplete 四层合并(omp 优先+覆盖徽章);/debug→/troubleshoot 一次性迁移提示 | **live**:101 命令(70+31),debug 存在/troubleshoot 别名;8 服务端测;UI 合并 11 测 |
| navigate(GAP-05) | 服务端无 navigate 端点(normalizeNavigateRequest 未挂载)→ 按指令并入 GAP-04 选择语义 | doc-vs-impl delta 留档,无 stub |

### 6.7 批次 3-5 门禁汇总
- UI 隔离套件 **293/293** 文件;tsc 五包 0 错误;omp-host **237/237**;VS Code 23/23;Electron 15/15;`check:events` OK(24/24/25);web vitest 34 失败=文档化基线(无新增)。
- 验收修复:角色 Clear 按钮 onClick 缺失;`openchamber:omp` localStorage 前缀违例(check:events 抓获 → `oc-omp-`);并发代理编辑丢 `setGitChangesViewMode`/`useChatTimelineController`/`ompModes` 工厂行(逐一恢复,类型 0 错)。
- live 验收覆盖:settings 页三区渲染+两处写落盘、commands 101 条两 tier、definitions/personas CRUD 往返、tree 真谱系、mode selector DOM 在场;persona 菜单交互/plan overlay/goal 指示器因无头 palette 覆盖层与需活动 plan/goal 状态未做点击级驱动——组件级单测背书,列为待真机项(对齐 §5.4 遗留口径)。

### 6.8 观察门计时起点登记(总纲 §8 07 章观察期)

- **起点**:2026-08-20(实施夜,批次 1 含 C9/C10 完成且 P1 消费者切换全量上线——批次 1-5 全部落地,dialogs/permission P1 消费面已切 omp 面)。
- **窗口**:≥14 天且跨一次完整回归;分母 = 对话框总结算数;阈值(冻结):legacy permission/question 命中 = 0、orphan 结算率 < 1%、respond 超时率 < 1%;最小样本 = 100 次结算,不足延长一个窗口(无数据 ≠ 通过)。
- **回滚**:capabilities 摘 `dialogs.v1`(服务端裁决,无本地 flag)。
- **批次 6(P3 删除列车)以此门通过为前置,今晚不执行**。

### 6.9 终审对照(§8.2/§8.3)

- 批次 1-5 交付内容与 §8.2 各批行逐项对齐(见 §6.1-6.7);deviation 留档:①批次 4 B04「存量导入」无服务端导入端点 → UI 不渲染(契约规定);②批次 4 08 GAP-03 scheduled persona 字段渲染但不发送(引擎 prompt 契约无 persona 通路);③批次 5 04 GAP-05 navigate 无服务端端点 → 并入 GAP-04 选择语义;④批次 4 B08 goal 无写端点 → 指示器只读(ModesFaces/TreeCommands 双向确认)。
- §8.3 固定命令门:五包 type-check 0 错、UI 隔离套件 293/293、omp-host 237/237、check:events OK、dead-code 检视(仅既有 file-local interface 噪声+1 重复导出)。计时类测试均注入 clock/advance,无真实定时器依赖。
- 技术债已偿还(2026-08-21):ompRoleModeSurfaces.test.tsx 的 readFileSync 源文本断言全部移除——表面切换/思考槽契约改由 **ompComposerSurfaces.test.tsx**(renderToStaticMarkup 行为测,mock `useOmpModelRoles` 权威态缝 + zustand v5 SSR 初态语义下的 session/readiness 缝)承载;级联断言删除(useConfigStore.cascade.test.ts 4 项行为测已在);enabledModels 存在性断言删除(自有测试文件);resolveSendAgent 补 personas.v1 三参矩阵。批次 6 前置改造项清零。门:UI 隔离套件 **294/294**(+1 文件)、tsc 0 错、dead-code 仅既有噪声。

### 6.10 批次 7:扩展宿主面 09 章 E01/E02/E08(2026-08-21 晚,用户真实扩展驱动)

**交付**(spec 09 §5;R-E1 镜像 RpcExtensionUIRequest / R-E2 被动面不租约门控 / R-E3 可观测丢弃):

| 项 | 落点 | 证据 |
|---|---|---|
| E01 widget 投影(host) | 新 `domain-chrome.js`(表 + `omp.chrome.updated` volatile 事件 + `GET /api/omp/chrome` 501/400/200 契约;目录键 normalizeDirectoryKey 归一,对齐 domain-dialogs 先例);dialog bridge 的 `setWidget/setStatus` 由 no-op 改为委托,R-E3 工厂载荷/终端面按方法计数入快照 `dropped` | domain-chrome.test 16 测(含 bridge 集成:无 chrome 时保持 no-op) |
| E01 UI | reducer `chrome` 切片 + `omp.chrome.updated` case(set/clear/幂等 no-op/畸形丢弃);`reconcileChromeSnapshot` 权威对账(内容级相等则引用稳定);resync 矩阵新增 `chrome` scope(次序 dialogs→chrome→settings);`OmpExtensionWidgetBar`(verbatim 行、placement 过滤、updatedAt 稳定排序、空态渲染 null——数据在场即能力门)挂接 ChatContainer 全部 5 个 composer 位点 | reducer 3 测 + WidgetBar 4 测 + store 3 测 + pipeline 次序断言更新 |
| E02 status 投影 | host 表同车(status set/clear);UI store/reducer 同切片;StatusRow 消费面未接(见 deviation ①) | domain-chrome.test status 组 |
| capability | `extensionChrome.v1: true` 注册;`omp.chrome.updated` 入事件注册表(check:events 25→**26**)+ crossChapterEvents 生产者登记 | check:events OK |
| i18n | `chat.extensionWidgets.ariaLabel` ×11(真实翻译;fr 撇号转义;es/pt-BR/uk 双引号风格) | grep 11/11 |

**live 验收(用户真实扩展 `~/.omp/agent/extensions/zhipu-usage.ts`,635 行)**:

- 托管栈(3903 web → 引擎子进程):建会话 → `POST /api/omp/dialogs/lease` → 14s 后 `GET /api/omp/chrome` 返回 **revision 1,zhipu_usage widget 5 行真数据**(GLM Coding Plan · Max / MCP 1mo 0% / Token 5h 64% / 24h 4,041 次 1.5B token / 重置倒计时)——与 TUI renderWidget 逐字一致
- 诊断期独立引擎(直连 host.js)交叉证实扩展生命周期:lease → initializeExtensions → `session_start` → sandbox notify + autoresearch clear + zhipu setWidget(rows:5, aboveEditor)

**验收中修复的两个真实缺陷**:

1. **lease 先于惰性物化**(engine.js #attachDialogUi):UI 租约挂接时 `sessions.get()` 尚无会话 → 扩展 UI 初始化被静默丢弃(web 引擎子进程日志被 lifecycle.js 丢弃故不可见;独立引擎复现)。修复:attach 视为会话访问,缺失时先 `#materialize`
2. **目录键未归一**:web 代理 realpath 把查询目录规范化为反斜杠形式,扩展上下文携正斜杠键 → 快照永远 miss。修复:domain-chrome 全入口 `normalizeDirectoryKey`(domain-dialogs 同款契约)

**门**:UI 隔离套件 **296/296**(+2 文件)、omp-host **253/253**(+1 文件)、五包 tsc 0 错、check:events OK(26 事件)、dead-code 仅既有噪声。

**deviation 留档**:①E02 StatusRow 消费面未接(host 侧就绪,UI 段渲染留待 E01 真机反馈后同车);②无头浏览器 open 持续超时(SSE 长连),WidgetBar 视觉确认待真机——组件级 4 测背书,与 §6.7 待真机口径一致;③E03/E04(setEditorText/title/open_url)按批次表随车项未做(09 章批次 7 行的"随车"为可选,主项 E01/E02/E08 已闭)。
