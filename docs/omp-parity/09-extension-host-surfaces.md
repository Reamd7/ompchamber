# 09 · 域 I:扩展宿主面(Extension Host Surfaces)

状态:设计稿 v1(2026-08-21 立项,由用户级真实扩展需求触发)
基线:omp SDK 17.3.7(vendored `packages/web/node_modules/@oh-my-pi/pi-coding-agent`,下文 `<s>` = 该目录;TUI 源 = `C:/Users/reamd/Documents/experiment_area/oh-my-pi/packages/coding-agent/src`)
裁决依据:00-MASTER D1(omp 原生概念走自有面)、D2(降级显式)、R1(事件单通道);本章新裁决 R-E1/R-E2/R-E3 见 §3。

---

## 1. 域概述与边界

**本域管:**

- omp 扩展(`~/.omp/agent/extensions/*.ts` 与项目级,SDK 启动加载)经宿主边界暴露的一切面的 **OpenChamber 消费**:
  - `ctx.ui`(ExtensionUIContext,`<s>/dist/types/extensibility/extensions/types.d.ts:151-228`)的**非对话框成员**——widget/status/title/editor 文本/open_url;
  - `pi.register*` 注册面中 OC 侧有消费缺口的:provider 流通性、shortcut;
  - TUI 绑定面(组件工厂类)的**可观测丢弃与未来渲染路径**。
- 投影机制本体:host 存储、`/api/omp` 快照端点、事件、UI 落点。

**本域不管:**

- 对话框半边(select/confirm/input/askDialog/notify)——**03 章已交付**(domain-dialogs.js:682-774),本章只改它的 no-op 成员;
- custom 消息/customType 的**渲染分层**——**05 章 GAP-E11**(P2,T1-T4 分层)是唯一权威;本章仅登记扩展来源条目汇入该管道的差别(§5.5);
- 事件通道机制(envelope/重放/注册表)——05 章 R1 单通道,本章只登记新事件;
- 扩展引擎侧运行时(加载/信任/生命周期/事件钩子执行)——SDK 内闭环,OC 不经手;
- `pi.on()` 流程拦截面(input/user_bash/user_python/tool_call 改写等 36 事件,`types.d.ts:791-835`)——引擎侧行为,扩展在 OC 宿主下与 TUI 下同样生效,**无需投影**;其中 `user_bash` 恰为 07 章 GAP-G05(`!` 本地执行面)的一条实现路径,归 07/02 章裁决。

**与其他章的接口:**

- 03 章:同一 `createDialogBridge` 实例(domain-dialogs.js:647)——本章把它的 chrome 成员从 no-op 变实;
- 05 章:`omp.chrome.updated` 事件按 R1 走 `OmpEventBus → /api/omp/events`,登记进注册表(check:events 门);
- 08 章:composer/StatusRow 是 WidgetBar 与 status 段的落点(§5.1/5.2)。

## 2. 现状分析(证据)

### 2.1 宿主边界的现状:收到即丢弃

`packages/web/server/lib/omp-host/domain-dialogs.js:766-797` 的 web `ExtensionUIContext`:

- **实**:select/confirm/input/askDialog/editor(对话框)/notify(→ onNotify → 通知系统);
- **no-op**(:775-788):`onTerminalInput`、`setStatus`、`setWorkingMessage`、`setWidget`、`setFooter`、`setHeader`、`setTitle`、`custom`、`setEditorText`、`pasteToEditor`、`getEditorText`、`addAutocompleteProvider`、`setEditorComponent`。

宿主会话 `mode: 'json'`(engine.js:213)。守 `ctx.mode === 'tui'` 才发 chrome 的扩展在 OC 下不调用(边界,见 §3.4);不守的(探测式/try-catch 式)会调用然后被丢弃。

### 2.2 官方宿主契约已存在:RpcExtensionUIRequest

omp 自己的 RPC 模式(`oh-my-pi/src/modes/rpc/rpc-mode.ts:798-882`,契约 `<s>/dist/types/modes/rpc/rpc-types.d.ts:591-665`)把字符串载荷的 chrome **全部转发给宿主**:

| ctx.ui 方法 | RPC 行为 | 证据 |
|---|---|---|
| `setWidget(key, string[]\|undefined, {placement})` | **发出**;注释:"Only support string arrays… factory functions are ignored" | rpc-mode.ts:824-837 |
| `setStatus(key, text)` | **发出** | :809-818 |
| `setEditorText(text)` | **发出**("host can implement editor control") | :868-876 |
| `pasteToEditor(text)` | 降级为 setEditorText 发出 | :863-866 |
| `setTitle(title)` | 发出,宿主 opt-in(`PI_RPC_EMIT_TITLE=1`,"low-value noise") | :847-856 |
| `open_url(url, launchUrl, instructions)` | **发出**("hosts SHOULD surface it",OAuth 回跳) | :1412, rpc-types:654-664 |
| `setWorkingMessage` / `getEditorText` | 不支持 | :820-822/:878-882 |
| `setFooter`/`setHeader`/`custom()`/组件工厂 | 不支持("would need TUI access") | :839-861 |

**含义**:字符串载荷 chrome 在 omp 官方语义里就是宿主面;OC 的 no-op 是偏离而非对齐。本章实现 = 在进程内 bridge 上对齐该契约(不经 RPC 线,但**字段名与语义照抄** `RpcExtensionUIRequest`,保持 omp 生态一致)。

### 2.3 用户级真实扩展的落点(本域的验收素材)

`C:/Users/reamd/.omp/agent/extensions/`(6 个,全部不守 mode-guard):

| 扩展 | 调用 | 证据 | OC 现状 |
|---|---|---|---|
| zhipu-usage.ts | `setWidget(key, string[], {placement:"aboveEditor"})`,300s 自刷新;`/zhipu-usage off` → `setWidget(key, undefined)` | :483/:511/:616/:629 | ❌ 丢弃 |
| tps-monitor.ts | `ctx.ui.setStatus(key, "… · …")`(探测式 `typeof setStatus === 'function'`) | :915-926 | ❌ 丢弃 |
| orca-titlebar-spinner.ts | `ctx.ui.setTitle(…)` | :32/:42 | ❌ 丢弃 |
| orca-prefill.ts | `ctx.ui.setEditorText(prefill)` | :10 | ❌ 丢弃 |
| sandbox.ts | `ctx.ui.notify(…)` | :123/:220 | ✅ 已通 |
| orca-agent-status.ts | 待逐行核对(立项时未验,§8) | — | — |

### 2.4 周边现状

- **custom 条目**:扩展经 `pi.sendMessage(custom)`/`pi.appendEntry(type,data)` 写入的条目走 omp custom 管道,OC 侧现为 `[omp:<customType>]` 文本前缀 + synthetic 消息(projection.js:159-189,05 章 §2 已录);结构化分层 = 05 章 GAP-E11(P2,未做)。扩展专属的 TUI 渲染器 `registerMessageRenderer`/`registerAssistantThinkingRenderer`(types.d.ts:915-917)为组件工厂,归 §3.3 未来路径。
- **registerProvider**(types.d.ts:995):扩展可注册自定义供应商(含 `streamSimple` 自定义流)。理论上经引擎 modelRegistry 流入 `/api/omp/models` → 选择器自动出现——**流通性未验证**(§5.6,一条 curl)。
- **registerShortcut**(types.d.ts:899-903):TUI KeyId 键位,OC 无对应物(CommandPalette 是语义不同的面)。

### 2.5 Settings plugins 面(已交付,2026-08-23)

Settings → Plugins 页是 OMP 面,不是 OpenCode `plugin` 配置编辑器(旧 AddPluginDialog/RegistryBadge/RegistryBanner 已删)。数据源唯一:`PluginManager` + `MarketplaceManager` + `getEnabledPlugins()` + `discoverExtensionPaths()`,经 `RuntimeAPIs.ompPlugins`(`packages/ui/src/lib/api/omp.ts`,zod 边界)消费 `packages/web/server/lib/omp-host/domain-plugins.js` 的 `plugins.v1` 端点组。

**投影**(GET /api/omp/plugins?directory=):npm/link 包插件(user+project)、marketplace 插件(含 project scope)、manifest 扩展入口(feature-gated + loaded/missing)、native 扩展文件(`~/.omp/agent/extensions` + `<project>/.omp/extensions`)、settings.extensions 配置路径。旁路端点:applied(per-session 物化快照)、reveal(Finder/Explorer/xdg-open,服务端按 id 反查路径,不接受任意路径)、reload(缓存失效 + refreshAgentDiscovery + refreshSkills,可选 sessionId 单会话刷新)。

**变更边界**:

- user 包插件 → `PluginManager` setEnabled/setEnabledFeatures/setPluginSetting(omp parseSettingValue + validateSetting 在边界校验);
- **project 包插件 → `.omp/plugin-overrides.json` 直写**(`applyProjectOverride()`,原子写 temp+rename;格式 = SDK `ProjectPluginOverrides`{disabled,features,settings};写后 `invalidatePluginCaches` 下次发现即生效)。SDK 无公开写入口径,该文件即 TUI 管理的同一文件——fs 直写是唯一路径,非平行存储;
- 权限矩阵:project npm 插件 toggle/features/settings = true(overrides 写入),uninstall = false(卸载是用户根全局操作);
- 变更后一律 `clearPluginRootsAndCaches` —— 冒烟测试抓到的真 bug:不失效则新装插件的 manifest 入口投影为 not loaded。

**安装 scope 语义**(2026-08-23 终版):用户显式选 User/Project,UI 不做启发式分类。切换到 Project 时小字即时提示规则;服务端对 project + 非 marketplace spec **400 拒绝**(文案可操作),**不静默降级** —— project 是更严格意图,静默给 user 违背显式选择。此处**有意偏离** omp CLI 的 warn-and-proceed(plugin-cli.ts:390/430):终端一次性命令可事后读输出,设置页会留下持久状态,降级即坑。

验证:domain-plugins.test.js + omp-host 全套 270/270;tsc;i18n ×11;真实插件安装→开关→设置→卸载全链路冒烟通过。

## 3. 目标语义

### 3.1 R-E1 官方契约为准

chrome 投影的载荷、字段名、清除语义以 `RpcExtensionUIRequest` 为规范:`setWidget → {widgetKey, widgetLines: string[]|undefined, widgetPlacement}`、`setStatus → {statusKey, statusText: string|undefined}`、`setTitle → {title}`、`set_editor_text → {text}`、`open_url → {url, launchUrl?, instructions?}`。`undefined` 载荷一律语义为**清除**。

### 3.2 R-E2 被动面不租约门控

对话框无租约即 fail-closed(03 章语义:没人能回答就不该挂起);chrome 是**纯展示**,后台会话同样合法——setWidget/setStatus/setTitle/open_url/setEditorText **不经租约判定**,按 `{directory, key}` 直写 host 存储。setEditorText 是唯一例外语义(写用户输入区,§5.3)。

### 3.3 R-E3 TUI 绑定面的未来可渲染原则(总纲级裁决,2026-08-21 用户指令)

组件工厂类载荷(`ExtensionUiComponentFactory`、`setFooter/setHeader`、`custom()`、`registerMessageRenderer`)**今天不判死刑**。三级机制:

1. **可观测丢弃**:bridge 对工厂载荷不再静默——按 `{directory, method}` 计数 + 一次性日志;快照端点携带 `dropped` 段(§5.7),UI 可提示"N 个扩展组件面待投影";
2. **上游跟踪项**:向 omp 提议组件工厂的**序列化投影契约**(组件树 → 声明式 JSON schema,如 `{type:"box", children:[{type:"text", text}]}`),模式同 08 章队列方案 B 的上游前置登记(08 §5.7 R2-H6)——上游合入前不实现消费端;
3. **渲染路径预留**:WidgetBar 的渲染层按"声明式树渲染槽"设计——今天只喂 string[] 行,上游契约落地后同一组件换数据源即可渲染声明式树,不改 UI 结构。

扩展作者逃生门(文档化):string[] 形式的 widget **今天即渲染**。

### 3.4 边界:mode 诚实

宿主继续如实报告 `mode: 'json'`;不谎报 `'tui'` 骗 guard 型扩展发 UI。守 `mode === 'tui'` 的扩展在 OC 无 chrome,属上游语义,不是本章缺陷。

## 4. 差距清单

| 编号 | 差距 | 分类 | 优先级 | 风险 |
|---|---|---|---|---|
| GAP-E01 | `setWidget(string[])` 投影:bridge 存储 + 事件 + 快照 + composer WidgetBar(above/belowEditor) | 建 | **P1**(用户级扩展在场等待) | 低(增量、门控) |
| GAP-E02 | `setStatus(key,text)` 投影:StatusRow 扩展段 | 建 | **P1** | 低 |
| GAP-E03 | `setEditorText`/`pasteToEditor` → 会话 composer 预填(与 /undo 预填同机制) | 建 | P2 | 中(写用户输入区,需会话对齐规则) |
| GAP-E04 | `setTitle`(opt-in 语义对齐 PI_RPC_EMIT_TITLE)+ `open_url`(浏览器宿主原生开链接 + launchUrl 复制目标) | 建 | P2 | 低 |
| GAP-E05 | 扩展 custom 条目汇入 05 章分层渲染(本章仅登记差别,实现归 05 GAP-E11) | 留(指针) | 随 05 章 P2 | — |
| GAP-E06 | `registerProvider` 流通性验证(扩展供应商 → `/api/omp/models` → 选择器) | 验证 | **P1**(一条 curl,阻塞才立项) | 低 |
| GAP-E07 | `registerShortcut` → CommandPalette 映射评估 | 评估 | P3 | — |
| GAP-E08 | TUI 绑定面三级机制:dropped 可观测 + 上游序列化契约跟踪 + WidgetBar 声明式渲染槽(R-E3) | 建(轻)+ 登记 | P2 | 低 |

## 5. 设计方案

### 5.0 总则

- 能力门控:capabilities 新 key **`extensionChrome.v1`**(门 E01-E04 + E08 的消费面;服务端 chrome 存储与事件恒产,UI 面受门)。与 03 章 `dialogs.v1` 并列登记。
- 存储:host 进程内 per-directory map:`{[directory]: {widgets: {[key]: {lines, placement, sessionId, updatedAt}}, status: {[key]: {text, sessionId, updatedAt}}, dropped: {[method]: count}}}`。多会话写同 key = **last-writer-wins**,快照携带 sessionId 供 UI 标注来源(zhipu 配额是账号级数据,agent-status 类是会话级,由扩展自选 key 命名空间)。
- 事件:**`omp.chrome.updated`**(`{kind: 'widget'|'status'|'title'|'editor'|'open_url', key?, directory, sessionId}`,volatile;载荷全量随事件,重连以快照对账)。注册进 05 章事件注册表(check:events 计数 +1)。`title`/`editor`/`open_url` 为一次性指令型,不入快照、不重放。
- 快照:`GET /api/omp/chrome?directory=` → `{widgets: [...], status: [...], dropped: {...}, revision}`。D2 降级:GET 失败 ≠ 清空——UI 冻结现有内容,重连对账。

### 5.1 GAP-E01 WidgetBar

- `ChatContainer` 在 composer 容器上/下按 `placement` 渲染 `<OmpExtensionWidgetBar directory>`:等宽字体行、内容 **verbatim**(扩展产出即真相,同 03 章弹窗 prompt verbatim 规则);窄布局(移动/VSCode compact)横向截断 + 悬停展开;≤10 行(SDK widget cap,扩展侧已自守)。
- 清除:widgetLines undefined 或行数为 0 → 移除该 key 的条。

### 5.2 GAP-E02 status 段

- StatusRow 新增扩展段区:按 key 渲染 `text`,多个 key 以 ` · ` 连接(tps-monitor 的产出即此格式);与 work-status 徽标并列,样式 `typography-meta` + muted。

### 5.3 GAP-E03 composer 预填

- `setEditorText(text)`:目标会话 = 调用方 session;若该会话 composer 在场 → 预填(不自动发送);不在场 → **丢弃并 dropped 计数**(不跨会话注入——orca-prefill 类扩展在 session_start 上下文调用,天然同会话)。

### 5.4 GAP-E04 title / open_url

- title:对齐上游 opt-in 精神——OC 端设置项 `extensionChrome.setTitle`(默认 off);开时 web 改 document.title、Electron 改窗口 title。
- open_url:web/desktop 用系统浏览器打开;`launchUrl` 存在时通知栏同时给出"复制短链"动作(保 OAuth 参数不被截断,上游注释语义)。

### 5.5 GAP-E05 custom 条目(指针)

扩展条目与核心 customType 走同一管道(05 §2:`[omp:<type>]` 前缀合成消息)。05 章 GAP-E11 的 T1-T4 分层落地时,扩展自定义类型落 **T4(未知类型 → 通用卡片:type 徽章 + JSON 折叠)**——该层同时服务核心未知类型与扩展类型,唯一权威在 05 章。

### 5.6 GAP-E06 provider 流通验证

一步:装一个 registerProvider 测试扩展(或用最小 fixture)→ `GET /api/omp/models?directory=` 断言扩展模型在场 → 选择器可见。通过 = 关闭;失败 = 立项(引擎 registry → 快照投影链上找断点)。

### 5.7 GAP-E08 三级机制落地

- bridge:工厂载荷/不支持方法 → `dropped[method]++` + 进程内一次性 warn(带扩展名);
- 快照 `dropped` 段:WidgetBar 区域显示一行可关闭提示"N 个扩展组件面待上游投影契约"(i18n ×11);
- WidgetBar 渲染层:行渲染器接口 `renderChromeNode(node): ReactNode`,今天仅 `string` 分支;声明式树分支预留(R-E3.3);
- 上游跟踪:开放问题 1(§8)登记序列化契约提案,合入后开 `extensionChrome.components.v1` 消费端。

## 6. 迁移与兼容

- 三矩阵:`extensionChrome.v1` off / 旧 UI / relay 旧客户端 → 无 chrome 面,bridge 照存(数据无害);旧 UI 连新服务端无感知。
- 回滚:capabilities 摘 `extensionChrome.v1`,UI 面消失,存储与事件继续(可独立恢复)。
- 与 03 章关系:改的是同一 bridge 对象的成员,无新上下文;dialogs 语义零改动。
- check:events:`omp.chrome.updated` 入注册表(25 → 26);存储前缀如需落 localStorage 一律 `oc-omp-`。

## 7. 验证方案(设计,不执行)

单测(omp-host bun:test):

- bridge 行为:setWidget string[]/undefined/工厂(计数)/无目录;setStatus 清除;title opt-in;setEditorText 会话对齐与丢弃;open_url 载荷形状;
- 事件与快照:注册表登记、volatile 语义(重连快照对账)、D2(GET 失败不清空)、last-writer-wins、revision 单调;
- UI:capability off 字节等价;WidgetBar verbatim/截断/清除;status 段连接;dropped 提示。

live 验收(真实扩展即 fixtures,§2.3):

- 起 dev 栈,`zhipu-usage` 配额条在 composer 上方自亮、300s 自刷新、`/zhipu-usage off` 后消失;
- `tps-monitor` 状态段在 StatusRow 出现(探测式写法被点亮);
- `orca-prefill` 预填生效;`sandbox` notify 不回归。

门:`check:events` OK;UI 隔离套件;tsc;涉新文件 `bun run dead-code`。

## 8. 开放问题

1. **组件工厂序列化投影的上游提案**(R-E3.2):形状(组件树 JSON schema)、协商(能力探测 vs 版本)、是否含交互(onSelect 等)——向 omp 上游提案后回填;合入前 OC 只做 dropped 可观测。
2. **orca-agent-status.ts 的面清单未核对**(立项时未逐行读),实施 GAP-E01/02 前补一次。
3. **setStatus 的多目录可见性**:status 是 footer 级(TUI 全局可见)还是会话级?TUI 侧取证后定 OC 呈现范围(当前设计:directory 级)。
4. registerShortcut 的 CommandPalette 映射价值:KeyId 语义(TUI 键位)与 OC 快捷体系差异大,P3 评估是否值得。

## 9. 依赖

前置(本章消费):

- 03 章:bridge 所有权与 notify 通路(已交付);
- 05 章:事件注册表与 R1 单通道、快照对账惯例(已交付);GAP-E11(T4 层)是 E05 的实现前置。

后置(消费本章产出):

- 07 章观察期无交集(chrome 面不触碰 legacy 协议);
- 上游 SDK:组件序列化契约(开放问题 1)是 `extensionChrome.components.v1` 的硬前置。
