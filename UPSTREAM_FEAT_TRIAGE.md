# 上游 feat 功能线甄别与吸收进度表(144 提交)

> 数据:`git log main..openchamber/main`(fork main = 138685d7d);分析基座:merge-openchamber 分支 `925482ac6`。
> 判定:diff 引入 `@opencode/client|schema` = **v2 SDK 依赖**;subject/diff 含 OC2 语义 = **引擎耦合**;触碰 packages/web|sdk = **需 server-rs 移植**;其余 = **自身能力**。
> **用法**:行勾选 `- [x]` = 已吸收;未勾但状态列有 SKIP/PARTIAL = 终态跳过/部分吸收。线标题勾选 = 该线全部行已有终态。进度列格式 `X/N(吸A/跳B/半C)`,X=已有终态行数。
> 图例:🟢 高价值 🟡 中价值 🔴 大工程 ⚫ 不可用

| 功能线 | 提交数 | v2 | 引擎 | 服务端 | 价值 | 进度 |
|---|---|---|---|---|---|---|
| Git/Diff 增强 | 9 | 0 | 0 | 5 | 🟢 高价值 / 自身能力 | 9/9 (9吸) |
| 主题/外观 | 5 | 0 | 1 | 2 | 🟢 高价值 / 自身能力 | 5/5 (2吸/3跳) |
| 会话管理 | 5 | 1 | 0 | 3 | 🟢 高价值 / 基本自身 | 5/5 (3吸/2跳) |
| Worktree/项目 | 3 | 0 | 0 | 3 | 🟢 高价值 / 自身能力 | 3/3 (3吸) |
| 终端 | 1 | 0 | 0 | 1 | 🟢 高价值 / 自身能力 | 1/1 (0吸/1跳) |
| Sidebar/Timeline | 7 | 0 | 0 | 3 | 🟢 高价值 / 自身能力 | 7/7 (3吸/4跳) |
| 文件/Context Panel | 4 | 0 | 1 | 1 | 🟢 高价值 / 自身能力 | 4/4 (3吸/1半) |
| Mobile | 7 | 0 | 0 | 2 | 🟢 中高价值 / 自身能力 | 7/7 (5吸/2跳) |
| VS Code | 4 | 0 | 1 | 0 | 🟢 中价值 / 自身能力 | 4/4 (3吸/1跳) |
| Composer/Chat 交互改版 | 22 | 0 | 4 | 7 | 🟡 中价值 / 高互锁 | 0/22 |
| Comments 评论 | 3 | 0 | 0 | 0 | 🟡 中价值 / 半独立 | 0/3 |
| Stats/Usage/Quota | 7 | 1 | 1 | 5 | 🟡 中价值 / 部分引擎依赖 | 6/7 (1吸/1跳/4半) |
| Routing/模型选择 | 5 | 0 | 2 | 4 | 🟡 中价值 / 部分引擎依赖 | 0/5 |
| 小模型 Jev | 2 | 1 | 0 | 2 | 🟡 中价值 / 需对齐 | 1/2 (0吸/1跳) |
| MCP/Plugins | 3 | 1 | 0 | 1 | 🟡 中价值 / 部分引擎依赖 | 1/3 (0吸/1跳) |
| 权限/安全 | 1 | 1 | 0 | 1 | 🟡 中价值 / 引擎+服务端 | 1/1 (0吸/1跳) |
| Websearch | 1 | 1 | 0 | 1 | 🟡 中价值 / v2 SDK | 1/1 (0吸/1跳) |
| 通知/浏览器 | 2 | 0 | 1 | 2 | 🟡 中价值 / 依赖扩展平台 | 0/2 |
| Spaces 隔离空间 | 12 | 0 | 2 | 12 | 🔴 大工程 / OC2 时代设计 | 12/12 (0吸/12跳) |
| Extensions/SDK 平台 | 12 | 2 | 0 | 12 | 🔴 大工程 / 平台级 | 12/12 (0吸/12跳) |
| OpenCode 2 迁移 | 2 | 2 | 0 | 2 | ⚫ OC2 依赖(不可用) | 2/2 (0吸/2跳) |
| 其他 | 27 | 4 | 5 | 7 | 🟡 杂项 / 逐条判定 | 5/27 (1吸/4跳) |

## - [x] Git/Diff 增强 — 🟢 高价值 / 自身能力

逐 hunk 操作、diff 文件树、PR 审阅——git 服务在 fork 有 server-rs 完整实现,5 提交含服务端需对照移植。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [x] | `85be91762` | feat(git): make the changes tree read as a tree | `自身` | PORTED(干净落地) |
| - [x] | `7c5637ab7` | feat(diff): add a file tree mode with one file open at a time | `服务端` | PORTED(设置键走 fork 本地 persist,registry 文件按 fork 架构不引入) |
| - [x] | `1938b9c5f` | feat(diff): expand collapsed context in pull request comparisons | `服务端` | PORTED(服务端移植 server-rs:pr-file 路由) |
| - [x] | `c469a40bf` | feat(review): inherit permission auto-accept in new review sessions | `自身` | PORTED(干净落地) |
| - [x] | `d5fbd86e9` | feat(diff): show a loading state on collapsed-context separators | `自身` | PORTED(PierreDiffViewer 取上游整文件) |
| - [x] | `bb7bededf` | feat(git): review published pull request diffs in Changes | `服务端` | PORTED(服务端移植 server-rs:pr-diff 路由/跨仓 PR source) |
| - [x] | `a1d4eda4c` | feat(git): float actions beside each diff hunk | `服务端` | PORTED(保留 fork FileDiffActions,服务端语义已在 server-rs) |
| - [x] | `138f9ba02` | feat(git): safely apply individual diff hunks (#3443) | `服务端` | PORTED(服务端移植 server-rs) |
| - [x] | `ebe5f93fb` | feat(git): per-hunk stage, unstage, and discard actions in Changes view | `自身` | PORTED(干净落地) |

## - [x] 主题/外观 — 🟢 高价值 / 自身能力

内置配色、VS Code 主题导入、字体——零依赖,拿来即用。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [ ] | `431972e0f` | feat(theme): add built-in Cursor palettes | `自身` | SKIP(互锁:新调色板缺 fork Theme 必填键,依赖上游 definition.ts zod 默认值,fork 无该模块) |
| - [ ] | `39a4940db` | feat(theme): add Osaka Jade Refined built-in palettes | `自身` | SKIP(互锁:新调色板缺 fork Theme 必填键,依赖上游 definition.ts zod 默认值,fork 无该模块) |
| - [ ] | `6a20db4d0` | feat(theme): import VS Code palettes from Open VSX and local files | `服务端` | SKIP(互锁:VSIX 解包需新增 zip 依赖且 Open VSX 目录为全新服务端子系统,另立项) |
| - [x] | `beb8e79ea` | feat: add an always-show scrollbars preference | `引擎耦合?` `服务端` | PORTED(设置走 fork 本地 persist,registry 文件不引入) |
| - [x] | `bf9d3d04f` | feat: add Fixel font support | `自身` | PORTED(干净落地) |

## - [x] 会话管理 — 🟢 高价值 / 基本自身

work-until-done(v2 SDK 标记)、归档清理、精确搜索、AI 重命名、预热——除 1 提交外均纯 UI/本地。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [ ] | `ea15762ee` | feat(sessions): keep sessions in work until the user marks them done | `v2 SDK` `服务端` | SKIP(v2 SDK) |
| - [ ] | `9dfdacf91` | feat(settings): add session warming toggle to Sessions settings | `服务端` | SKIP(互锁:写 OpenCode config warming 键依赖 config-v2 管线,pi 引擎无 session warming 行为) |
| - [x] | `0e9e93dc7` | feat(sessions): add archived-only retention cleanup | `服务端` | PORTED(织入 fork 自动清理 hook;i18n 键改 ompchamber: 前缀;设置键走本地 persist) |
| - [x] | `0c7653c4c` | feat: rename sessions with AI using recent conversation turns | `自身` | PORTED(v2 SDK 类型换绑 v1 wire;runtime 事件名改 ompchamber: 前缀) |
| - [x] | `3903bfad8` | feat(sessions): add exact ID search to sidebar and archive (#3428) | `自身` | PORTED(干净落地) |

## - [x] Worktree/项目 — 🟢 高价值 / 自身能力

创建前拉取远端、chats 目录迁移、缺目录标记——git/fs 层,server-rs 有对应实现。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [x] | `9934110cd` | feat(projects): integrate default agents and preserve model selections (#3291) | `服务端` | PORTED(ModelControls variant 逻辑保留 fork 侧;variant 语义对齐上游 override=null=Default;i18n 键归位) |
| - [x] | `9cb3a2d7f` | feat(worktrees): fetch remote source branch before worktree creation (#3296) | `服务端` | PORTED(服务端移植 server-rs worktrees.rs:--no-track/fetch 回退/显式 upstream 优先) |
| - [x] | `d6bbbe9f4` | feat: add default agent to project settings | `服务端` | PORTED(cascade 织入 fork useConfigStore.cascade,内联副本删除;i18n 键归位 settings.projects.*) |

## - [x] 终端 — 🟢 高价值 / 自身能力

ghostty-web→仓内 libghostty-vt 适配器:消灭 fork 的 references/ghostty-web 外部引用。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [ ] | `e5c2c81bf` | feat(terminal): replace ghostty-web with an in-repo libghostty-vt adapter | `服务端` | SKIP(互锁:fork 终端栈已重写(sequence/revision 重放+viewport driver),上游适配器需整套 store/transport 替换,另立项) |

## - [x] Sidebar/Timeline — 🟢 高价值 / 自身能力

Timeline 视图、排序设置、provider logo、活动 spinner——纯 UI,fork sidebar 有本地改动需小量适配。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [ ] | `a8b1e93dd` | feat(sidebar): show the session's provider logo on timeline rows | `自身` | SKIP(互锁:SessionTimelineRowBody 属 7ad4 Timeline 视图) |
| - [x] | `38ea51fce` | feat(sidebar): mark projects whose folder is missing | `自身` | PORTED(SortableProjectItem 补 projectDirectory prop) |
| - [ ] | `d06fdc407` | feat(sidebar): add a worktree sort setting, manual by default | `服务端` | SKIP(互锁:构建于 7ad4 Timeline UI 之上) |
| - [x] | `65b3a5135` | feat(sidebar): mark sessions where the agent left a next step open | `服务端` | PORTED(wire time.idle 可选字段;元数据命名空间对齐 ompchamber) |
| - [ ] | `7ad4b2897` | feat(sidebar): add the Timeline view and a Grouped/Timeline switch | `服务端` | SKIP(互锁:Timeline 视图依赖 rowModel/headerPresentation 中间架构,fork sidebar 分叉) |
| - [x] | `0d59ddf3c` | feat(sidebar): unify sticky headers with crossfade transitions | `自身` | PORTED(干净织入) |
| - [ ] | `79a7da01c` | feat(sidebar): show running project actions by directory | `自身` | SKIP(互锁:依赖上游 terminal store 演进(execution 分配/reconcile),fork 终端栈重写) |

## - [x] 文件/Context Panel — 🟢 高价值 / 自身能力

持久文件树、编辑器切换、artifact 预览与 file.open——1 提交涉引擎动作需甄别。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [x] | `2d406f9f9` | feat(files): upload files from the folder menu and tree toolbar | `自身` | PORTED(FilesView 仅织入 upload 增量,preview 栈保留 fork) |
| - [x] | `8c9e08234` | feat(files): rich artifact previews and an agent file.open action | `服务端` | PARTIAL(preview 子系统+file-open 事件落地;FilesView 集成保留 fork 栈;server 侧 fork 已有 grants 等价) |
| - [x] | `49bfdac7f` | feat(context-panel): integrate persistent file tree and editor toggle (#3590) | `自身` | PORTED(编辑器树持久化织入;terminal 单例用例依赖富 store 已裁剪) |
| - [x] | `cf0904586` | feat(context-panel): keep file tree width and add editor toggle | `引擎耦合?` | PORTED(宽度键含 file-tree;min 宽对齐上游 320;terminal 目标用例依赖富 store 已裁) |

## - [x] Mobile — 🟢 中高价值 / 自身能力

会话抽屉、分支对比、手势——纯移动端 UI,fork mobile 面存在。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [x] | `5badd2472` | feat(mobile): add copy session ID swipe action (#4060) | `自身` | PORTED(MobileSessionSwipe 拆分+Copy ID 动作;timeline-list 依赖 ea15762ee 已跳过,不引入) |
| - [ ] | `54fc3f28a` | feat(mobile): recent section, pinning and a shorter swipe row in the sessions drawer | `服务端` | SKIP(互锁:依赖 ea15762ee work 状态(已跳)与 MobileTimelineList 活动指示器栈) |
| - [x] | `99b9c23c9` | feat(mobile): add branch and commit comparison views | `服务端` | PORTED(移动端 branch/commit 对比;DiffView 织入 fork 版 comparison cache 结构) |
| - [x] | `47bc2f604` | feat(mobile): drill into the OpenCode settings pages from a phone | `自身` | PORTED(干净落地,i18n 键补齐) |
| - [x] | `7194d5d41` | feat(mobile): follow the project sort order in the sessions drawer | `自身` | PORTED(干净落地) |
| - [x] | `baba83c4d` | feat(mobile): start a session in a project from the sessions drawer | `自身` | PORTED(MobileSwipeActionsRow 整组件落地) |
| - [x] | `ff8edd6d4` | feat(mobile): close the drawers with the reverse edge swipe | `自身` | PORTED(干净织入) |

## - [x] VS Code — 🟢 中价值 / 自身能力

prompt navigator 进扩展、多运行替换 Agent Manager、土耳其语本地化。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [x] | `9f8af62d4` | feat(vscode): replace the Agent Manager with shared multi-run | `自身` | SKIP(互锁:删除 Agent Manager 依赖共享 multi-run store,fork 无对应实现,保留 agent-manager 并补齐 i18n) |
| - [ ] | `0d358adea` | feat(vscode): enable the prompt navigator in the extension | `引擎耦合?` |  |
| - [x] | `fa482bcf2` | feat(vscode): localize loading splash and dev-server status for Turkish (#3303) | `自身` | PORTED(干净落地) |
| - [x] | `7c38f8aed` | feat(vscode): add Turkish localization for extension strings (#3300) | `自身` | PORTED(干净落地) |

## - [ ] Composer/Chat 交互改版 — 🟡 中价值 / 高互锁

浮动 composer、附件内嵌、/btw 隔离舱等;4 提交涉引擎行为,其余纯 UI 但与 fork composer 重写冲突。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [ ] | `d9fbf6b78` | feat(multirun): run on several models from the composer | `服务端` |  |
| - [ ] | `1a541f97f` | feat(chat): render compaction and shell notices as timeline rows | `引擎耦合?` |  |
| - [ ] | `9b3fcc16c` | feat(chat): keep the text selection menu on its selection while scrolling | `自身` |  |
| - [ ] | `142357ee7` | feat(chat): attach inline /skill mentions to the prompt | `自身` |  |
| - [ ] | `d7a0df9c7` | feat(chat): fork a session from an agent answer | `自身` |  |
| - [ ] | `04282b084` | feat(chat): describe permission requests in plain words | `引擎耦合?` |  |
| - [ ] | `807d820a0` | feat(chat): accept file attachments in the /btw composer | `自身` |  |
| - [ ] | `cd23d89e9` | feat(chat): customize work-status section order and collapse tasks | `引擎耦合?` `服务端` |  |
| - [ ] | `18f6bb2c9` | feat(chat): dock the suggested message inside the composer everywhere | `自身` |  |
| - [ ] | `a5bdcd6df` | feat(chat): float the recap hint over the composer and glass the popups | `自身` |  |
| - [ ] | `084eeabc3` | feat(chat): float the composer over the transcript | `自身` |  |
| - [ ] | `af15c1154` | feat(chat): move attachments inside the composer and cite pasted files | `自身` |  |
| - [ ] | `a16d947a0` | feat(chat): improve session assistance and floating panels | `服务端` |  |
| - [ ] | `d55144ca7` | feat(chat): support collapsible Markdown disclosures | `自身` |  |
| - [ ] | `b2058db3d` | feat(chat): collapse completed live activity with tool summaries | `自身` |  |
| - [ ] | `bce838ca7` | feat(chat): turn /btw into an isolated composer (#3398) | `自身` |  |
| - [ ] | `e246ad5ab` | feat(chat): tighten markdown typography and unify text selection colour | `自身` |  |
| - [ ] | `1f128ab20` | feat(chat): persist more composer input history with global or session scope; recallable with up/down arrow keys (#3035) | `服务端` |  |
| - [ ] | `65683c8db` | feat(ui): add composer enter-to-send toggle (#3178) | `引擎耦合?` `服务端` |  |
| - [ ] | `6d4794595` | feat(chat): remember JSON output view mode (#3072) | `服务端` |  |
| - [ ] | `a2ff36d43` | feat(chat): Comments and review in VS Code, like the OpenChamber desktop app (#1724) | `自身` |  |
| - [ ] | `953f2f415` | feat(queue): queue a message with everything the composer had attached | `服务端` |  |

## - [ ] Comments 评论 — 🟡 中价值 / 半独立

纯 UI 评价/评论栈;地基与 fork 重写的 composer/queue 互锁(已试织入后回滚)。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [ ] | `c06a9f53d` | feat(chat): highlight commented reply fragments with an editable comment popover | `自身` |  |
| - [ ] | `499700ab1` | feat(chat): merge mobile comment composer (#3733) | `自身` |  |
| - [ ] | `dca0239a3` | feat(ui): replace mobile comment overlay with composer comment mode | `自身` |  |

## - [ ] Stats/Usage/Quota — 🟡 中价值 / 部分引擎依赖

用量统计页直接读 OC2 会话统计(v2 SDK);quota providers 是自身能力(server-rs 移植)。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [ ] | `4e3b82c82` | feat(stats): add a usage stats page from OpenCode's session stats | `v2 SDK` | SKIP(v2 SDK) |
| - [x] | `d716d7861` | feat(usage): integrate ClinePass quota provider (#3431) | `服务端` | PARTIAL(vscode/UI 侧落地;server-rs 无 quota 子系统) |
| - [x] | `f70f18150` | feat(usage): add ClinePass quota provider | `服务端` | PARTIAL(UI/vscode 侧落地;server-rs 无 quota provider 子系统,JS 参考保留) |
| - [ ] | `2dfd1190e` | feat(work-status): add opt-in turn statistics (#3177) | `引擎耦合?` `服务端` |  |
| - [x] | `fbdd80fa2` | feat: add Charm Hyper quota provider (#3368) | `服务端` | PARTIAL(UI/vscode 侧落地;server-rs 无 quota provider 子系统) |
| - [x] | `a189fb43f` | feat: add exe.dev usage tracking | `服务端` | PARTIAL(UI/vscode 侧落地;server-rs 无 quota 子系统) |
| - [x] | `7f3b94652` | feat: add exe.dev provider logo | `自身` | PORTED(干净落地) |

## - [ ] Routing/模型选择 — 🟡 中价值 / 部分引擎依赖

按任务路由与 Auto 依赖 OC2 模型元数据的部分需甄别;纯 UI 选择器部分自身。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [ ] | `0b936476e` | feat(routing): answer Jev through OpenRouter and Vercel AI Gateway | `服务端` |  |
| - [ ] | `9aeb6fe93` | feat(providers): configure reasoning levels for custom provider models | `服务端` |  |
| - [ ] | `800b2aee9` | feat(providers): sign in to OpenCode Go through the Console | `自身` |  |
| - [ ] | `48ce6028d` | feat(routing): keep Auto across restarts and offer it as the session default | `引擎耦合?` `服务端` |  |
| - [ ] | `ec95fe2e0` | feat(routing): route prompts to a model per task and hold risky auto-accepted actions | `引擎耦合?` `服务端` |  |

## - [ ] 小模型 Jev — 🟡 中价值 / 需对齐

上游小模型基建;fork 已有 small_model 模块,Jev/OpenRouter 网关思路可借鉴,1 提交 v2 SDK。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [ ] | `fae74f78d` | feat(goal): check goal progress with Jev, the small model as fallback | `服务端` |  |
| - [ ] | `ca8ae6972` | feat(small-model): allow Claude Code and retry while plugins load | `v2 SDK` `服务端` | SKIP(v2 SDK) |

## - [ ] MCP/Plugins — 🟡 中价值 / 部分引擎依赖

MCP 重连为受管 OC 能力;插件加载状态展示 1 提交 v2 SDK。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [ ] | `c841fe2ad` | feat(mcp): let OpenCode decide Code Mode by default | `自身` |  |
| - [ ] | `0a6198389` | feat(plugins): show OpenCode load status and update unpinned plugins | `v2 SDK` | SKIP(v2 SDK) |
| - [ ] | `aa13c9811` | feat(mcp): reconnect failed MCP servers in managed OpenCode | `服务端` |  |

## - [x] 权限/安全 — 🟡 中价值 / 引擎+服务端

ask/safety-net/accept-all 三态与分类 provider——v2 SDK + 服务端,需对齐 fork 的 permission_auto_accept。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [ ] | `1bc709ed0` | feat(permissions): ask / safety net / accept-all modes and classification providers | `v2 SDK` `服务端` | SKIP(v2 SDK) |

## - [x] Websearch — 🟡 中价值 / v2 SDK

结果卡片与设置——1 提交 v2 import,需换绑 v1 wire。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [ ] | `69f2a3252` | feat(websearch): show results as cards and add web search settings | `v2 SDK` `服务端` | SKIP(v2 SDK) |

## - [ ] 通知/浏览器 — 🟡 中价值 / 依赖扩展平台

插件/代理通知与按 id 寻址浏览器——无扩展平台则为空转。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [ ] | `c0fb97ac1` | feat(notifications): let plugins and agents notify the user | `引擎耦合?` `服务端` |  |
| - [ ] | `1edf908ce` | feat(browser): let agents address browser tabs by id | `服务端` |  |

## - [x] Spaces 隔离空间 — 🔴 大工程 / OC2 时代设计

整套 Docker 隔离工作区子系统(stage 3a→5d),服务端 socket 转发/gatekeeper 全新架构;上游为 OC2 会话模型设计,fork 需整体重新评估。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [ ] | `117e45456` | feat(spaces): access to a space, with its blocked attempts (stage 5d-1) (#4076) | `服务端` | SKIP(平台级,另立项) |
| - [ ] | `211a5e713` | feat(spaces): the create flow, with the live switch in Settings (stage 5c) (#4069) | `服务端` | SKIP(平台级,另立项) |
| - [ ] | `102bff7e9` | feat(spaces): grants on the server, with the window bound to the inner network (stage 5b) (#4011) | `服务端` | SKIP(平台级,另立项) |
| - [ ] | `e0ee886e8` | feat(spaces): the journey's server routes and the live switch (stage 5a) (#4007) | `服务端` | SKIP(平台级,另立项) |
| - [ ] | `1290fd121` | feat(spaces): the space in the sidebar, chat through the prefix, the settings switch (stage 4c) (#4005) | `服务端` | SKIP(平台级,另立项) |
| - [ ] | `d67dcca2d` | feat(spaces): the merged session list, space events and socket forwarding (stage 4b) (#3999) | `引擎耦合?` `服务端` | SKIP(平台级,另立项) |
| - [ ] | `f9d212f38` | feat(spaces): the feature switch and the dispatcher (stage 4a) (#3995) | `引擎耦合?` `服务端` | SKIP(平台级,另立项) |
| - [ ] | `ccad7ec55` | feat(spaces): bring a space's work out and apply it (stage 3b) (#3991) | `服务端` | SKIP(平台级,另立项) |
| - [ ] | `f25d6f503` | feat(spaces): bring a project's code into a space (stage 3a) (#3833) | `服务端` | SKIP(平台级,另立项) |
| - [ ] | `45ff8c648` | feat(spaces): a gatekeeper as a space's only way out (#3790) | `服务端` | SKIP(平台级,另立项) |
| - [ ] | `918eb9300` | feat(spaces): tools volume and the server inside a space (#3771) | `服务端` | SKIP(平台级,另立项) |
| - [ ] | `66b42b32c` | feat(spaces): unwired Docker place and space manager, with the isolated spaces design (#3749) | `服务端` | SKIP(平台级,另立项) |

## - [x] Extensions/SDK 平台 — 🔴 大工程 / 平台级

第三方扩展平台 + @openchamber/sdk + 页面/存储/后台动作;2 提交直接用 v2 SDK,且与 fork 的 rsbuild/server-rs 均需适配。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [ ] | `c8ce4565b` | feat(extensions): file editors in the SDK, Excalidraw moves to an extension | `v2 SDK` `服务端` | SKIP(平台级,另立项) |
| - [ ] | `e348013ca` | feat(sdk): identify surface viewers to services and let docked pages resize | `服务端` | SKIP(平台级,另立项) |
| - [ ] | `6c5a1ff1b` | feat(sdk): let extensions add a section to the Work Status panel | `v2 SDK` `服务端` | SKIP(平台级,另立项) |
| - [ ] | `959d179c6` | feat(sdk): scope browser provider calls and dock a page beside a shared surface | `服务端` | SKIP(平台级,另立项) |
| - [ ] | `5883d28af` | feat(sdk): browser provider role and shared surface for extension services (#3734) | `服务端` | SKIP(平台级,另立项) |
| - [ ] | `4adb70398` | feat(sdk): separate background entry from visible panels | `服务端` | SKIP(平台级,另立项) |
| - [ ] | `ecad0a2db` | feat(sdk): support background actions and interactive toasts | `服务端` | SKIP(平台级,另立项) |
| - [ ] | `9df61f0f3` | feat(sdk): turn examples into an interactive extension gallery | `服务端` | SKIP(平台级,另立项) |
| - [ ] | `b59ab5671` | feat(extensions): add built-in infrastructure and refresh SDK examples | `服务端` | SKIP(平台级,另立项) |
| - [ ] | `201a5c83a` | feat(extensions): support SSH identities and relay asset loading | `服务端` | SKIP(平台级,另立项) |
| - [ ] | `e0cb68fc6` | feat(extensions): add pages, storage, and workspace session access | `服务端` | SKIP(平台级,另立项) |
| - [ ] | `5181bcd33` | feat(extensions): add third-party extensions and @openchamber/sdk (#3460) | `服务端` | SKIP(平台级,另立项) |

## - [x] OpenCode 2 迁移 — ⚫ OC2 依赖(不可用)

引擎层迁移与会话元数据迁移,pi 引擎无对应概念。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [ ] | `461f91452` | feat(opencode): move session metadata onto OpenCode 2.0.15 | `v2 SDK` `服务端` | SKIP(OC2 引擎,pi 引擎无对应) |
| - [ ] | `654705f7d` | feat: move OpenChamber to OpenCode 2.x (#3837) | `v2 SDK` `服务端` | SKIP(OC2 引擎,pi 引擎无对应) |

## - [ ] 其他 — 🟡 杂项 / 逐条判定

键盘布局、输入历史、changelog 基建、exe.dev 追踪等——多为自身能力,少数 v2。

| 吸收 | hash | 提交 | 依赖 | 状态 |
|---|---|---|---|---|
| - [ ] | `18ca96969` | feat(ui): expand stats with token composition, efficiency and tool calls (#4026) | `v2 SDK` | SKIP(v2 SDK) |
| - [ ] | `a30029f90` | feat: add Excalidraw editor support and file integration (#4064) | `v2 SDK` | SKIP(v2 SDK) |
| - [ ] | `220d7e0ae` | feat(settings): bring back the Claude Code integration card | `自身` |  |
| - [ ] | `f42715c25` | feat(ui): lay out the keyboard shortcuts dialog in two columns | `自身` |  |
| - [ ] | `4e5f26d07` | feat(settings): card grids for MCP and plugins, list search, compact model rows on mobile | `v2 SDK` | SKIP(v2 SDK) |
| - [ ] | `60d836c48` | feat(settings): redesign providers as a card grid with account switching | `v2 SDK` | SKIP(v2 SDK) |
| - [ ] | `15e51be9e` | feat(settings): add a manual Restart OpenCode action | `自身` |  |
| - [ ] | `fe2ec474f` | feat(about): separate OpenChamber and OpenCode updates on About | `服务端` |  |
| - [ ] | `8dcc61279` | feat(ui): add /fork, runtime hints for plugin agents, and About in the mobile app | `自身` |  |
| - [ ] | `f99fe1fd6` | feat(memory): tell every session when to save and deliver it everywhere | `引擎耦合?` `服务端` |  |
| - [ ] | `0c4fbe362` | feat: add start session action to empty sidebar groups | `自身` |  |
| - [ ] | `fafbd50bc` | feat(settings): searchable theme picker, tighter dropdowns, stable scroll gutter | `自身` |  |
| - [ ] | `83ec4fbde` | feat(ui): add opt-in session activity spinners (#3459) | `引擎耦合?` |  |
| - [ ] | `449d9b42a` | feat(sync): evict idle session histories whole instead of trimming turns | `自身` |  |
| - [ ] | `921f764c8` | feat(changelog): add SDK group for extension author notes | `服务端` |  |
| - [ ] | `85b20dbf0` | feat(issues): gate intake and triage decisions (#3588) | `引擎耦合?` |  |
| - [ ] | `8ee477207` | feat: swap sidebar activity header when sticky | `自身` |  |
| - [ ] | `819b2b218` | feat(relay): report app metadata on connections | `服务端` |  |
| - [ ] | `b65bed1f0` | feat: preserve scroll and cursor positions across file preview views | `自身` |  |
| - [x] | `26b54dbe3` | feat(ui): unify change comparisons and compact message actions | `服务端` | PORTED(服务端移植 server-rs:commit-diff/commit-files/工作区区间对比) |
| - [ ] | `8dce7ae6b` | feat: enable turn stats by default and prepare release notes | `引擎耦合?` |  |
| - [ ] | `73040d71e` | feat: add search to the new session project picker (#3408) | `自身` |  |
| - [ ] | `1b1421a59` | feat(chats): relocate managed chat worktrees via OPENCHAMBER_CHATS_DIR (#3135) | `服务端` |  |
| - [ ] | `d8060b9d2` | feat(changelog): one source file per release, generated outputs | `引擎耦合?` |  |
| - [ ] | `be7ebd446` | feat: streamline release preparation and changelog checks | `自身` |  |
| - [ ] | `9fa6cdc23` | feat(settings): retire the third-party plugin integrations | `自身` |  |
| - [ ] | `04d64fa30` | feat(queue): deliver queued messages from the server | `服务端` |  |
