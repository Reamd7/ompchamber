# 终端能力差距清单（vs Orca）——按本项目定位排优先级

> 基线：2026-08-25 会话调研 `references/orca-terminal-research.md` + 本 worktree HEAD `f140c82c`
> fork（`references/ghostty-web`，7 提交）已实现：鼠标上报 parity、focus 1004、触摸手势、Orca 式排版、font-bbox 基线

## 本项目定位与优先级依据

OpenChamber 是 **AI 编程会话的伴随终端**（ContextPanel rail 的一等 surface、非独立终端产品），
多端面（web / Electron 桌面 / mobile drawer）共享同一 server runtime，且 WS 协议已复用
openRuntimeWebSocket（auth/relay/proxy 全走通）。排优先级的三条依据：

1. **日常高频可见** > 基础设施改造（用户每周感知 vs 工程投入）
2. **已有基建可搭便车** > 需从零建（unread 通知、mobile drawer、relay 传输已在）
3. **server-centric 架构**下"远端"已由部署模型天然覆盖（连到远端 server 即远端终端），
   不需要 Orca 那种 desktop→SSH 的专用 relay 腿

## 已达成 parity 或反超（无需再动）

| 能力 | 状态 |
|---|---|
| 鼠标协议（SGR 1006 / urxvt 1015 / UTF-8 1005 / X10、motion、滚轮刻度、shift 旁路） | ✅ 与 xterm.js 字节级验证（btop/lazygit/fresh 实测） |
| kitty keyboard 状态 | ✅ 结构性优势：WASM 内核即权威（`getMode` 直查），无需 Orca 的镜像状态机 |
| focus 上报（mode 1004） | ✅ `\e[I`/`\e[O` |
| 触摸→TUI 鼠标（点按/拖动/双指滚轮） | ✅ Orca 没有的能力 |
| 字体排版（300/500/1.2 行高） | ✅ |
| 查询应答（OSC10/11、2031+997、DA1 兜底、COLORFGBG） | ✅ PTY 层单点权威，vim 实测跟随明暗 |
| 模拟器内核正确性（grapheme/复杂文字/XTPUSHSGR） | ✅ 真 Ghostty WASM，按构造正确 |

---

## P0 —— 高频可见，基建已就位

### 分屏（splits）

- **为什么 P0**：开发者日常最直观的缺失能力；每次想"一边看日志一边跑命令"就痛
- **现状**：仅 tab，无 pane 分屏
- **搭的便车**：无——纯渲染层新功能
- **Orca 参考**：两层模型——tab group 级 React 声明式（`TabGroupLayoutNode` zod 持久化）+ pane 级命令式 DOM（`PaneManager` + `pane-tree-ops` reparent）；拖拽直写 style 不进 store
- **落点**：`packages/ui/src/components/terminal/` 扩展；`TerminalViewport` 的容器从单实例变树
- **注意**：mobile drawer 布局兼容（竖屏优先单 pane，横屏可分）
- **规模**：中——渲染层改造为主，server 无改动（多个 viewport attach 同一 PTY 已支持）

### Shell 集成注入（OSC 133）

- **为什么 P0**：解锁命令边界检测 → 喂给**已有的** `unreadTerminalPanes` / `AgentCompletionPanes` 通知基建，
  让终端从"哑管道"变成"能告诉用户命令跑完了"的智能伴随——正是本项目"AI 伴随终端"定位的核心增值
- **现状**：无注入；shell 靠自身
- **搭的便车**：unread/通知 store 已有（`useTerminalStore`）；只需喂事件
- **Orca 参考**：zsh 单 `.zshenv` 包装（ZDOTDIR 早归还 + OSC 777 ready marker 走 zle-line-init）；
  bash ≥5.1 PROMPT_COMMAND 数组 / 4.4-5.0 标量分发 + trap DEBUG；PowerShell `-EncodedCommand`；fish `--init-command`
- **落点**：`packages/web/server/lib/terminal/` 新增 shell-integration 模块（spawn 时包装 argv）；
  输出侧复用已有的 WS 事件流（新增 `command-finished` 事件类型）
- **关键坑**：`#11044` HISTFILE 落错目录——用"ZDOTDIR 早归还"形，不霸占
- **规模**：小——server 侧包装 + 一个新事件类型 + UI 消费

---

## P1 —— 可靠性，真实使用下会痛

### scrollback 磁盘持久化 + 冷恢复

- **为什么 P1**：server 重启（dev 迭代、Electron 崩溃恢复）丢全部终端历史——开发者每天碰
- **现状**：内存 512KiB（`runtime.js`），重启即丢
- **搭的便车**：`WorkspaceSessionState` 已持久化 tab 骨架（`partialize` 已做引用稳定性短路），
  只缺 scrollback 内容本身的落盘
- **Orca 参考**：OCKL 帧日志（`'OCKL'` magic + batch/output/resize/clear 帧，seq 连续性校验）+
  `checkpoint.json` 全量快照（headless 模拟器序列化）+ tombstone 删除 + generation 指纹
- **落点**：`packages/web/server/lib/terminal/` 新增 history-persistence 模块
- **关键设计**：高频增量 append + 低频全量（冷却）+ tombstone；内容寻址 ref 已有模式可抄
  （workspace scrollback 的 `v1-<sha256[:32]>.bin`）
- **规模**：中——纯 server 侧，不改协议（snapshot 帧已存在，补数据源）

### 多端流控精装

- **为什么 P1**：mobile 是一等面（`MobileWorkspaceDrawer` 挂 `TerminalView`）；无流控下慢链路 +
  大输出（`cargo build` / `npm install`）会撑爆 mobile WS 或丢帧
- **现状**：WS fan-out 多连接可同看 + attach/snapshot 语义（协议地基已有）
- **缺**：per-stream ACK 窗（512KB→2MB）、连接级总窗（2MB→8MB）、宽度仲裁、input floor、
  seq-gap 自愈（丢帧→关 live 路径拉全量恢复快照）
- **搭的便车**：WS 协议的 seq 字段已存在（`output` 帧带 `q`），只需消费端 gap 检测 + 恢复逻辑
- **Orca 参考**：`terminal-multiplex-flow-control.ts`（常量表）+ `TerminalMultiplexStream`
- **规模**：中——协议扩展（新 opcode）+ 渲染端自愈

---

## P2 —— 基础设施，投入大且依赖部署场景

### PTY 存活于 app 生命周期外（daemon 模式）

- **为什么 P2**：仅在 Electron 桌面频繁崩溃/重启时痛；web 部署下 server 本身长驻
- **现状**：PTY 活在 web server 进程内
- **依赖**：P1 scrollback 持久化（daemon 的前提——无盘历史则 daemon 恢复无意义）
- **Orca 参考**：NDJSON 双 socket（control + stream）；Windows kill-on-close job 放 daemon
- **规模**：大——新进程模型 + 会话 adoption + Electron 接线

### Ghostty config/主题导入

- **为什么 P2**：onboarding 打磨（"从 Ghostty 迁移"场景），有感知但非刚需
- **搭的便车**：settings 体系 + 主题管道已有；只需发现/解析/映射
- **Orca 参考**：discovery → parser → theme-resolution → mapper → 只读预览 IPC
- **规模**：小——纯 server 侧 + settings UI 一个 modal

---

## P3 —— 长尾，按需

### SSH 远端终端

- **为什么 P3**：OpenChamber 的 server-centric 架构已天然覆盖"远端"场景（连到远端 server =
  远端终端，WS 经 relay/auth 全走通）；Orca 那种 desktop→SSH 专用腿对我们是**架构错配**
- **如果真要**：用户场景是"桌面 app 直连一台不在跑 OpenChamber server 的机器"——这是
  新产品决策而非功能补齐
- **规模**：大——新传输层 + PTY 代理

### IME 事务深度

- **为什么 P3**：ghostty-web 上游基础 IME + 我们的 Android beforeinput 转发够用；
  重度 CJK 输入者才痛；上游可能自己修
- **规模**：大（700 行补丁级别）

---

## 遗留小项

- [ ] `theme-response.js` 补 kitty 键盘查询 `CSI ? u` 应答（zellij 类程序会等）
- [ ] zellij 真机验证（WSL `~/bin/zellij` 已装，ConPTY 嵌套下未跑通）

---

## 移动端终端专项差距（2026-08-25 二次深挖补充）

### 手势能力差距（Orca WebView 注入层 vs TerminalViewport.tsx）

| # | 能力 | Orca 实现 | 我们现状 | 缺口 |
|---|---|---|---|---|
| G1 | **pinch 缩放改字号** | 双层模型：手势期间 CSS `userScale` 瞬时缩放，松手 snap 到 `TERMINAL_TEXT_SCALES=[0.5,0.75,1,1.25,1.5,2]` 预设并真正改 fontSize → 网格 reflow → PTY resize → 持久化 | **完全缺失** | 🔴 高价值 |
| G2 | **惯性滚动 momentum** | touchmove 速度混合采样（`velY = velY*0.55 + instant*0.45`），松手摩擦衰减（FRICTION=0.972, MIN_VEL=0.012），新触点即停 | 松手即停 | 🟡 体验 |
| G3 | **alt-screen 无鼠标追踪时滚动→箭头键** | `shouldRouteScrollToTerminalInput = wheelTracking || altScreen`；箭头序列尊重 `applicationCursorKeysMode`（SS3 `ESC O A/B`） | 仅 `hasMouseTracking()` 走 app 模式；alt-screen TUI 无鼠标时触摸滚动无效果 | 🔴 功能缺失 |
| G4 | **tap/长按分离阈值** | tap SLOP=24px + 700ms 窗口；长按 SLOP=10px + 500ms | 单一 8px 阈值 | 🟡 精度 |
| G5 | **选择把手 + 边缘自动滚动** | 44px 把手拖拽、边缘 40px 区触发 60ms 自动滚 1 行、边界 haptic | 依赖 xterm 原生选择（无把手、无边缘滚） | 🟡 体验 |
| G6 | **tap 路由优先级** | OSC8 file > file:// > OSC8 url > 正则 URL > 磁盘路径 > TUI 鼠标点击 > 聚焦；宽字符列换算 | 只有"聚焦 + TUI mousedown" | 🔴 功能缺失 |
| G7 | **触觉反馈** | 4 种 kind（selection/edge-bump/pinch-snap/gesture-start），Android HapticFeedbackConstants / iOS expo haptics | 无 | 🟡 体验 |
| G8 | **rAF 合帧滚动** | 密集行重绘合并为每帧一次行滚 | 每 pointermove 直接 scrollLines | 🟡 性能 |
| G9 | **内容超宽水平 pan** | `clampPan` 钳制，内容不足钉左上角 | PTY resize 适配宽度（无溢出场景） | 架构不同，暂不需要 |

### 快捷键模拟输入差距（Orca accessory bar vs terminalInput.ts）

| # | 能力 | Orca | 我们现状 | 缺口 |
|---|---|---|---|---|
| K1 | **内置键数量** | 20 个（Esc/Tab/Enter/arrows + F1-F12/Home/End/PgUp/PgDn/Ins/Del/Backspace/Space） | 7 个（Esc/Tab/Enter/4 方向） | 🔴 覆盖面 |
| K2 | **修饰键组合** | Ctrl/Alt/Shift 任意组合，`csiModifierParameter` 算法生成 CSI 修饰参数 | ctrl/alt 单修饰仅用于方向键 | 🔴 组合能力 |
| K3 | **Ctrl+符号控制字节** | Ctrl+A-Z 全覆盖 + Ctrl+符号（cprint 映射） | Ctrl+A-Z 有（`& 0b11111`）但无符号 | 🟡 |
| K4 | **F1-F12** | SS3 编码（`ESC OP`-`ESC OS` + CSI 15~24~） | 无 | 🔴 |
| K5 | **Home/End/PgUp/PgDn/Ins/Del** | CSI 编码（`ESC[H/F`、`ESC[5~/6~`、`ESC[2~/3~`） | 无 | 🔴 |
| K6 | **长按重复** | 400ms 触发 + 45ms 间隔 | 无 | 🟡 效率 |
| K7 | **自定义键 / 宏** | CustomKey{id,label,bytes,enter} 三步向导（快捷组合/特殊键/文本宏），AsyncStorage 持久化 | 无 | 🟡 高级 |
| K8 | **快捷条布局定制** | orderedBuiltInIds/visibleBuiltInIds，拖拽排序 + 显隐开关 + 重置默认 | 固定 7 键，无定制 | 🟡 |
| K9 | **Alt+Enter 换行** | 括号粘贴模式下 CSI-u `\e[13;2u`，否则 `\e\r` | 无 | 🟡 |
| K10 | **Ctrl+方向行为修正** | 字导航（readline 兼容 `\e[1;5C/D`） | 我们发 `\e[1;5C` 但 readline 不识别 CSI 形式（需 SS3 或裸 ESC 前缀） | 🔴 行为 bug |

### 优先级补充（并入主优先级表）

| 并入 | 项 | 理由 | 规模 |
|---|---|---|---|
| **P0** | G3 alt-screen 箭头回退 + K10 Ctrl+方向 bug | 功能性缺失/bug，TerminalViewport 手势机 + terminalInput.ts 几行 | 小 |
| **P0** | K1+K2 快捷键覆盖 + 修饰组合 | 高频日常，扩展 terminalInput.ts 编码表 | 小 |
| **P1** | G1 pinch 缩放 | mobile 核心体验，需字号设置联动 + PTY resize | 中 |
| **P1** | G6 tap 路由 | 链接/路径点击高频 | 中 |
| **P2** | G2 惯性 + G4 阈值分离 + G5 把手 + G7 haptic + G8 rAF | 体验打磨包 | 中 |
| **P2** | K4+K5 功能/导航键 + K6 长按重复 + K7 自定义键 + K8 布局定制 | 快捷键进阶包 | 中 |

### 多设备同步 + 弱网（2026-08-25 三次补充）

| # | 能力 | Orca 实现 | 我们现状 | 缺口 |
|---|---|---|---|---|
| S1 | **多端同看一个终端** | `terminal.multiplex` streaming RPC：每连接 128 条流，每流独立 ACK 窗（512KB→2MB），连接级总窗（2MB→8MB）round-robin 排空 | WS fan-out 多连接可 attach 同一终端（协议地基已有），无 per-stream 流控 | 🟡 有地基缺精装 |
| S2 | **宽度仲裁** | DriverState：mobile 订阅即驱动（隐式）；desktop 经 `ClaimViewport` 帧声明；订阅键按 `connectionId:streamId` 防互踩 | 任何一端 resize 即改共享 PTY 尺寸，所有端被动跟随 | 🔴 无仲裁 |
| S3 | **输入地板（input floor）** | mobile 输入持锁（`beginMobileInputFloor`），期间其他端 Input 被拒（回 `WriteUnavailable` 帧），松手释放 | 多端 write 自由竞争，无互斥 | 🔴 无仲裁 |
| S4 | **快照对账元数据** | SnapshotStart 带 `seq/truncated/unavailable/pendingEscapeTailAnsi/kittyKeyboardFlags` 对账字段；渲染端 seq gap= 丢帧 → 关 live 路径拉全量恢复快照自愈（退避 500ms→5s） | snapshot 帧有 `q` 但渲染端不检测 gap、不自愈 | 🔴 无自愈 |
| S5 | **seq-gap 丢帧自愈** | 上同：检测到 seq 跳变即关 live 路径、无 requestId 恢复快照全量重置、退避重试 | 无 gap 检测；丢帧=静默渲染缺失 | 🔴 |
| S6 | **弱网退避重连** | 指数退避 500ms→5s；隐藏/离线固定 60s + `visibilitychange/online` 即时唤醒；对订阅者发 `reconnecting` 事件（UI 显示状态） | 指数退避 500ms→8s；隐藏/离线 60s + visibility/online 唤醒（已有）；无 `reconnecting` 事件（UI 不知断线） | 🟡 部分已有 |
| S7 | **空闲 socket 保活** | 最后订阅者退订后 15s 宽限（防 tab 切换的全量 snapshot 回放），复用而非关 socket | **已有**（`IDLE_SOCKET_GRACE_MS=15s`） | ✅ parity |
| S8 | **快照不可用分类** | `unavailable` 区分"现在答不了"（retry-worthy）vs"真空 buffer"（permanent）vs unknown-legacy；渲染端据此决定重试或放弃 | 无分类；快照失败一律当空 | 🟡 |
| S9 | **查询应答权威让位** | 多路流喂带查询权威的远程 xterm 时，主 responder 让位（`registerRemoteTerminalViewSubscriber`）；detach 才恢复 | 单点 PTY 层应答，无多路让位概念 | 🟡（单主机架构下暂不需要） |
| S10 | **移动端 resize 全缓冲重放** | 移动端 resize 重序列化全缓冲重放（Why：mobile xterm 无法重排快照烤死的硬换行） | resize 后靠 scrollback replay，无全缓冲重序列化 | 🟡 |

#### 优先级（并入主表）

| 并入 | 项 | 理由 | 规模 |
|---|---|---|---|
| **P1**（并入现有"多端流控精装"） | S4+S5 seq-gap 自愈 + S2/S3 宽度/输入仲裁 | mobile 一等面 + 弱网真实场景；协议 seq 已有，补消费端 gap 检测 + 恢复逻辑 | 中 |
| **P1** | S6 `reconnecting` 事件 | 几行改动，用户体验大提升（知道断线 vs 看起来卡死） | 极小 |
| **P2** | S8 快照不可用分类 + S10 移动端 resize 重放 | 长尾打磨 | 小-中 |
| 不动 | S9 查询应答让位 | 单 PTY 主机架构下无场景 | — |
