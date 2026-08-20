# omp-parity 实施验收证据总录

状态:2026-08-20 实施夜验收完成(用户指定的 `/compact` 压缩样式验收已闭合)
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

## 5. 遗留(未闭合项)
- 模式菜单开合:无头环境 container-query 渲染移动变体,无法驱动;组件级 11/11 已断言,待真机手验。
- notice toast / retry 徽标 / cache-miss 分隔线视觉:需真实失败/压缩注入,仅状态级测试背书。
- 审批弹窗 UI 消费面、会话树/agent-runs 侧栏面、设置页迁移、agent-definitions 管理页:服务端就绪,UI 面属下一批(spec 分阶段)。
