# 03 · 审批与交互(Approvals & Dialogs)—— 域 C

状态:设计稿(v3,第二轮评审修订 2026-08-20)
基线:omp SDK `@oh-my-pi/pi-coding-agent`(安装副本 `<s>` = `C:/Users/reamd/.bun/install/global/node_modules/@oh-my-pi/pi-coding-agent/src`);OpenChamber 仓库根 = 本仓库。
裁决依据:00-MASTER.md D1(双轨契约)、D2(投影权威)、D3(permission/question 协议删除,omp 审批 = approvalMode/tier + tool_approval 弹窗桥 + ask 对话框)、**D6 冻结契约 R1/R2/R3/R4/R10/R11/R12** —— 与本章 v1 冲突处以 D6 为准。

> **v2 修订摘要**(评审 12 高/15 中中落地本章者):R10 审批桥原子化(C3+C4+C5+C8+C12 为单一 P0 能力,capability `dialogs.v1` 唯一启用开关,无人值守任务 fail-closed);R1 事件改名 `omp.dialog.requested/settled` 并归 05 章唯一通道;R4 进程归属 omp-host + 认证头转换表;R11 pending 生命周期硬化(含四组测试);R12 清理排程 P0 原子桥 → P1 消费者切换 → P3 删除。变动节以 **[REVISED v2]** 标记;被否决的 v1 方案以删除线或"作废"注明,不留并行设计。
> **v3 修订摘要**(第二轮评审 7 高/12 中中落地本章者):**R2-H1/H7(高)** capability 与 UI presence 解耦 —— `dialogs.v1` 只声明服务端桥就绪,**不直接驱动任何会话的 `hasUI`**;会话交互性由 **per-session UI attachment 租约**(认证 + 心跳/过期/引用计数)决定,无人值守、旧 UI、断线会话永不持租约 → 精确保留 SDK fail-closed 抛错;对话框加**双保护 TTL**(presented-ack 锚点 + 租约丢失 orphan 窗口)消除无限悬挂(§5.0/§5.1 D-C1b/§5.4.3/§5.6.1)。**R2-M1** 弹窗按钮契约收敛为 Approve/Deny 两键,"始终允许"改为高级操作事务(写设置成功才批准,失败不批准,§5.3.2)。**R2-M2** 超时锚点改为客户端 presented-ack + 注册期未呈现保护 TTL(§5.4.3)。**R2-M8** vendored wire 处置收敛为"仅删消费引用,gen 不动"(§5.8 P3-14)。变动节以 **[REVISED v3]** 标记;v2 与之冲突的表述(capability 直驱 hasUI、"挂起至超时"、第三按钮、超时近似、裁剪/重生成二选一)注明作废,不留并行设计。

---

## 1. 域概述与边界

**本域管什么**

1. **删**:OpenCode `permission`(V1+V2)与 `question` 协议的完整链条 —— omp-host 空桩端点、wire 事件类型、UI reducer/store/卡片/toast/托盘/auto-accept/AgentPermissionsEditor/驱逐联动,以及发送路径上的"自动拒绝"行为。删除排程按 R12:**P1 只切消费者,删除在观察期后的 P3**(§5.8)。
2. **建**:omp 审批桥与 ask 对话框桥 —— omp-host 内嵌 SDK 的无头 `AgentSession` 获得一个 **WebUIContext**(`ExtensionUIContext` 的 web 实现),`select/confirm/input/askDialog/notify` 经 OpenChamber 自有面(RuntimeAPIs `/api/omp/dialogs` 端点组 + **05 章唯一事件通道 `OmpEventBus → /api/omp/events` 上的 `omp.dialog.requested`/`omp.dialog.settled`**)转发到浏览器,弹窗应答回投后 resolve 工具执行。**GAP-C3+C4+C5+C8+C12+C13 为单一 P0 原子交付单元,capability `dialogs.v1` 是唯一启用开关(R10,§5.0);会话交互性(`hasUI`)由 per-session UI 租约决定,capability 不直接驱动任何会话的 `hasUI`(v3,R2-H1/H7,§5.1 D-C1b)**。
3. **配套**:approvalMode / tools.approval / bash.patterns / ask.* 设置键在设置面的呈现(实现归第 06 章,本章定义交互语义);审批与 ask 等待态的通知集成(tray/OS 通知,复用第 08 章通知系统);**UI 租约与心跳生命周期(§5.1 D-C1b)**;页面关闭/重连时的对话框背压与重放;**宿主 shutdown/dispose/restart 的 pending 对话框生命周期(R11,§5.6.5)**。

**本域不管什么**

- 设置读写通道与 schema 代理(第 06 章);本章只定义"审批设置如何影响弹窗与弹窗如何反写设置"。
- wire `session.next.*`、share、shell 等其他残留(第 07 章);本章只负责 A3(permission)/A4(question)两项的删除设计与执行顺序。
- custom agents 的权限字段删除涉及的 agent 模型收敛(第 02 章主导,本章提供 AgentPermissionsEditor 删除清单)。
- 事件流 drop-switch 的总体治理与 omp 事件通道本体(第 05 章):本章的 `omp.dialog.*` 两事件只在 05 章注册表**登记**(§5.2),envelope/事件 ID/重放/schema 版本以 05 章为唯一权威(R1);`tool_approval_requested/resolved` 是扩展包装器内部事件,不是 `AgentSessionEvent`,不进 session 事件通道(§3.2),仅作诊断。

**与其他章的接口契约** [REVISED v2]

| 接口 | 方向 | 内容 |
|---|---|---|
| Ch05 事件流 | 依赖 | `omp.dialog.requested/settled` 经 05 章唯一 `OmpEventBus → /api/omp/events` 通道;本章只登记注册表行,不自建通道(R1)。`GET /api/omp/capabilities` 的 `dialogs.v1` 状态由 05 章端点组承载(R2);dialogs 位于 D2 bootstrap 顺序(modes/model 之后),该步 = **UI 租约 acquire + 快照对账**(§5.6.2) |
| Ch06 设置 | 依赖/被依赖 | 读取 `tools.approvalMode`/`tools.approval`/`bash.patterns`/`ask.*`;弹窗高级操作"始终允许"反写 `tools.approval`(**事务语义:写设置成功才批准,失败不批准**;用户显式操作,与无人值守任务的禁改令不冲突,§5.3.2/§5.3.3) |
| Ch07 残留 | 后置 | 本章 §5.8 删除清单是 Ch07 A3/A4 的执行细则;删除发生在 P1 消费者切换 + 观察期之后(R12) |
| Ch08 原创面 | 被依赖 | scheduled tasks 的 `permissionAutoAccept` 输入删除;**无人值守任务 fail-closed、不改全局审批设置(R10);per-session approval-mode 端点依赖取消**(master D6 R10 对 08 章的裁决,该端点从未在本章定义、也不得在 08 章依赖);审批/ask 等待接入通知系统与 tray |

---

## 2. 现状分析(OpenChamber 侧)

### 2.1 服务端:空桩 + 无头会话 = 双重不可用

**端点层**:omp-host 把 permission/question 全部回答为空(`packages/web/server/lib/omp-host/endpoints.js:334-344`):
- `GET /permission` → `[]`;`POST /permission/{requestID}/reply` → `{}`(338-339)
- `POST /api/session/{sessionID}/permission` → `{id:'',effect:'deny'}`(340);`GET /api/session/{sessionID}/permission/{requestID}` → 404(341)
- `GET /question` → `[]`;`POST /question/{requestID}/reply|reject` → `{}`(342-344)
注释明言:"OpenChamber's permission/question protocol has no live producer yet … until the approval bridge lands"(endpoints.js:335-337;omp-host/DOCUMENTATION.md:114-116)。

**引擎层(关键现状,scout 报告未展开,本章实测)**:omp-host 创建会话时不传任何 UI 能力 —— `engine.js#materialize` 的 `createAgentSession` 调用仅含 `cwd/sessionManager/authStorage/modelRegistry/agentRegistry/model/planYolo/systemPrompt/toolNames`(`packages/web/server/lib/omp-host/engine.js:454-473`),没有 `hasUI`,也没有 `setToolUIContext`。SDK 侧 `hasUI` 默认 `false`(`<s>`/sdk.ts:562-563,注释:"Whether UI is available (enables interactive tools like ask). Default: false")。后果:

1. **ask 工具不存在**:`AskTool.createIf(session)` 在 `session.hasUI` 为假时返回 `null`,工具根本不进注册表(`<s>`/tools/ask.ts:832-834)。即使代码里有兜底,执行到也会 `context.abort()` + `throw ToolAbortError("Ask tool requires interactive mode")`(ask.ts:856-860)。
2. **审批 fail-closed 硬错误**:SDK 的审批门在扩展工具包装器里,`!runner.hasUI()` 时直接抛错("Tool … requires approval but no interactive UI available. Options: 1. Set tools.approvalMode: yolo … 2. Add tools.approval.<name>: allow … 3. Use an interactive UI",`<s>`/extensibility/extensions/wrapper.ts:307-322)。也就是说:**今天任何把 omp 设置调成非 yolo(或对某工具设 `prompt`)的 OpenChamber 用户,对应工具调用会直接变成工具错误**,而不是弹窗。这是本域最高优先级的用户可见缺陷;R10 裁决其修复(桥)必须整体原子交付。

### 2.2 wire 契约层(死类型)

`packages/ui/src/lib/opencode/wire/gen/types.gen.d.ts`(vendored,不可手改):
- V1:`permission.asked`(:1133-1146)、`permission.replied`;`question.asked/replied/rejected`(:1237-1260)
- V2:`permission.v2.asked/replied`(:1028-1047,`PermissionV2Source`/`PermissionV2Reply`)
- `PermissionRequest`(:2035+,字段 `permission/patterns/…`)、`QuestionRequest`(:2021-2034,`questions` 数组)
- `PermissionRuleConfig`(含 `todowrite/question/webfetch/external_directory`,:1347-1351)—— agent 权限编辑器的数据形状
- SDK 方法面:`permission.reply`/`question.reply`/`question.reject`/`question.list` 在 `client.ts:1163-1170/1337-1352/1364-1369` 有封装并有测试 `client.permission.test.ts`

### 2.3 UI 消费链(全量清单)

事件入口与 reducer:
- `packages/ui/src/sync/event-reducer.ts:521-576`:`permission.asked/replied`、`question.asked/replied/rejected` 五个 case,写入 `draft.permission[sessionID]` / `draft.question[sessionID]`
- `packages/ui/src/sync/sync-context.tsx:1527-1592`:`permission.asked` → VS Code auto-accept 拦截(:1529-1545)或会话级 auto-accept 吞掉(:1546-1549)或 toast(:1551-1559);`permission.replied` 撤 toast(:1562-1571);`question.asked` toast(:1573-1592)

组件与动作:
- `components/chat/PermissionCard.tsx`(:97-114 应答)、`QuestionCard.tsx`(:92-99、:284-299 拒绝);挂载于 `ChatContainer.tsx:373-378`(阻塞卡),作用域 hook `useScopedBlockingPermissions/useScopedBlockingQuestions`(:638-639,含 subagent 后代作用域),`hasUnreconciledQuestionTool`(:641)
- `sync/permission-toast.ts`、`sync/question-recovery.ts`(含各自测试)
- `sync/session-actions.ts:1548-1661`:`respondToPermission/dismissPermission/dismissOpenPermissionsForSession`(发送路径自动拒绝,:1597-1619 文档);question 路径 :1725-1785(发送即拒绝并排队)
- `hooks/useTraySync.ts`:permission/question 标签(:113-122)、托盘审批菜单(:274-275)、`respond-permission` 动作(:596-600)
- `components/chat/message/parts/ToolPart.tsx:1409-1447`:OpenCode `question` 工具的流内渲染器

auto-accept 体系(OpenChamber 自设权威,对 omp 永远空转):
- `stores/permissionStore.ts`(persist;快照来自 `/api/permission-auto-accept`,:47-49;`isSessionAutoAccepting` :51-55)+ 测试 `permissionStore.test.ts/.vscode.test.ts`
- 服务端:`packages/web/server/lib/permission-auto-accept/runtime.js:249-262`(`GET /api/permission-auto-accept`、`PUT .../sessions/:sessionId`),路由闸门 `core-routes.js:1067` —— **注意:这是 web server 进程内路由,R4 裁决后它只作为"待删残留"存在,不再是 omp-parity 端点的注册范式**(§5.2)
- `sync/vscode-permission-auto-accept.ts`(+测试):VS Code 宿主策略
- UI 开关:`components/chat/composer/ui/PermissionAutoAcceptButton.tsx`(:19-67)、`components/chat/permissionAutoAccept.ts`(:4-35)、`ChatInput.tsx` 草稿态 `permissionAutoAcceptEnabled`(:321-323)与装配(:2436-2462)
- 定时任务输入:`components/sections/session/ScheduledTaskEditorDialog.tsx:479/531/569/1174-1175/1688-1701`(`permissionAutoAccept` 字段)
- i18n:`chat.chatInput.permissionAutoAccept.*` 与 `sessions.scheduledTasks.editor.permissionAutoAccept.*` 共 10 个语言文件(如 `en.ts:2162-2166/320-321`)

agent 权限编辑器:
- `components/sections/agents/AgentPermissionsEditor.tsx` + `agentPermissionModel.ts`(+测试)
- `components/chat/ModelControls.tsx:1438-1447`(桌面)与 :2449-2458(移动)agent 选择器 tooltip 里的 edit/bash/webfetch 三行 allow/ask/deny 图标

辅助联动:
- `sync/eviction.ts:15-24` `hasPendingBlockingRequests`(有 pending permission/question 的目录永不驱逐,:32、:58)—— 注释自认这是在补偿"a permission request raised by a child session has no representation in the transcript"(`components/chat/work-status/WorkStatusSubagentsSection.tsx:19-22`)
- 目录 store 的 `state.permission`/`state.question` 字段与 bootstrap 重水化(eviction.ts:9-13 注释)

### 2.4 事件通道现状(基础设施已具备;v1 挂靠方案作废)[REVISED v2]

- omp-host `WireEventBus`:2048 条环形重放缓冲、单调 id、`Last-Event-ID` 续订(`packages/web/server/lib/omp-host/events.js:8-58`;SSE 入口 `endpoints.js:553-593` 读 `last-event-id` 头后 `subscribeSince`)—— 这是 wire 轨基础设施,dialog 事件**不进**此轨(D1/R1)。
- OpenChamber 自有合成事件:`openchamber:session-status/session-activity/notification/heartbeat` 经 `createGlobalUiEventBroadcaster` fan-out 到 SSE 与 WS 客户端(`packages/web/server/lib/event-stream/DOCUMENTATION.md:47-48`;广播行为测试 `runtime.test.js:86-99`);通知先例 `notifications/emitter-runtime.js:68-72`;UI 侧消费先例 `hooks/useWebNotificationStream.ts:21/37`。
- **命名沿革**:v1 曾设计 dialog 事件沿用 `openchamber:omp-dialog-*` 命名挂靠该广播器;**R1 裁决后作废** —— omp 原生事件一律走 05 章 `OmpEventBus → /api/omp/events` 唯一通道、命名 `omp.<域>.<事件>`(本章 §5.2 落实)。`openchamber:notification` 是 OpenChamber 原创通知面(Ch08)的数据载体,**非 omp 原生事件**,不受 R1 约束,继续走广播器(§5.3.4)。

### 2.5 进程归属、认证与宿主生命周期现状(R4/R11 的地基)[NEW v2]

**进程归属(R4)**:omp-host 是独立 HTTP 子进程(`packages/web/server/lib/omp-host/host.js`),由 web server 托管启动并注入 `OPENCODE_SERVER_PASSWORD`(lifecycle.js:526-528;launch 解析 `opencode/omp-host-launch.js:3-7`);web server 经 `registerOpenCodeProxy` 对 `/api/*` 做泛代理转发(含 hop-by-hop 头过滤,`opencode/DOCUMENTATION.md:388-397`)。**omp-parity 新端点照此拓扑:注册在 omp-host,web server 零新增路由。**

**认证**:omp-host 全路由 Basic auth(`Basic base64("opencode:<password>")`,host.js:36-39;不符即 401 + `WWW-Authenticate`,host.js:68-70)。web server 侧各接入形态的凭据(UI 密码/JWT 会话/relay bearer)在代理层经 `getOpenCodeAuthHeaders()` 转换为该 Basic 头(`opencode/auth-state-runtime.js:47-51`)—— 这是 R4 认证头转换表的现状锚点(§5.2)。

**宿主生命周期(R11 的靶子,现状三缺口)**:
1. 信号关停:SIGTERM/SIGINT → `host.close()` → `engine.shutdown()`(host.js:106-110、:120-126),`shutdown()` 逐会话 `#disposeSession`(engine.js:815-821)—— 但 `#disposeSession` 只退订 + fire-and-forget `agentSession.dispose()`(engine.js:135-142),**没有任何挂起中工具/对话框的结算路径**。
2. **dispose 路由绕过优雅关停**:`POST /global/dispose`、`POST /instance/dispose` 直接 `setTimeout(() => process.exit(0), 0)`(endpoints.js:158-165)—— 桥落地后若仍如此,进程退出瞬间全部 pending 对话框 Promise 悬死,transcript 无诊断。
3. 空闲清扫:idle sweeper 到期直接 `#disposeSession` 淘汰 live 会话(engine.js:126-131)—— 有 pending 对话框的会话被清扫同样悬死。

**现状判定**:permission/question 是一条"生产者已死、消费端仍在全速运转"的链 —— 20+ 文件的 UI 逻辑在对抗永远为空的协议;而 omp 引擎真正的两个交互面(审批门、ask)因 `hasUI:false` 被双双禁用。本域工作 = 一边以**原子单元**把引擎的两个交互面接出来(P0),一边按 R12 排程切消费者(P1)、删旧链(P3);同时补齐三个生命周期出口的 pending 结算(R11)。

---

## 3. 目标语义(omp/TUI 侧)

### 3.1 审批模型:tier + 三步解析,无持久权限记录

设置键(`<s>`/config/settings-schema.ts):
- `tools.approvalMode`:enum `always-ask|write|yolo`,**默认 `yolo`**(:3674-3708,interaction 页 Approvals 组;语义注释:always-ask 只自动批 read;write 批 read+write;yolo 全批但 user policy 仍可 prompt/block)
- `tools.approval`:record `<tool|policyKey> → allow|prompt|deny`,默认 `{}`,**任何模式下都被尊重**(:3662-3672)
- `bash.patterns`:有序规则 `{match, approval}`,仅 `*` 通配(:3481-3491);消费在 `BashTool.approval()`:`deny` 规则 → tool 级 deny;命中 `CRITICAL_BASH_PATTERNS` → 强制 prompt;`allow` 规则降 tier 为 write;`prompt` 规则强制 prompt(`<s>`/tools/bash.ts:546-572)

解析(`resolveApproval`,approval.ts:120-219):
1. 工具自身 `approval(args)` 声明(默认 tier `exec`;可带 `policyKey` 走 `tools.approval.<policyKey>` 覆盖,:107-113)
2. 用户 `tools.approval` 覆盖(:128-134)
3. 模式 tier 比较(`APPROVAL_MODE_MAX_TIER`:always-ask≤read、write≤write、yolo≤exec,:41;`modeApprovesTier` :100-102)

产物 `ResolvedApproval {policy: allow|deny|prompt, tier, override, source: tool|user|mode, reason?, policyKey?}`。`deny` 抛错带修复提示(`requiresApproval` approval.ts:235-243)。**没有任何持久化 permission 记录、没有 per-message 权限徽章** —— 与 OpenCode 协议的根本差异。

### 3.2 审批门的位置:扩展工具包装器(桥必须模拟的东西)

sdk.ts 用 `ExtensionToolWrapper` 包装整个工具注册表(只要 ExtensionRunner 存在,wrapper.ts:354-358 注释;runner 无条件创建于 sdk.ts:2555)。每次工具执行(`<s>`/extensibility/extensions/wrapper.ts:171-346):
1. 预解析 deny 短路(:189-200)→ `tool_call` 事件(扩展可改输入,:202-239)→ 对**实际执行参数**重新全量解析(:241-268)
2. `approvalCheck.required`(prompt 且非 xdev 旁路,或 provider 安全检查待确认,:263-268)时:
   - 等 `waitForToolApprovalPreview`(:271-277,engine.js 已可见的 `tool_execution_start` 之后)
   - 向扩展 handler 发 `tool_approval_requested {sessionId, toolName, toolCallId, reason?, approvalMode}`(:279-291;事件类型 types.ts:871-886,注册口 :1245-1246)
   - **`!runner.hasUI()` → fail-closed 抛错**(:307-322)
   - 有 UI:`uiContext.select(safetyPrompt, ["Approve","Deny"])`(:324-332;`safetyPrompt = formatApprovalPrompt(tool, args, reason)` —— 即 TUI 弹窗正文,approval.ts:258-279,含 "Allow tool: <name>"、mcp 来源标注、`formatApprovalDetails` 明细行如 `Command: …`)
   - 选择后发 `tool_approval_resolved {approved, reason?}`(:293-303、:338);Deny → `throw "Tool call denied by user: <name>"`(:339-341)
3. TUI 侧重:`extension-ui-controller.ts:130` `setToolUIContext(uiContext, true)` 装入;工具卡处于 pending、工作标题从 working 变 attention(`event-controller.ts:1426-1441`,`#toolWillPromptForApproval` 与 wrapper 同输入镜像解析);warp 集成把两个事件桥为 `permission_request/permission_replied`(`warp-events.ts:197-207`)= **外部表面桥接审批的直接先例**。

外部宿主先例:RPC 模式整类 `RpcExtensionUIContext implements ExtensionUIContext`(`rpc-mode.ts:734-926`:select :740-758、confirm :760-776、input :778-791、notify :798-807、editor :884-891;装配 :931-932 `setToolUIContext(rpcUiContext, true)`;请求/应答经 `extension_ui_request` 帧 + `pendingExtensionRequests` 表配对)。ACP 模式另有四选项权限门 `allow_once/allow_always/reject_once/reject_always`(`acp-permission-gate.ts:16-21`)—— 是 ACP 协议要求,非 omp 本体语义。

### 3.3 ask 工具与 askDialog

注册与门禁:`ask.enabled` 默认 true(settings-schema.ts:4092-4095);`AskTool.createIf` 以 `session.hasUI` 为准(ask.ts:832-834)。参数:每题 `{id, question, header?, options:[{label, description?, preview?}], multi?, recommended?}`(ask.ts:67-69;类型 types.ts:136-149)。行为要点:
- **首选富对话框** `extensionUi.askDialog(questions, {timeout, signal})`(ask.ts:894-912);宿主未实现 `askDialog` 则逐题降级为 `select` 循环(:989 起)
- 返回 `ExtensionAskDialogResult`:`{kind:"submit", results:[{id, selectedOptions, customInput?, note?, timedOut?}]}` 或 `{kind:"chat"}`(types.ts:151-174)
- **"Chat about this"**(`ASK_CHAT_OPTION`,extension-ui-controller.ts:36):用户拒绝答题改为对话 —— 工具返回合成文本 "User chose to chat about this instead of answering…" + `details.chatRedirect`(ask.ts:918-929);树重答场景下禁用并提示(selector-controller.ts:1407-1418)。UI 还会加 "Other (type your own)" 与 "Next →"(:35-37)
- **超时**:`ask.timeout`(秒,默认 0=禁用,settings-schema.ts:1943-1946);plan mode 下强制禁用(ask.ts:870-875);超时自动选 recommended(否则第一项)并在答案后缀 "(auto-selected after timeout)"、transcript 标注 "auto-selected after timeout — not a user choice"(ask.ts:176-182、:693-703、:713-719、:1442-1456);UI 未回调 `onTimeout` 时按 `TIMEOUT_DETECTION_TOLERANCE_MS=1s` 窗口推断(:152-156、:527-535)
- **通知**:`ask.notify` 默认 on,等待输入时发终端通知(:836-841;schema :1961-1964)
- **阻塞语义**:ask 是工具调用,弹窗未答 = 该 turn 挂起(与审批门同一机制);用户取消(Esc/abort)→ `ToolAbortError`(:914-917、:982-984)

`ExtensionUIContext` 完整面(types.ts:254-369):本域桥接 `select/confirm/input/askDialog/notify` 五个方法;`custom()/setWidget/setFooter/…` 为终端专属,web 明确不支持(§5.1)。

### 3.4 SDK 宿主接入点(引擎改造的 API 依据)

- `createAgentSession({…, hasUI?: boolean})`(sdk.ts:562-563)—— **创建时**决定 ask 等交互工具注册(经 `toolSession.hasUI`,:1670;`AskTool.createIf` 读此值,ask.ts:832-834)、LSP 预热(:3730-3733)、MCP 状态事件(:1838-1841)。**创建期一次性,不可事后补注册**(已注册/未注册的工具表在会话生命期内固定)。
- 返回值携带 `setToolUIContext(uiContext, hasUI)`(结果类型 :601-602;导出 :3940-3942;实现 :3167-3169 → `toolContextStore.setUIContext`)—— **运行期**安装工具执行上下文的 UI(`ctx.ui`/`ctx.hasUI`,ask 执行期门禁 ask.ts:856-860);
- `session.extensionRunner.initialize(actions, contextActions, commandActions, uiContext?, mode?)` —— **运行期装配/摘除审批门所读的 runner UI 上下文**:`#uiContext = uiContext ?? noOpUIContext`(runner.ts:698),`runner.hasUI()` 即 `#uiContext !== noOpUIContext`(runner.ts:610 构造初值 noOp、:878-880);**重复 initialize 是被注释明示支持的重装配路径**(runner.ts:702-705)。宿主直配先例:TUI `extension-ui-controller.ts:286`、RPC 经 `runtime-init.ts:56-144`(`uiContext` 为第 4 实参,:142-143)、ACP `acp-agent.ts:2345`、任务执行器 `executor.ts:3234-3283`(**无 uiContext 实参 = 子代理 runner 恒 hasUI:false 的现状依据**)。`ExtensionMode ∈ "tui"|"rpc"|"json"|"print"`(types.ts:441)。
- `createAgentSession` 已支持 `options.settings`/`settingsManager` 注入(sdk.ts:554-560,消费点 :1273-1275)—— per-session 隔离 Settings 的注入口**已存在**(OQ-3 相关;全局/目录层的权威模型归第 06 章裁决)。
- `session.setUsageFallbackConfirmer` 可选挂钩(extension-ui-controller.ts:132-133 先例)—— usage-reserve 确认也走 UI confirm

---

## 4. 差距清单 [REVISED v3:R10 原子化 + R12 排程 + R11 C12 + R2 新增 C13]

| 编号 | 差距 | 分类 | 优先级 | 风险 | 对应设计 |
|---|---|---|---|---|---|
| GAP-C1 | OpenCode permission 协议全链(V1+V2:端点桩、wire 类型、reducer、卡片/toast/tray/auto-accept/AgentPermissionsEditor)删除 | 删 | **P3**(R12:P1 只切消费者,删除在观察期后) | 中:牵连文件多(§2.3),i18n 10 语言;eviction 守卫 P1 换轨 | §5.8 |
| GAP-C2 | question 协议全链 + 发送路径"自动拒绝并排队"行为删除 | 删 | **P3**(同上;发送行为变更本身 P1 生效,§5.8 P1-3) | 中:行为变更需公告(§6) | §5.8 |
| GAP-C3 | omp-host 会话无 UI(`engine.js:454-473` 无 `hasUI`)→ 非 yolo 设置下工具硬错误(现状缺陷,R10 定级 P0) | 改 | **P0(原子)** | 低:SDK 原生支持;风险全在"半开"中间态,由原子单元消除 | §5.0/§5.1 |
| GAP-C4 | 审批弹窗桥:`uiContext.select` → web Allow/Deny 弹窗 → resolve(含 registry + respond 端点) | 建 | **P0(原子)** | 中:需与 tool part pending 态、通知系统集成 | §5.2/§5.3 |
| GAP-C5 | ask 对话框桥:`askDialog` web modal(options/preview/recommended/multi/timeout/Other/Chat about this;server 权威超时) | 建 | **P0(原子)** | 中:超时与页面关闭语义 | §5.4 |
| GAP-C6 | permissionAutoAccept 体系(store + `/api/permission-auto-accept` 路由 + VSCode 策略 + 草稿/定时任务输入)删除 | 删 | **P3**(R12;其无人值守替代语义 = P0 即生效的 fail-closed,§5.3.3) | 低:TUI 无此概念,master 裁决直接删 | §5.8 |
| GAP-C7 | 审批/ask 设置键在设置面的呈现与"始终允许"高级操作(事务)反写 `tools.approval` | 建 | P1 | 低(实现归 Ch06) | §5.5/§5.3.2 |
| GAP-C8 | 对话框背压与权威快照:respond(幂等/作用域)/abort/timeout/**snapshot 全量对账**(环形缓冲可能挤出) | 建 | **P0(原子)** | 高:做错即"用户永远无法满足 agent"(eviction.ts:4-8 的老教训) | §5.6 |
| GAP-C9 | 审批/ask 等待态的通知与 tray(后台会话可见性) | 建 | **P1**(R12:P1 即消费者切换;原 P2 提前) | 低:复用第 08 章通知系统 | §5.3.4 |
| GAP-C10 | eviction 守卫与子代理阻塞可见性从 permission/question 切到 omp 对话框 | 改 | **P1**(R12;原 P2 提前——漏改会驱逐挂起会话) | 中 | §5.6.4 |
| GAP-C11 | wire gen 死类型的消费引用清除时机(permission/question/V2) | 删 | P3 | 低(vendored,不可手改;**冻结裁决:仅删消费引用,gen 文件不动**,随上游刷新自然消失,§5.8 P3-14) | §5.8 P3-14 |
| GAP-C12 | pending 对话框生命周期硬化:shutdown/dispose/sweeper 原子结算 aborted + transcript 诊断;重启不伪恢复;ID 不可猜;respond 作用域绑定 registry(R11) | 建 | **P0(原子)** | 高:做错即悬死 turn 或跨目录越权应答 | §5.6.5 |
| GAP-C13 | 会话 UI presence 模型:capability 直连 `hasUI` 会使无人值守/旧 UI/断线会话在 `uiContext.select()` 上永久悬挂(审批无默认超时,wrapper.ts:324-341)→ 需 **per-session UI attachment 租约**(心跳/过期/引用计数)+ 对话框**双保护 TTL**(presented-ack 锚点 + 租约丢失 orphan 窗口)(R2-H1/H7/M2) | 建 | **P0(原子)** | 高:做错即"不可见 pending Promise"兼容性倒退或误放行 | §5.0/§5.1 D-C1b/§5.4.3/§5.6.1 |

> **原子交付单元(master D6 R10/R11 + v3 R2-H1/H7)**:GAP-C3+C4+C5+C8+C12+C13 是**单一 P0 能力** —— registry、UIContext 桥、**UI 租约表与 attach/release 端点**、respond、**presented-ack**、abort、**双保护 TTL**、快照、生命周期结算**全部落地并通过 §7 验证(含 R11 四组测试 + v3 租约/TTL 组)后**,才允许 capability `dialogs.v1` 翻 true 并开放租约 attach 端点(§5.0)。任一缺失时 capability 保持 false、attach 恒 501、租约表恒空,会话维持现状 fail-closed(§2.1)—— **不存在"开了 hasUI 但没有应答面"的中间态,也不存在"capability 开了但无人可应答"的悬挂态**(R10 原文:那会把明确失败变成悬挂;R2-H1:capability ≠ UI presence)。消费者切换类(C9/C10)与删除类(C1/C2/C6/C11)按 R12 排程,不与原子单元捆绑。

---

## 5. 设计方案

### 5.0 原子交付单元与启用开关 [REVISED v3:R10/R2 + R2-H1/H7 —— capability 与 UI presence 解耦]

**(单一能力)** 审批 + ask 桥是**一个** P0 能力,不是三个阶段(v1 把 C3 列 P0、C4/C5 列 P1 的拆法作废 —— 只开 `hasUI` 而没有应答面,等于把 fail-closed 硬错误变成无限悬挂)。构成清单(**九项全部完成才翻开关**;v3 新增第 3/6/9 项):

1. **registry**:`PendingDialogRegistry`(`dialogs.js`)—— 注册/查询/respond/**presented-ack**/abort/**双保护 TTL**/snapshot/生命周期结算;ID 不可猜(§5.2);
2. **桥**:`WebUIContext`(`web-ui-context.js`)—— 五方法 + editor + no-op 集(§5.1);
3. **UI 租约**:`UiLeaseTable` + `POST /api/omp/dialogs/lease(/release)` 端点(GAP-C13,§5.1 D-C1b)—— 会话级 `hasUI` 的唯一权威;
4. **respond**:`POST /api/omp/dialogs/:dialogId/respond`,作用域绑定 registry(§5.2);
5. **abort**:用户 Stop → 会话级批量 settle(§5.7);宿主 shutdown/dispose/sweeper → 全量 settle aborted + transcript 诊断(§5.6.5);
6. **双保护 TTL**:presented-ack 计时锚点(§5.4.3)+ 租约丢失 orphan 窗口(§5.6.1)—— **无任何对话框可无限悬挂**;
7. **snapshot**:`GET /api/omp/dialogs` 权威快照 + 05 章通道事件 + bootstrap 对账(§5.6.2);
8. **生命周期**:重启不伪恢复(§5.6.5b);
9. **engine 装配**:materialize 读租约表定创建期 `hasUI` + 租约翻转时 runner 重装配(§5.1 触点 0-2)。

**(唯一启用开关 = capability `dialogs.v1`;v3 核心修正(R2-H1/H7):capability 只声明服务端支持,绝不直接驱动任何会话的 `hasUI`)**:

- omp-host 在 `GET /api/omp/capabilities`(05 章端点组)上报 `dialogs.v1: true|false`,含义 = **"桥/registry/租约/TTL/端点组已在服务端就绪"**。它仅有的产品效果:① UI 是否展示对话框面(R2 协商,不变;UI 不得以本地 feature flag 推断);② 租约 attach 端点是否可用(`false` → attach 恒 501,租约表恒空)。~~v2"engine `#materialize` 读自身 capability 状态决定 `hasUI`"~~ **作废**:评审 R2-H1 证实 `hasUI:true` 时 wrapper 会无限等待 `uiContext.select()` 且审批无默认超时(wrapper.ts:324-341),capability 直连会把 scheduled task、旧 UI、断线浏览器全部变成永久悬挂;`!hasUI` 才抛错(wrapper.ts:307-322)。
- **`hasUI` 的唯一权威 = per-session UI attachment 租约**(§5.1 D-C1b):会话仅在"已认证且 dialogs-capable 的客户端持活跃租约"期间可交互。无人值守/定时任务会话、旧 UI(不认识 attach 端点,永不持租约)、无人连接的目录 → `hasUI:false` → 审批走 wrapper 既有 fail-closed 抛错、ask 不注册(ask.ts:832-834)—— **fail-closed 精确保留**。
- **翻转条件**:九项全部落地 + §7 测试组(含 R11 四组 + v3 租约/TTL 组)通过 → 默认 `dialogs.v1: true`。此前或回滚时为 `false`:attach 恒 501 → 存量租约到期排空(默认 ≤30s)→ 引擎摘除 uiContext → 精确回到现状 fail-closed 行为(§2.1)。
- **灰度三矩阵修正(R2-H7)**:新 UI + 旧 engine(capability 缺失 → UI 隐藏对话框面,且不 acquire 租约);**旧 UI + 新 engine:旧 UI 永不持租约 → `hasUI:false` → 需审批工具立即 fail-closed 抛错(与升级前完全同文案),不是"不可见的 pending Promise"** —— ~~v2"旧 UI 不订阅 `/api/omp/events`,对话框挂起至超时"~~ 作废,该超时来源并不存在(这正是 R2-H7 指出的兼容性倒退);relay 旧 bundle 同旧 UI。三矩阵在 §6 复述。

**(无人值守任务 fail-closed,R10;租约语义下自然成立)**:scheduled task 会话从不 acquire UI 租约(任务运行器不是 dialogs 客户端)→ `hasUI` 恒 false → prompt 档工具走 wrapper 既有 fail-closed 错误(§2.1 文案),任务步失败且原因可在任务诊断与 transcript 中看到;**不**静默改 `tools.approvalMode`、**不**批量写 `tools.approval`(§5.3.3、OQ-3)。

### 5.1 总体架构:WebUIContext + per-session UI 租约 [REVISED v3:R2-H1/H7 —— 租约驱动 hasUI]

**核心决策 D-C1(桥的形态,不变)**:不给 omp-host 写"第二个审批实现",而是提供 `ExtensionUIContext` 的 web 实现 —— 审批解析(tier/模式/覆盖/patterns)、fail-closed、事件序列全部复用 SDK wrapper(§3.2),桥只负责"把 `select()` 的 Promise 挂起,直到浏览器 POST 应答"。理由:审批正确性以 `resolveApproval` 为唯一规格(master D5:行为对齐以 TUI 源码为规格说明),自建实现必然漂移;RPC 模式(`rpc-mode.ts:731-950`)已证明该接口可被任意宿主协议承载。

**核心决策 D-C1b(UI presence = per-session attachment 租约;v3 新增,GAP-C13,R2-H1/H7)**:

SDK 中"有 UI"是三个**互相独立**的机制(§3.4),租约模型对三者的映射:

| SDK 机制(证据) | 租约映射 |
|---|---|
| 创建期 `options.hasUI`:ask 工具注册(sdk.ts:562-563 → `toolSession.hasUI` :1670;ask.ts:832-834)、LSP 预热(:3730-3733)、MCP 状态事件(:1833-1841) | `#materialize` 调 `createAgentSession` 时读租约表:`hasUI: leaseTable.has(directoryKey, sessionId)`(engine.js:454-473) |
| `runner.hasUI()`(审批门,wrapper.ts:307;runner.ts:610 构造 noOp、:698、:878-880) | 租约 **0→n / n→0 翻转**时 engine 调 `session.extensionRunner.initialize(actions, ctxActions, cmdActions, uiContext \| undefined, "json")` 重装配(重复 initialize 为支持路径,runner.ts:702-705;宿主直配先例 runtime-init.ts:56-144、acp-agent.ts:2345) |
| `toolContextStore`(`ctx.ui`/`ctx.hasUI`,sdk.ts:3167-3169;ask 执行期门禁 ask.ts:856-860) | 同一翻转点上调 `result.setToolUIContext(webUiContext, leaseActive)` |

**租约定义**:

- **持有者**:已认证(Basic auth,§5.2)且 dialogs-capable 的客户端;`clientId` = UI 页面实例生成的 UUID(多标签页各自成 holder)。**旧 UI 不认识 attach 端点 → 永不持租约 → 永远 fail-closed**(R2-H7 的矩阵保障即在此,不靠事件订阅与否)。
- **端点组**(omp-host,R4 归属同 §5.2):`POST /api/omp/dialogs/lease` body `{directory, sessionId, clientId}` —— **acquire-or-renew**(幂等:重复调用续期并返回同一租约),响应 `{leaseId, expiresAt, heartbeatIntervalMs}`;`POST /api/omp/dialogs/lease/release` body 同上 —— 显式释放(页面 unload / 离开会话视图时 `fetch(…, {keepalive:true})`)。capability `dialogs.v1=false` → 两者恒 501。
- **心跳与过期**:默认 heartbeat 10s、租约 TTL 30s(3 次未续期即过期),常量归 omp-host 启动配置(默认值内建,产品级可配置性记 OQ-11)。**SSE 连接存活不算 presence**(代理保活不可信),只有显式心跳续期。
- **状态机**:`none ⇄ active(holder 集合非空)`;`hasUI = holder 数 ≥ 1`(引用计数)。holder 过期/释放即时移除;全员清空的瞬间触发 detach 翻转。
- **翻转动作(active→none)**:① runner 摘除 uiContext(后续审批门调用立即 fail-closed);② `setToolUIContext(webUiContext, false)`;③ 该会话全部 pending 对话框进入 **orphan 窗口**(§5.6.1,默认 120s,窗口内重连可救)。**(none→active)**:反向装配;orphan 计时取消、pending 恢复等待。
- **respond / presented-ack 不要求持租约**(校验仍是认证 + `(directory, dialogId)` 作用域,§5.2):刚重连、租约握手未完成的客户端也必须能应答;对话框的存在性本身已由"创建时必须有人持租约"保证(无人持租约 ⇒ `hasUI:false` ⇒ 根本不会产生对话框)。

```
 dialogs-capable UI(持租约:心跳 10s/TTL 30s)
    │ POST /lease(renew)/ POST .../presented / POST .../respond / GET /api/omp/dialogs
    ▼
 omp-host UiLeaseTable [新]                    ← hasUI 唯一权威(GAP-C13)
    │ 租约翻转 → engine 重装配 runner + toolContextStore(§5.1 D-C1b 表)
 SDK wrapper (approval gate / AskTool)          ← runner.hasUI() 决定交互/fail-closed(wrapper.ts:307-341)
    │ uiContext.select / confirm / input / askDialog / notify
    ▼
 omp-host WebUIContext            [新] packages/web/server/lib/omp-host/web-ui-context.js
    │ 注册 PendingDialog {id, kind, payload, resolver, directory, sessionId}   [新] .../dialogs.js
    ├─→ RuntimeAPIs:GET /api/omp/dialogs(权威快照)/ POST .../presented(计时锚点,§5.4.3)/ POST .../respond
    └─→ 05 章 OmpEventBus → /api/omp/events:omp.dialog.requested / omp.dialog.settled(R1 唯一通道)
    ▼
 UI useOmpDialogStore + <ApprovalDialog/> / <AskDialogModal/>(租约 acquire → 快照对账)
```

**engine.js 触点(v3 全量改动点;租约/装配均受 §5.0 原子单元门控)**:
0. **新增 engine 级 runtime 装配助手**(omp-host 现状完全不装配 runner —— engine.js 全文无 extensionRunner/initialize/setToolUIContext):镜像 `runtime-init.ts:41-148` 的 action/contextAction/commandAction 三组闭包,首次 materialize 后发射一次 `session_start`,后续租约翻转仅重调 `initialize` 换 uiContext 实参(mode 取 `"json"`,types.ts:441);
1. `#materialize` 的 `createAgentSession` 增加 `hasUI: leaseTable.has(directoryKey, sessionId)`(engine.js:454-473)—— 创建期注册 ask 工具及其副作用(LSP 启动发现 sdk.ts:3730-3733、MCP 连接状态事件 :1838-1841)仅在"用户已在场"的会话发生;~~v2 `hasUI: <dialogs.v1 capability>`~~ 作废(§5.0)。capability 为 false 时租约表恒空,效果等同不传(现状语义);
2. **租约翻转处理**:attach 时对已物化会话执行 D-C1b 表的三处装配;detach 时反向摘除 + pending 进 orphan 窗口(§5.6.1)。WebUIContext 实例按 `(directory, sessionId)` 维度由 engine 持有,会话 `#materialize`/淘汰时同生命周期(淘汰出口必须先结算 pending,§5.6.5);
3. 可选:`session.setUsageFallbackConfirmer` 也映射到 `confirm()` 对话框(extension-ui-controller.ts:132-133 先例),避免 `usageAwareFallback: confirm` 用户在 web 下卡死;
4. `WebUIContext` 覆盖面:`select/confirm/input/askDialog/notify`(+`timeoutStartsOnPresentation: true` —— v3 起由 presented-ack 协议真实兑现,§5.4.3);`custom()/setWidget/setFooter/setHeader/onTerminalInput/addAutocompleteProvider/setEditorComponent` 显式 no-op(终端专属,与 RPC 降级同构 rpc-mode.ts:824-925);`editor()` 用 web 多行输入对话框实现(ask 的 note 编辑会用到);`theme` 相关返回常量空实现。**不实现 `registerMessageRenderer` 类扩展 UI** —— 扩展生态的 web 化不在本域。

**已知局限(记录在案,不阻塞)**:
- 创建期无租约的会话(如先被定时任务物化)后来被用户打开并获得租约:审批门随翻转变为可交互,但 ask 工具已在创建期被剔除(ask.ts:832-834 注册期一次性),该会话内 ask 不可用;引擎不为补 ask 而重建会话(重建会丢运行态,代价大于收益)。上游若提供运行期工具注册可解,记 §9 上游依赖。
- 子代理会话(task executor)的 runner 不配 uiContext(executor.ts:3234-3283 无 uiContext 实参)→ 子代理 prompt 档工具 fail-closed,**与 TUI 同构**(同一执行器代码),桥不额外改造;上游若传播父 uiContext,子代理对话框自然经同一 WebUIContext 进 registry,无需本域变更。

### 5.2 端点与事件形状(D1:全部走 OpenChamber 自有面)[REVISED v3:R1/R3/R4 + R2-H1/M2 端点新增]

**进程归属与认证(R4)**:dialog 端点与 registry **只注册在 omp-host** —— 经 omp-host 既有 `route()` 装配(endpoints.js 同层),Basic auth 由 host.js:68-70 统一执行;web server **零新增路由**,既有 `registerOpenCodeProxy` 泛 `/api/*` 透传(opencode/DOCUMENTATION.md:388-397)原样覆盖。~~v1"注册方式对齐 permission-auto-accept/runtime.js 的自有路由先例"~~ 表述作废:那是 web server 进程内路由(`core-routes.js:1067` 闸门),R4 裁决一切触碰 AgentSession/registry 的 omp-parity 路由不得落在 web server。

**认证头转换表要求(R4)**:直连(web/局域网 UI 密码)、桌面(Electron)、VS Code 宿主、relay 四种接入形态的凭据,必须在 web server 代理层统一转换为 omp-host 的 Basic 头(现状锚点:`getOpenCodeAuthHeaders()`,auth-state-runtime.js:47-51),且 **`/api/omp/events` SSE 与 `Last-Event-ID` 续订必须在四形态下穿透验证**(长连接 + 重放头不得被剥离;omp-host 侧已读该头,endpoints.js:556)。转换表本体归总纲 R4/05 章通道节(落位见 OQ-10);本章验收含四形态 × (GET 快照 / POST lease / POST respond / SSE 重连)矩阵(§7 E2E-8)。租约心跳走同一代理链 —— 转换表若剥离授权头,租约会静默失败并被误判为断线,故纳入验收。

**RuntimeAPIs 端点组**(集合复数,R3;禁止组件自行拼 URL,统一经 RuntimeAPIs):

```
POST /api/omp/dialogs/lease               → 200 {leaseId, expiresAt, heartbeatIntervalMs} | 501(capability off)   // UI 租约 acquire-or-renew(§5.1 D-C1b)
     body: { directory, sessionId, clientId }
POST /api/omp/dialogs/lease/release       → 200 | 501                                                    // 显式释放(页面 unload,fetch keepalive)
GET  /api/omp/dialogs?directory=<dir>     → { dialogs: OmpDialog[] }     // 权威快照(重连对账)
POST /api/omp/dialogs/:dialogId/presented → 200 | 404 | 409 | 403        // 呈现确认 = 计时锚点(§5.4.3)
     body: { directory }
POST /api/omp/dialogs/:dialogId/respond   → 200 {ok:true} | 404 | 409 | 403    // 应答/取消
     body: { directory, result: RespondResult }
```

**respond / presented 作用域绑定(R11,两者同规)**:registry 登记时绑定 `(directory, sessionId, dialogId)` 三元组;校验**以 registry 绑定为权威** —— `dialogId` 未知/已淘汰 → 404;已 settle → 409(幂等丢弃,UI 据此丢弃陈旧卡);body `directory` 与绑定不符 → 403(**客户端提交的 directory 仅用于校验,不用于路由**;ID 不可猜使存在性探测无收益,见下)。**两个动作均不要求持 UI 租约**(§5.1 D-C1b:对话框存在性已由创建时租约门槛保证;重连中客户端不得因租约握手竞态被拒)。lease 端点按 `(directory, sessionId, clientId)` 幂等,校验目录一致性后写租约表。

**事件(R1:走 05 章唯一通道;v1 的 `openchamber:omp-dialog-requested/-settled` 命名与全局广播器挂靠方案作废,沿革见 §2.4)**:

事件经 05 章 `OmpEventBus → /api/omp/events` SSE 下发;envelope、事件 ID、durable/volatile、directory 作用域、`Last-Event-ID` 重放、schema 版本**以 05 章为唯一权威**,本章不自建通道、不定义 envelope。本章在 05 章事件注册表登记两行:

| SDK source | 公开名 | payload | producer | durable | 作用域 | 快照端点 | UI reducer |
|---|---|---|---|---|---|---|---|
| WebUIContext 挂起:审批门(wrapper.ts:279-303)、askDialog(ask.ts:894-912)、零散 select/confirm/input/editor | `omp.dialog.requested` | `{directory, dialog: OmpDialog}`(全量描述,UI 无需二次取数) | omp-host `PendingDialogRegistry` | durable(长驻状态,重放环覆盖短暂断连) | directory | `GET /api/omp/dialogs` | 按 id upsert |
| registry settle(respond/abort/timeout/dispose) | `omp.dialog.settled` | `{directory, dialogId, sessionId, outcome: "responded"\|"cancelled"\|"timeout"\|"aborted"}` | 同上 | durable(双端竞答兜底) | directory | 同上(权威移除) | 按 id remove |

`OmpDialog` / `RespondResult`(payload 与 v1/v2 一致;v3 修订:id 生成规则 + 新增 `presentedAt`):

```ts
type OmpDialog = {
  id: string;                    // "dlg_" + crypto.randomBytes(16).toString("base64url") —— 不可猜(R11,§5.6.5c)
  sessionId: string;             // wire sessionID
  createdAt: number;
  presentedAt?: number;          // presented-ack 时刻(§5.4.3);未 ack(含队列等待中)省略 —— UI 倒计时与 T_present 状态据此渲染
  kind: "approval" | "ask" | "select" | "confirm" | "input" | "editor";
  // kind=approval(由 WebUIContext.select 的 Approve/Deny 双选项调用了望,见 §5.3.1):
  approval?: {
    prompt: string;              // formatApprovalPrompt 原文 = TUI 弹窗正文(approval.ts:258-279)—— 奇偶校验基准
    approvalMode: "always-ask" | "write" | "yolo";
    // 尽力关联(engine 用 pending tool_execution_start 记录,无关联时省略):
    toolName?: string; toolCallId?: string; tier?: "read" | "write" | "exec";
    reason?: string;
  };
  // kind=ask:
  ask?: {
    questions: Array<{ id: string; question: string; header?: string;
      options: Array<{ label: string; description?: string; preview?: string }>;
      multi?: boolean; recommended?: number }>;   // 原样透传 ExtensionAskDialogQuestion(types.ts:142-149)
    timeoutMs: number;           // 0 = 无限等
  };
  // kind=select/confirm/input(扩展或内核零散调用):
  select?: { title: string; options: string[]; helpText?: string };
  confirm?: { title: string; message: string };
  input?: { title: string; placeholder?: string };
};

type RespondResult =
  | { kind: "select"; value?: string }         // approval: "Approve" | "Deny"(或 undefined=取消)
  | { kind: "confirm"; value: boolean }
  | { kind: "input" | "editor"; value?: string }
  | { kind: "ask"; results: Array<{ id: string; selectedOptions: string[];
      customInput?: string; note?: string; timedOut?: boolean }> }
  | { kind: "chat" }                           // 仅 ask:"Chat about this"
  | { kind: "cancel" };                        // 通用取消(Esc)
```

**决策 D-C2(为何不进 wire)[REVISED v2]**:R1 已裁决 omp 原生事件的唯一出口是 05 章 `OmpEventBus → /api/omp/events`;wire `/event` 是 OpenCode 契约面,D1 明令不扩张,dialog 亦不得例外。v1 备选"独立 `/api/omp/dialogs/stream` SSE"仍否:05 章通道已具备重连/心跳/重放,第四条流重复造基建且与 05 的 resync 语义割裂。

**决策 D-C3(权威与重放分离)**:`PendingDialogRegistry`(omp-host)是对话框的**唯一权威**;事件只做通知。UI 启动/重连/收到 05 章 `omp.resync` 时 `GET /api/omp/dialogs` 对账(sync-state-invariants:权威状态 + 重连对账);dialogs 位于 master D2 bootstrap 顺序 `capabilities → session snapshot → modes/model → dialogs → agents/jobs/queue → transcript 增量`。理由见 §5.6.2 —— 重放环不保证长驻状态可重放。

### 5.3 审批弹窗

#### 5.3.1 语义与呈现

- **按钮集 = `Approve` / `Deny` 两个,别无其他**(wrapper.ts:332 的 `select(safetyPrompt, ["Approve","Deny"])` 是硬编码契约;`RespondResult {kind:"select"}` 的取值域即此二值 + undefined=取消)。web 弹窗正文直接渲染 `approval.prompt`(`formatApprovalPrompt` 产物,与 TUI 完全同文)—— 这是与 TUI 对照的奇偶校验锚点。"始终允许"是弹窗外的**高级操作**,不属于按钮契约(§5.3.2,v3 收敛)。
- 尽力富化:engine 在 `#handleEngineEvent` 已见 `tool_execution_start(toolName, toolCallId, args)`(wrapper 在其之后才等待,event-controller.ts:1428-1430 注释),WebUIContext 创建 approval 对话框时按"同 session 最近一个未决 tool_execution_start"关联出 `toolName/toolCallId/tier`,供弹窗头部徽标与流内 tool part 联动定位。并行工具下不保证一一对应 —— 关联失败就纯文本呈现,不影响正确性(approve/deny 永远作用于"实际执行参数",由 wrapper 保证 wrapper.ts:241-253)。
- 弹窗打开期间,对应 tool part 处于 pending(既有投影已如此);会话状态徽标显示"等待确认"(对照 TUI 的 attention 标题,event-controller.ts:1426-1434)。

#### 5.3.2 "Always allow" = 弹窗外高级操作(事务)[REVISED v3:R2-M1 —— 与 §5.3.1 按钮契约收敛]

- **P1 不提供 per-session "always allow" 内存态**(TUI 无此语义;master 裁决 TUI 为准)。ACP 的 `allow_always`(acp-permission-gate.ts:16-21)是 ACP 协议产物,不采纳。
- **按钮契约只含 Approve/Deny(§5.3.1),别无其他** —— ~~v2"弹窗第三个次级按钮"~~ 作废:它让 §5.3.1"别无其他"与 §5.3.2 自相矛盾(评审 R2-M1),且第三按钮提高误触持久化配置的概率。
- **"始终允许"是独立高级操作**:弹窗溢出菜单("⋮ → 始终允许此工具(写入设置)并批准"),需二次确认;`RespondResult` 不新增变体 —— 该操作最终仍回投一个普通 Approve 应答。
- **事务语义(顺序硬约束)**:① 用户确认高级操作 → ② `POST /api/omp/settings` 写 `tools.approval.<tool|policyKey> = "allow"`(经 Ch06 设置通道;`policyKey` 存在时写 policyKey 键,approval.ts:127)→ ③ **写成功后**才自动 respond Approve → ④ **写失败 → 不批准**:弹窗保持打开、错误 toast 可见,用户仍可手动 Approve/Deny(不允许出现"设置没写成但已放行"的状态)。若步骤 ③ 返回 409(另一端已 settle):设置已落盘不算污染 —— 它与设置页同一语义、可审计、可再改,toast 告知即可。语义基础即 omp 自己的持久覆盖(approval.ts:117-118),无新发明。
- **与 R10 禁改令的边界**:R10 禁止的是**无人值守任务/自动化**改写全局审批设置(§5.3.3);本操作是**用户在弹窗上的显式确认**(每次一次写入,经 Ch06 通道落盘可审计),不在禁止之列。
- Deny 不提供持久化变体(`tools.approval.<tool>=deny` 属设置页操作,不混入弹窗,避免误触不可逆配置)。

#### 5.3.3 per-session approvalMode 开关:**不提供**(决策 D-C4)[REVISED v2:R10 无人值守裁决]

TUI 没有会话级开关 —— `approvalMode` 是全局设置(wrapper.ts:191 直接 `settings.get`,设置单例为进程级,settings.ts:2420);web 的"切换审批档"= 改全局设置,入口放设置页(§5.5)。OpenChamber 的 `PermissionAutoAcceptButton`、草稿态 `permissionAutoAcceptEnabled`、定时任务 `permissionAutoAccept` 全部删除(GAP-C6,P3;其 UI 输入在 P1 起停止生效)。

**无人值守任务(R10 裁决,替代 v1 OQ-3 的"任务模板写 yolo/allow 快照"方案 —— 作废,那等于替用户全局放行)**:
- scheduled task 会话**不得**为通过审批而改写全局 `tools.approvalMode`/`tools.approval`,也不获得任何会话级豁免;
- capability `dialogs.v1` 上线后,任务会话**从不 acquire UI 租约**(§5.1 D-C1b)→ `hasUI` 恒 false → prompt 档工具走 wrapper 既有 fail-closed 错误(§2.1 文案)→ 任务步失败,失败原因进任务诊断与 transcript(可见、可重试);**无任何对话框被创建、无 pending 可悬挂**;
- 任务编辑器创建时检测全局 `approvalMode ≠ yolo` 即警告"无人值守运行将在需要审批的工具上失败";
- 唯一正道 = 会话级 settings 注入(SDK 注入口已存在:`createAgentSession` 的 `options.settings`/`settingsManager`,sdk.ts:554-560、:1273-1275;OQ-3)—— 但全局/目录层 Settings 权威模型归第 06 章裁决(R2-3),其定稿前不提供任何任务侧审批改写;master §7.10 同记。

#### 5.3.4 后台会话可见性

非当前会话的审批/ask 到达 → 复用第 08 章通知系统:`openchamber:notification` 合成事件(emitter-runtime.js:68-72 通道;**OpenChamber 原创通知面,非 omp 原生事件,不受 R1 约束**)+ Web Notification + tray 菜单项("会话 X 等待批准 → Approve/Deny")。替换 useTraySync 的 permission 菜单(§5.8 P1-1);toast 的"打开会话"动作保留语义,数据源换轨。

### 5.4 ask 对话框

#### 5.4.1 组件设计(`AskDialogModal`)

- 每题一卡:`question`(markdown)、`header` 做分区标题;选项列表渲染 `label + description`,hover/展开 `preview`(monospace 块);`multi` → checkbox,单选 → radio(对照 TUI 的 radio/checkbox 字形 ask.ts:1212-1218);`recommended` 项加 "(Recommended)" 徽标(ask.ts:152、:163-172)
- 常驻动作:**Other (type your own)**(自定义输入 + 可选 note)、**Chat about this**(全局次级按钮)。多题时逐题 "Next →"(extension-ui-controller.ts:35-37 的三常量逐一对应)
- 提交校验沿用工具语义:多选空提交合法("select none");单选空且无 customInput 且非超时 = 取消(ask.ts:951-965)
- 提交 → `RespondResult {kind:"ask", results}`;"Chat about this" → `{kind:"chat"}`;Esc → `{kind:"cancel"}` → 工具 `ToolAbortError`(ask.ts:914-917)
- **流内回显**:ask 工具结果在 transcript 渲染为答案卡(AskToolDetails:`question/options/selectedOptions/customInput/note/timedOut`,ask.ts:966-976;超时答案标 "auto-selected after timeout — not a user choice" 对照 ask.ts:1454-1456)—— ToolPart 新增 `ask` 渲染器,取代被删的 question 工具渲染器(ToolPart.tsx:1409-1447;P0 随原子单元上线,P3 才删旧渲染器)

#### 5.4.2 阻塞与流关系

ask 是 turn 内工具调用:弹窗未答 = agent turn 挂起(会话 busy,sidebar 活动点常亮);composer 此时发消息走既有 steer/queue 语义( omp `prompt()` 已实现 steer/followUp,engine.js:690-695)—— **不再自动拒绝弹窗**(行为变更自 P1 生效,§5.8 P1-3;公告 §6)。用户想改主意:答弹窗或按 Stop(abort → 对话框 `aborted`,见 §5.7)。

#### 5.4.3 计时模型:presented-ack 锚点 + 双保护 TTL(server 权威;原子单元第 6 项,§5.0)[REVISED v3:R2-M2]

**锚点修正(v3)**:~~v2"server 端以创建并入队时刻近似 presentation"~~ 作废 —— 断线、后台标签页或队列中前一个 modal 都会吃掉后一个对话框的超时,`timeoutStartsOnPresentation` 名实不符(评审 R2-M2)。计时锚点改为**客户端 presented-ack**:

1. **presented-ack**:UI 把对话框渲染为**活动 modal**(队列首层,§5.6.3)时 `POST /api/omp/dialogs/:dialogId/presented`(作用域校验同 respond,§5.2)。registry 记 `presentedAt`(快照字段),取消 T_present、启动 T_answer。
2. **T_answer(应答窗口)从 ack 起算**:
   - ask 且 `timeoutMs > 0`:T_answer = timeoutMs。到期由 **server 端(PendingDialogRegistry)计时器**合成 `results[i] = {selectedOptions:[recommended|first], timedOut:true}`(镜像 ask.ts:176-182)自动 submit,发 `omp.dialog.settled(outcome:"timeout")`,浏览器端 modal 收事件即关。**页面关闭不救场** —— 计时器在引擎侧,turn 不会永挂;断线/后台标签页不再偷跑超时(ack 未到不计时)。
   - ask `timeoutMs == 0`(默认)与 approval/select/confirm/input/editor:**租约在场时无限等待**(TUI parity:用户 blocking 是产品语义);**租约丢失时进入 orphan 窗口**(默认 120s,§5.6.1)—— 有界,重连可救。
3. **T_present(未呈现保护 TTL)从注册起算**:默认 300s 内无 presented-ack → settle `timeout`(approval → resolver reject "dialog expired before presentation";ask → abort 路径)。它保护的是"注册了但从未被呈现"(客户端 bug、队列卡死、租约刚丢后无人 ack),**不是产品超时**;队列中排在后面的对话框在获得 ack 前不消耗自己的 T_answer。T_present 必须 ≥ orphan 窗口 + 合理队列等待(多 modal 排队场景)。
4. **`timeoutStartsOnPresentation: true`(WebUIContext 声明,types.ts:256)自此真实成立** —— 不再是近似:server 只认 ack 锚点;UI 展示倒计时环(数据源 = `presentedAt` + `timeoutMs`)。TUI 会在按键交互时重置(onTimeoutReset,ask.ts:503),web P1 不重置(点击即提交,无中间导航态)—— 差异点照旧记 §7 对照表。
5. TTL 常量(present 300s / orphan 120s)归 omp-host 启动配置,默认值内建(产品级可配置性记 OQ-11);ask 的 `timeoutMs` 语义与取值仍由 omp 设置 `ask.timeout` 决定(§5.5),不受本章 TTL 影响。

### 5.5 设置面呈现(交互规格,实现归 Ch06)

设置页 Interaction→Approvals 区(对齐 schema `ui{tab:"interaction",group:"Approvals"}`):
- `tools.approvalMode`:三选一(Always ask / Write / Yolo),文案直取 schema options 描述(settings-schema.ts:3688-3706)—— 全局生效,弹窗即时反映
- `tools.approval`:per-tool allow/prompt/deny 三态表(工具名下拉 = 会话可见工具集),标注"任何模式下生效"
- `bash.patterns`:有序 match/approval 规则编辑器(对齐 schema :3487-3489 描述)
- `ask.enabled/ask.timeout/ask.notify` 三个键随 Ch06 通用 schema 渲染;`ask.timeout` 输入单位秒、0=禁用,帮助文案注明"超时自动选推荐项;plan mode 下无效"(ask.ts:870-875)

### 5.6 背压与生命周期:租约丢失、重连、重放、宿主退出 [REVISED v3:R11 + R2-H1 orphan 窗口]

#### 5.6.1 引擎侧:租约在场永等 / 租约丢失进入 orphan 窗口 [REVISED v3:R2-H1]

- **租约在场(UI 持有心跳,含页面打开但用户离开)**:`timeoutMs == 0` 的对话框无限等待 —— 与 TUI 用户离开终端完全同构,这是产品语义(yolo 默认下弹窗本就稀有);`PendingDialogRegistry` 常驻内存,会话对象不淘汰(挂起 turn 本身保持 agentSession 存活;服务端清扫例外见 §5.6.5a-3);配合通知系统(§5.3.4)保证用户知道有东西在等。
- **租约丢失(最后一个 holder 过期/显式释放,§5.1 D-C1b 翻转)**:runner 已摘除 uiContext,新到达的审批门调用立即 fail-closed;该会话全部 pending 对话框进入 **orphan 窗口(默认 120s,omp-host 配置)**:窗口内任何客户端重新 acquire 租约 → 计时取消、对话框恢复等待(§5.6.2-0);窗口到期 → registry 原子 settle —— approval 的 resolver 以 `Error("dialog orphaned (no UI lease)")` reject(走 wrapper.ts:333-336 catch 分支 → `tool_approval_resolved(false, reason)` → 工具错误,错误文本诚实标注孤儿结算而非伪造用户 Deny);ask 走 abort 路径(`ToolAbortError`,ask.ts:982-984)—— settle 事件 `outcome:"timeout"`。**不存在无界悬挂**(R2-H1 的硬要求);宿主进程退出由 §5.6.5 兜底。

#### 5.6.2 重连重放(四层)[REVISED v3:租约重取前置]

0. **租约重取**:重连/刷新后 UI 在 bootstrap 的 dialogs 步**先 `POST /api/omp/dialogs/lease`**(§5.1 D-C1b;capability 协商通过才发)—— orphan 窗口内的 pending 因此恢复等待;对尚不存在的会话,engine `#materialize` 据此判定创建期 `hasUI:true`。
1. **SSE 增量**:浏览器对 `/api/omp/events` 重连带 `Last-Event-ID`,05 章通道的 durable 重放环覆盖短暂断连(通道语义以 05 章为权威,R1;`omp.dialog.*` 两事件均标 durable,§5.2)。
2. **权威对账**:冷启动 / 05 章 `omp.resync`(事件缺口)/ `ready` → `GET /api/omp/dialogs?directory=` 全量对账 —— 覆盖"重放环被挤出"与"刷新丢 Last-Event-ID"两种失效;dialogs 在 D2 bootstrap 顺序中先于 agents/jobs/queue。对话框是**长驻状态**,不能只当事件流(决策 D-C3 的根因)。恢复的 modal 重新发 presented-ack(§5.4.3)。
3. **应答幂等**:POST respond 对已 settle 的 id 返回 409,UI 据此丢弃本地陈旧卡(settled 事件可能先于用户点击到达,如双端同时操作)。

#### 5.6.3 UI store 规约

`useOmpDialogStore`:状态 = server 快照 + 事件增量;`requested` upsert、`settled` 移除;重连 bootstrap 覆盖本地(权威对账);同 session 多对话框按 `createdAt` 排序渲染为**队列**(栈式 modal + 计数徽标)。**只有队列首层(活动 modal)发 presented-ack(§5.4.3)** —— 被压在后面的对话框保持未 ack(T_answer 未启动、T_present 保护中),成为活动 modal 时才 ack 并启动自身应答窗口;会话切换时弹窗归属其 session(非当前会话 → 通知面,§5.3.4)。订阅与租约 acquire 均由 capability `dialogs.v1` 决定(R2),不得本地 flag 推断。

#### 5.6.4 eviction 换轨(GAP-C10,P1)

`eviction.ts:15-24` 的守卫从 `state.permission/state.question` 改为查询 omp 对话框状态(UI 侧:`useOmpDialogStore` 按 directory 聚合存在挂起对话框 → 该目录的 child store 不可驱逐/不可 dispose,:32、:58)。WorkStatusSubagentsSection 的"子会话权限阻塞"提示改读同源数据(子会话挂起对话框)。

#### 5.6.5 宿主生命周期:shutdown / dispose / sweeper / 重启 [NEW v2 · R11]

**(a) 全量原子结算**。omp-host 的每个生命周期出口都必须先 `registry.settleAll('aborted')` 再退场。对每个 pending:
- reject 其 resolver,reason = 出口名(如 `"omp-host shutdown"` / `"session disposed"` / `"idle sweep"`),走与用户 abort 相同的 Promise 拒绝路径(审批门 wrapper.ts:333-336 的 abort 分支;ask 的 `ToolAbortError` ask.ts:982-984)→ 工具以错误完成,**错误文本含结算原因 → transcript 诊断**(aborted stopReason 的投影已有,projection.js:257-263);
- 会话退出前先 `agentSession.abort({reason})` 落定 turn(engine.abort 同型,engine.js:706-712),确保 SDK 把挂起 turn 写完(工具错误结果 + aborted 完成),再 dispose —— **不发明额外持久层,诊断即 transcript 内的错误文本**;
- 尽力发出最后一批 `omp.dialog.settled {outcome:"aborted"}`(进程退出场景以重启后快照对账兜底,见 b)。

出口清单(现状证据 §2.5):
1. `engine.shutdown()`(SIGTERM/SIGINT 链:host.js:120-126 → host.js:106-110 → engine.js:815-821)—— `#disposeSession` 前 settleAll;
2. `POST /global/dispose`、`/instance/dispose` —— **必须改为经 `engine.shutdown()` 再 exit**(现状 endpoints.js:158-165 直接 `process.exit(0)`,是桥落地后的悬死出口,属本 GAP 修复项);
3. idle sweeper —— **跳过有 pending 对话框的会话**(engine.js:126-131 现状无守卫;服务端镜像 GAP-C10 的 UI 侧守卫,防"UI 不驱逐但服务端清扫"的错位);
4. 会话显式 delete / agent 切换重建路径(engine.js:318-320、:643-646)—— dispose 前同样结算该会话 pending。

**(b) 重启不伪恢复**。registry **与 UI 租约表**都是 omp-host 进程内存态,进程退出即消失(与 overrides 的进程生命周期语义一致,omp-host/DOCUMENTATION.md:50-52);重启后 `GET /api/omp/dialogs` 返回空、租约表为空(客户端重连时重新 acquire,§5.6.2-0)—— UI 重连权威对账后本地不得残留旧卡,**禁止**从陈旧事件重放或客户端持久化复活对话框。引擎侧 turn 已在 (a) 落定为 aborted/错误;**不存在**"重启后 resolve 旧 Promise"的路径(R11:重启不伪恢复)。

**(c) ID 不可猜**。`dlg_` + `crypto.randomBytes(16).toString('base64url')`(仓库先例:loopback auth token `agent-tool/runtime.js:254`、relay 会话 `tunnel-auth.js:513`)。v1 提案"engine 侧单调 id(对齐 RPC Snowflake)"作废 —— 单调/时间序 id 可猜,不适用于跨进程暴露的 respond 面(rpc-mode.ts:872 是 TUI 进程内用法,不作先例)。

**(d) respond 作用域**。见 §5.2:registry 绑定 `(directory, sessionId, dialogId)` 为权威,客户端 directory 仅校验(403 拒绝跨目录应答),不泄露其他目录对话框内容。

**(e) 测试**。四组:双端竞答 / 错误目录应答 / abort / 热重启(R11,§7 E2E-7)。

### 5.7 并发、中止与超时语义汇总 [REVISED v3:补租约/TTL 行]

| 场景 | 引擎行为(既有,不改) | 桥/UI 行为 |
|---|---|---|
| 并行工具双审批 | wrapper 各自挂起(wrapper 并发模型 shared/exclusive,bash.ts:605-606) | registry 允许多 pending;UI 队列化,首层才 ack(§5.6.3) |
| 用户按 Stop(abort) | signal → `select` reject → `tool_approval_resolved(false,"approval aborted")`(wrapper.ts:333-336);ask → `ToolAbortError`(ask.ts:982-984) | engine 监听会话 abort → 对该 session 全部 pending settle(`aborted`)+ 发 settled 事件 + UI 关弹窗 |
| 弹窗期间关页面/断线 | 引擎无感知,继续等 | 心跳停止 → 租约过期(≤30s)→ pending 进 orphan 窗口(默认 120s)→ 重连可救或 settle `timeout`(§5.6.1);重连 = 租约重取 + 快照对账 + 重新 presented-ack(§5.6.2) |
| **无人持租约的会话**(scheduled task / 旧 UI / capability on 但无客户端) | `runner.hasUI()==false` → wrapper 直接抛错(wrapper.ts:307-322) | **无对话框、无 pending、无悬挂**;旧 UI 与升级前同文案 fail-closed(R2-H7) |
| 租约过期后新工具到达审批门 | 同上(hasUI 已摘除) | 后续工具错误可见;pending 的旧对话框仍走 orphan 窗口 |
| ask 超时(页面开着/关着) | 见 §5.4.3 | server 计时器权威,**从 presented-ack 起算**(R2-M2) |
| **T_present 到期**(注册后从未 ack,默认 300s) | — | settle `timeout`(approval deny-fail / ask abort)(§5.4.3-3) |
| 双端应答竞态 | 先到先得,Promise 只 resolve 一次 | 后到 409;settled 事件广播兜底 |
| Deny | `throw "Tool call denied by user: <tool>"`(wrapper.ts:339-341) | tool part 渲染错误态(既有投影);弹窗关 |
| **宿主 shutdown / dispose**(R11) | 现状无结算路径(§2.5 缺陷) | settleAll aborted + `abort()` 落定 + transcript 诊断(§5.6.5a) |
| **重启后**(R11) | turn 已落定 aborted/错误 | 快照空;UI 对账清卡;不伪恢复(§5.6.5b) |
| **idle sweeper**(R11) | 现状直接 dispose(engine.js:126-131) | 有 pending 的会话跳过清扫(§5.6.5a-3) |

### 5.8 删除计划(三段排程;执行细则供 Ch07 引用)[REVISED v3:R12 + R2-M8]

**顺序原则(R12 裁决,替代 v1 的"桥先行 → 立即逐链摘除"连续清单)**:**P0 原子桥**(纯新增,不动旧链)→ **P1 消费者切换**(通知/pending 状态/tray/eviction/WorkStatus 改读 omp 对话框;旧 permission/question 链保留但不再是权威)→ **观察期**(旧链零有效消费验证 + E2E 回归)→ **P3 删除**(协议桩、UI 链、auto-accept、agent 编辑器、i18n、wire gen 类型)。v1 隐含的"桥落地后即删旧链"作废 —— 先删生产者/设置会让审批、通知、WorkStatus 断供(评审中危原话)。每步可独立回滚(revert 单 commit),步骤间无交叉依赖者可并行。

| 阶段 | # | 动作 | 文件(证据行) | 备注 |
|---|---|---|---|---|
| **P0** | 0 | **原子交付单元上线**(§5.0 九项,含 UiLeaseTable/lease 端点与双保护 TTL)+ capability `dialogs.v1` 翻 true;dispose 路由改造(§5.6.5a-2) | 新增 web-ui-context.js / dialogs.js(含租约表);改 engine.js:454-473 + runner 装配助手(§5.1 触点 0-2);endpoints.js:158-165 | 纯 additive;回滚点:capability 置 false → lease 恒 501、租约排空 → hasUI 回 false |
| **P1** | 1 | 通知/tray 换轨:审批/ask 等待接入通知系统与 omp 对话框 tray 菜单(旧菜单并存) | useTraySync.ts:113-122/274-275/596-600 数据源换轨 | GAP-C9;§5.3.4 |
| P1 | 2 | pending 状态/eviction/WorkStatus 换轨 | eviction.ts:15-24、WorkStatusSubagentsSection.tsx:19-22、目录 store 读取换源(字段暂留,P3 清) | GAP-C10;§5.6.4 |
| P1 | 3 | 发送路径行为切换:发送不再自动拒绝挂起弹窗(自动拒绝代码停用,删除留 P3) | session-actions.ts:1597-1619/1725-1785 | 行为变更公告 §6 |
| **观察期** | — | grep 旧链零有效消费;dialogs 面 E2E 回归;观察窗口与 07 章大扫除同窗 | — | 进入 P3 的门 |
| **P3** | 4 | reducer 摘除 permission/question 五 case | event-reducer.ts:521-576 | 同步删 `PermissionRequest/QuestionRequest` 本地引用(v1 步骤 1) |
| P3 | 5 | sync-context 拦截/toast 摘除 | sync-context.tsx:1527-1592 | v1 步骤 2;通知已换轨(P1-1) |
| P3 | 6 | 阻塞卡与恢复链删除 | PermissionCard.tsx、QuestionCard.tsx、ChatContainer.tsx:373-378/638-641、permission-toast.ts、question-recovery.ts(+各测试) | v1 步骤 3;ToolPart ask 渲染器 P0 已上(§5.4.1),此处再删 question 渲染器 ToolPart.tsx:1409-1447 |
| P3 | 7 | 动作层删除 | session-actions.ts:1548-1661、client.ts:1163-1170/1337-1352/1364-1369;client.permission.test.ts | v1 步骤 4 |
| P3 | 8 | auto-accept 体系删除 | permissionStore.ts(+2 测试)、utils/permissionAutoAccept.ts(+测试)、vscode-permission-auto-accept.ts(+测试)、PermissionAutoAcceptButton.tsx、chat/permissionAutoAccept.ts、ChatInput.tsx:321-323/2436-2462;server permission-auto-accept/(runtime.js:249-262)+ core-routes.js:1067 闸门 | v1 步骤 5;GAP-C6;VSCode 扩展侧策略文件同步清(桌面仓) |
| P3 | 9 | 定时任务输入删除 | ScheduledTaskEditorDialog.tsx:479/531/569/1174-1175/1688-1701 | v1 步骤 7;fail-closed 语义 P0 已生效(§5.3.3),此处只删 UI 输入;诉求转 OQ-3 |
| P3 | 10 | agent 权限编辑器删除 | AgentPermissionsEditor.tsx、agentPermissionModel.ts(+测试)、ModelControls.tsx:1438-1447/2449-2458 | v1 步骤 8;与 Ch02 agent 模型收敛协同 |
| P3 | 11 | 目录 store 字段清理 | directory state `permission`/`question` 字段、bootstrap 重水化、sanitize Session.permission | v1 步骤 10;兼容旧持久化:读取时丢弃 |
| P3 | 12 | omp-host 桩收口 | endpoints.js:334-344 → 501 unsupported(与 share 同风格 :326-331)或直接摘路由 | v1 步骤 11;依 Ch07 口径 |
| P3 | 13 | i18n 清理 | 10 语言 `chat.chatInput.permissionAutoAccept.*`、`sessions.scheduledTasks.editor.permissionAutoAccept.*`、`chat.permissionCard.*`、question 相关键 | v1 步骤 12;locale 维护者注意 |
| P3 | 14 | wire gen 死类型 | types.gen.d.ts permission/question/V2(:1028-1047/:1133-1146/:1237-1260/:2021-2034/:1347-1351 等) | v1 步骤 13;**冻结裁决(00 D1 / 07 §5.0 / R2-M8):vendored gen 文件不动,仅删除本仓对死类型的消费引用** —— P3-4..13 的引用清零即本项完成,类型随未来上游重新生成自然消失。~~"要么整段裁剪+注释,要么留待上游重生成"二选一~~ 作废,不再作为开放问题(GAP-C11) |

---

## 6. 迁移与兼容 [REVISED v3:R2-H7 三矩阵修正 + R2-M8]

- **存量数据**:permissionStore(persist)与 `/api/permission-auto-accept` 的服务端快照随 P3-8 直接废弃,不迁移(对 omp 引擎它们从未生效过 —— 空桩,§2.1);localStorage 旧键留置无害,读取代码删除后自然失效。
- **并发会话(capabilities 三矩阵,R2;v3 修正 R2-H7)**:升级瞬间旧版 UI 与新版 server(capability on)并存 —— **旧 UI 不认识 `/api/omp/dialogs/lease`,永不持租约 → 其会话 `hasUI:false` → 需审批工具立即 fail-closed 抛错(与升级前完全同文案、同可见性),不是"不可见的 pending Promise"**;~~v2"旧 UI 审批到达时挂起至超时/用户经新版应答"~~ 作废(该超时来源不存在,且把明确失败降级为悬挂正是 R2-H7 否决的兼容性倒退)。新 UI + 旧 engine(capability 缺失)→ UI 隐藏对话框面、不发 acquire,回旧交互;relay 旧 bundle 同旧 UI。空桩本就无生产者,旧 UI 只是安静不弹;`omp.dialog.*` 走独立 omp 通道,旧 UI 根本不订阅,无中间态协议。
- **行为变更公告(用户可感)**:① 发送消息不再自动拒绝挂起弹窗(§5.4.2,P1 生效);② composer/定时任务的"自动接受权限"开关消失(P3 删 UI;语义 P0 起由 fail-closed 替代),替代 = 设置 `tools.approvalMode`;③ **持有 UI 租约的**非 yolo 用户从"工具报错 no interactive UI"变为"弹窗"(缺陷修复,§2.1);④ 无人值守任务遇审批将失败并留下诊断,不再有任何自动放行(任务会话无租约 → SDK 抛错,§5.3.3);⑤ 断线超 orphan 窗口(默认 120s)后挂起弹窗按 timeout 结算、工具留诊断文本(§5.6.1)—— TUI 之外新增的有界性,公告中说明。
- **回滚**:桥(纯 additive,capability `dialogs.v1=false` → lease 恒 501 → 存量租约 ≤30s 排空 → 引擎摘除 uiContext → 回 `hasUI:false` 的现状 fail-closed,代码可原地保留)与删除(P3 每步一 commit)分离;回滚删除只需 revert 对应 commit —— 服务端桩在 P3-12 前保持空应答,旧 UI 组件 revert 后即刻恢复工作。v1 的 `OMP_HOST_WEB_DIALOGS` 环境开关方案作废(R2:统一 capabilities,服务端裁决)。
- **阶段性开关**:capability `dialogs.v1` 即阶段性开关(§5.0);不再有独立灰度变量。租约/心跳/TTL 常量是 omp-host 启动配置,非用户可见开关(产品级可配置性见 OQ-11)。

---

## 7. 验证方案 [REVISED v3:R11 四组 + capability 门控 + 认证矩阵 + 租约/TTL 组(R2-H1/H7/M1/M2)]

**单元/集成(omp-host,bun:test;server JS `node --check`)**:
1. `dialogs.js` PendingDialogRegistry:创建/查询/respond(200/404/409/**403 目录不符**)/presented-ack(**T_present 取消、T_answer 起算、重复 ack 幂等**)/超时自动 submit(recommended 缺省回退首项,对齐 ask.ts:176-182,**从 presentedAt 而非 createdAt 起算**)/abort 批量 settle/orphan 窗口结算(approval → reject 文本含 "orphaned";ask → abort 路径)/**`settleAll` 原子性(任一 pending 不漏结算、Promise 均落定、settled 事件齐发)**/ID 不可猜(连续生成量级采样无碰撞、与计数器/时间无关)。
2. `web-ui-context.js`:五方法 → 对话框形状映射(approval 的 prompt 原文透传;ask 的 questions 透传与 `timeoutMs` 换算);`timeoutStartsOnPresentation` 声明(v3:由 ack 协议真实兑现,§5.4.3);terminal 专属方法 no-op。
3. **UiLeaseTable**:acquire-or-renew 幂等(同 `(directory, sessionId, clientId)` 续期返回同 leaseId)/心跳过期(3 次未续期移除 holder)/引用计数(两个 clientId → 全释放才翻 none)/翻转回调恰触发一次(none→active→none)/release 立即生效/SSE 存活不影响过期判定。
4. engine 装配:**capability × 租约 双门** —— `dialogs.v1=false`:lease 端点 501、createAgentSession 不带 `hasUI`(工具表不含 `ask`,对齐 ask.ts:832-834)、`tools.approvalMode=always-ask` 的 bash 调用抛错文案与现状一致(wrapper.ts:307-322);`true` **但无租约**:同配置仍抛错(v3 新断言:capability 不得单独改变行为);`true` 且持租约:同配置调用挂起而非抛错,runner 重装配后 `runner.hasUI()==true`(runner.ts:878-880),`setToolUIContext` 同步;租约全部过期后新审批门调用回抛错。
5. 生命周期出口:SIGTERM 链与改造后的 dispose 路由均先 settleAll 再退场;sweeper 跳过含 pending 会话(engine.js:126-131 反例)。

**E2E(dev 栈 5180/3902,浏览器驱动)**:
1. 审批正/反向(先 acquire 租约):`~/.omp/agent/config.yml` 置 `tools.approvalMode: always-ask` → 触发 bash → 弹窗出现且正文含 `Command:`(formatApprovalPrompt)→ Approve → 工具完成;Deny → tool part 错误文本 `Tool call denied by user: bash`(wrapper.ts:340)。
2. bash.patterns:加 `{match:"git push*", approval:"prompt"}` → 仅该命令弹窗(对照 bash.ts:563-570);`deny` 规则 → 不弹窗直接报 "Blocked by bash pattern"(bash.ts:551-557)。
3. **"始终允许"高级操作(事务,R2-M1)**:成功路径 → `tools.approval.bash=="allow"` 落盘 + 本次 Approve + 同命令不再弹(对照 approval.ts:117-118);**失败路径(注入设置写 5xx/只读)** → 弹窗保持打开、无 Approve 应答发出、错误 toast 可见、手动 Approve/Deny 仍可用;无人值守任务(fail-closed):同一 always-ask 配置下跑 scheduled task → 工具错误含 "no interactive UI available" 类文案、任务步失败、诊断可见、**全局设置未被改写、租约表无该会话记录**。
4. ask 全链:诱导 `ask`(options+recommended+multi)→ modal 交互(Other/Chat about this/multi 空提交);`ask.timeout: 5` → 不操作 5s(自 presented-ack 起)自动选推荐项且 transcript 标 "auto-selected after timeout"(对照 ask.ts:1454-1456);**关闭页面再开(orphan 窗口内)** → 租约重取、modal 恢复、重发 presented-ack、计时继续(§5.6.2)。
5. 背压:弹窗挂起时刷新浏览器 → `GET /api/omp/dialogs` 对账恢复;弹窗挂起时按 Stop → modal 消失、tool part aborted、`omp.dialog.settled {outcome:"aborted"}`。
6. 通知:后台会话审批到达 → toast/OS 通知/tray 菜单可 Approve(§5.3.4)。
7. **R11 生命周期四组**(master D6 R11):
   - a. **双端竞答**:两个客户端(浏览器 + tray)同时 respond 同一 dialog → 先到 200、后到 409,两端弹窗均经 settled 关闭,工具恰好执行一次;
   - b. **错误目录应答**:对绑定目录 B 的 dialog 以 `directory:A` 提交 respond → 403,对话框仍 pending,B 目录 UI 无感、A 目录快照不含它;
   - c. **abort**:挂起时 Stop → settled(aborted)、tool part 错误、turn 结束(§5.7);
   - d. **热重启**:挂起 ≥2 个对话框时 SIGTERM omp-host(及改造后的 `/global/dispose`)→ 重启后 `GET /api/omp/dialogs` 空、transcript 冷读含 aborted 工具错误与结算原因文本、UI 重连对账清卡、无僵尸 pending。
8. **认证矩阵(R4)**:直连/桌面/VS Code/relay 四形态 × (GET 快照、POST lease、POST respond、`/api/omp/events` SSE 重连带 `Last-Event-ID`)全部成功 —— SSE 与重放头穿透不被剥离(§5.2 转换表要求)。
9. **租约与保护 TTL 组(v3,R2-H1/H7/M2)**:
   - a. **无租约会话**:无任何 UI 打开该目录时经 API 发起会话/消息 → prompt 档工具立即错误(文案对齐 wrapper.ts:307-322)、`GET /api/omp/dialogs` 无该会话条目、无 pending;
   - b. **旧 UI 模拟**(不调用 lease 端点的客户端,如旧 bundle/curl 直发消息)→ 行为与 a 完全一致(fail-closed,无不可见 pending);
   - c. **scheduled task**:always-ask 下跑任务 → 同 a;引擎日志/任务诊断含失败原因,租约表无记录;
   - d. **断线孤儿结算**:弹窗挂起时关闭全部浏览器 → ≤30s 租约过期 → orphan 窗口(默认 120s)内不重连 → settled `timeout`、approval tool part 错误文本含 "orphaned"、无悬挂 turn;
   - e. **orphan 窗口内重连**:关闭后 ~60s 重开页面 → 租约重取成功、modal 从快照恢复、应答 200、工具继续;
   - f. **队列计时锚点**:同会话排队两个 `ask.timeout: 5` 对话框 → 第一个 ack+应答后第二个才成为活动 modal 并 ack;断言第二个的自动提交时刻 ≈ 其 presentedAt+5s,而与其 createdAt 无关(§5.4.3/§5.6.3);
   - g. **T_present 保护**:持租约但永不呈现(阻断 ack)→ 300s 后 settled `timeout`(可用缩小常量加速验证)。

**TUI 对照(行为基准,引用即规格)**:
- 同一 config.yml 下,TUI 与 web 对同一工具调用的批准决策一致 —— 由共享 `resolveApproval`(approval.ts:120-219)保证;对照点:弹窗正文同文(approval.ts:258-279)、Deny 错误同文(wrapper.ts:340)、ask 超时后缀同文(ask.ts:714/734)。
- 已知有意差异(记录在案):web 无 TUI 的按键超时重置(§5.4.3);web 提供弹窗外高级操作"始终允许(写设置+批准,事务)"(TUI 无,语义即 omp 设置覆盖,R2-M1);web 多对话框队列(首层 ack)vs TUI 串行 modal;**web 会话交互性由 UI 租约决定、断线有 orphan 窗口有界结算**(TUI 进程即终端在场,无此概念,R2-H1)。

---

## 8. 开放问题 [REVISED v3:OQ-3 事实更新 / OQ-7 冻结 / 新增 OQ-11]

| # | 问题 | 建议 |
|---|---|---|
| OQ-1 | **always-for-session 持久化位置**(任务书指定必答) | 不设会话级持久层:session 内存态违背"TUI 为准"且 TUI 无此面;"始终允许"落到 `tools.approval.<policyKey\|tool>`(全局 YAML,`~/.omp/agent/config.yml`,与 TUI/CLI 共享)。若后续要项目级,走 `.omp/config.yml` 的既有分层 —— 但 omp 项目层目前只权威 `modelRoles` 子树(settings.ts:~1218),扩层需上游配合,不在本域 |
| OQ-2 | 弹窗 Deny 是否附理由输入 | P1 不做(TUI 无);模型可随后续 turn 用 ask 追问。若反馈强烈,加可选折叠输入框,注入 deny 后的合成 tool result 文本 |
| OQ-3 | 定时任务/无人值守会话的审批策略 | **R10 已裁决基线**:fail-closed,不改全局审批设置(§5.3.3);任务编辑器警告已定。会话级 settings 注入是唯一增强路径 —— **注入口已存在**(`createAgentSession` 的 `options.settings`/`settingsManager`,sdk.ts:554-560、消费点 :1273-1275,第二轮评审核实),但 omp-host 现以进程级 `Settings.init` 单例运行,全局/目录层 Settings 的权威模型归第 06 章(R2-3)裁决;其定稿前不提供任何任务侧审批改写(v1"任务模板写 allow 快照"建议作废) |
| OQ-4 | 并行工具多弹窗策略 | 队列 + 计数徽标(§5.6.3);若上游 TUI 后续引入并行 dialog 交互再对齐 |
| OQ-5 | ~~`openchamber:omp-dialog-*` 命名与作用域~~ **已裁决(R1)** | 事件定名 `omp.dialog.requested`/`omp.dialog.settled`,走 05 章唯一 `/api/omp/events` 通道,directory 作用域(§5.2 注册表行);v1 命名留作沿革记录(§2.4)。本章无残留动作 |
| OQ-6 | approval 弹窗与 tool part 的关联精度 | P1 尽力关联(§5.3.1);精准方案是上游在 `select()` 调用点携带 toolCallId(wrapper.ts:332 签名扩展)—— 建议向上游提 issue,落地后 dialog 结构体加必填 `toolCallId` |
| OQ-7 | ~~vendored wire 中 permission/question 类型裁剪~~ **已冻结(R2-M8)** | 冻结裁决与 00 D1/07 §5.0 一致:**gen 文件不动,仅删消费引用**(P3-4..13 引用清零即完成,类型随上游重生成自然消失,§5.8 P3-14);不再是开放问题,本域无待决动作 |
| OQ-8 | 扩展生态 UI(`custom()` overlay、registerShortcut 等)在 web 的边界 | 本章明确只桥五方法 + editor;`custom()` 等维持 no-op。若未来要 web 化扩展 UI,是独立设计域(不属域 C) |
| OQ-9 | master D1 的 `/api/omp/...` 前缀与既有 `/api/permission-auto-accept` 的删除是否需要过渡别名 | 不需要:该路由从未对 omp 引擎生效(空桩),P3-8 直接删(§6) |
| OQ-10 | **R4 认证头转换表(直连/桌面/VS Code/relay)的权威落位** | 本章按总纲 R4 引用并要求穿透验证(§5.2、§7 E2E-8);现状锚点 `auth-state-runtime.js:47-51`。建议转换表本体随 05 章通道节或总纲附录定稿 —— 若 05 章修订未收录,需回填一处唯一权威,避免各章各写一份 |
| OQ-11 | **UI 租约与保护 TTL 的常量/策略定档**(v3 新增,covenant 相关) | 心跳 10s / 租约 TTL 30s / orphan 窗口 120s / T_present 300s 为本章工程默认值(§5.1 D-C1b、§5.4.3)。孤儿结算 outcome 按 kind 定死(approval → deny-fail、ask → abort,§5.6.1)—— 若产品要求"可配置 outcome"或按部署调参,需在总纲确认;**租约模型本身是对 D6 R10(fail-closed)的细化而非变更,建议总纲下轮把"capability ≠ UI presence、hasUI 由租约决定"收编为正式裁决**,避免后续章节再以 capability 直驱交互性 |

---

## 9. 依赖 [REVISED v3:租约/TTL 前置 + 上游清单更新]

**前置**:
- Ch05(事件流):`omp.dialog.requested/settled` 走 05 章唯一 `OmpEventBus → /api/omp/events` 通道(R1;本章只登记注册表行);`GET /api/omp/capabilities` 端点与 `dialogs.v1` 状态承载(R2);`omp.resync` 与 D2 bootstrap 顺序中的 dialogs 位(**该步含 UI 租约 acquire,先于快照对账,§5.6.2**);认证头转换表落位(OQ-10);`tool_execution_update` 等映射影响弹窗期间 tool part 的 pending 呈现。
- Ch06(设置):`tools.approvalMode/tools.approval/bash.patterns/ask.*` 的读写通道;"始终允许"高级操作的**事务式**反写路径(写成功才批准,§5.3.2);无人值守任务 fail-closed 的设置读取路径(§5.3.3);per-session Settings 注入的权威模型(R2-3,影响 OQ-3)。

**后置**:
- Ch07(残留清除):本文件 §5.8 即 A3/A4 的执行细则;删除窗口 = P1 消费者切换 + 观察期之后(R12,与 07 章大扫除同窗)。
- Ch02(agents):AgentPermissionsEditor 删除与 agent 模型收敛的合流点(§5.8 P3-10)。
- Ch08(原创面):scheduled tasks 输入迁移(OQ-3,fail-closed 语义);通知/tray 集成(§5.3.4);**per-session approval-mode 端点依赖删除**(master D6 R10)。

- OQ-6:`select()` 审批调用携带 toolCallId;
- OQ-3:会话级 settings 注入(注入口已存在,sdk.ts:554-560/:1273-1275;omp-host 单例改造与目录隔离权威模型归第 06 章 R2-3 裁决 —— 任务侧增强的唯一正道,定稿前 fail-closed);
- 运行期 ask 工具注册(消除 §5.1 已知局限:创建期无租约的会话被用户打开后仍无 ask;非阻塞);
- ask.ts:527-535 的超时推断容错在 v3 presented-ack 锚点模型下仍成立(server 计时更权威),无需上游变更。
