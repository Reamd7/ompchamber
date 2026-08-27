# 终端增强实施计划（第一批：最小投入 × 最高价值）

> 从 TODO-terminal-gap.md 挑选：每项 ≤ 半天工作量，均为 P0/P1 级差距
> 前置条件：无（全部基于当前 HEAD，无依赖链）

## 批次总览

| # | 项 | 差距编号 | 规模 | 价值 |
|---|---|---|---|---|
| 1 | Ctrl+方向键行为修正 | K10 | ~10 行 | 🔴 修 bug |
| 2 | alt-screen 滚动→箭头回退 | G3 | ~15 行 | 🔴 功能缺失 |
| 3 | 快捷键覆盖 + 修饰组合 | K1+K2 | ~80 行 | 🔴 高频日常 |
| 4 | 断线重连状态事件 | S6 | ~10 行 | 🔴 UX |
| 5 | Shell 集成注入 OSC 133 | — | ~150 行 | 🔴 解锁通知 |

---

## 1. Ctrl+方向键行为修正（K10）

**问题**：`terminalInput.ts` 发 `\e[1;5C/D`（CSI 形式），readline/bash/zsh 的字导航期望
裸 `\e[1;5C` 但**不识别**——正确做法是发 CSI 带 5 修饰参数或 SS3 前缀，取决于 terminal mode。

**改动**：`packages/ui/src/lib/terminalInput.ts`
- Ctrl+Right/Left: 保持 `\e[1;5C/D`（**这是对的**——CSI modifier 形式是 xterm 标准，readline 默认绑定识别）
- **验证**：在 bash 里实测 Ctrl+Left/Right 是否跳词（如果不行才是真 bug）

**验证**：bash 中 `echo "hello world foo"` 输入后按 Ctrl+Left → 光标应跳到 `foo` 前面

---

## 2. Alt-screen 滚动→箭头回退（G3）

**问题**：vim/less 无鼠标追踪时（`hasMouseTracking()=false` 但 `isAlternateScreen()=true`），
触摸滚动调 `scrollLines()`——在 alt buffer 无效果。应发箭头键。

**改动**：`packages/ui/src/components/terminal/TerminalViewport.tsx` 手势机

```
现有: if (terminal.hasMouseTracking()) → app-press 模式
新增: else if (terminal.buffer.active.type === 'alternate') → app-scroll 模式
        单指拖动 → 按 scrollLines(lines) 数量发 \e[A / \e[B 箭头序列
        （需查 applicationCursorKeysMode：如为 true 用 \eOA / \eOB）
```

**Orca 参考**：`shouldRouteScrollToTerminalInput = wheelMouseTracking || altScreen`；
箭头序列 `terminal-accessory-keys.ts` 中的 SS3 变体

**验证**：vim 无 mouse 模式下触摸上下滚动 → vim 视口滚动

---

## 3. 快捷键覆盖 + 修饰组合（K1+K2）

**问题**：仅 7 键 + ctrl/alt 单修饰；缺 F1-F12、Home/End/PgUp/PgDn、Ctrl+任意键组合。

**改动**：重写 `packages/ui/src/lib/terminalInput.ts`（28 行 → ~100 行）

```ts
// 完整编码表
const SPECIAL_KEYS = {
  escape: '\x1b', tab: '\t', enter: '\r',
  up: '\x1b[A', down: '\x1b[B', right: '\x1b[C', left: '\x1b[D',
  home: '\x1b[H', end: '\x1b[F',
  pageUp: '\x1b[5~', pageDown: '\x1b[6~',
  insert: '\x1b[2~', delete: '\x1b[3~',
  backspace: '\x7f', space: ' ',
  f1: '\x1bOP', f2: '\x1bOQ', f3: '\x1bOR', f4: '\x1bOS',
  f5: '\x1b[15~', f6: '\x1b[17~', f7: '\x1b[18~', f8: '\x1b[19~',
  f9: '\x1b[20~', f10: '\x1b[21~', f11: '\x1b[23~', f12: '\x1b[24~',
};

// 修饰参数（CSI modifier parameter）
const MOD_PARAMS = { shift: 1, alt: 2, altShift: 3, ctrl: 5, ctrlShift: 6, ctrlAlt: 7, ctrlAltShift: 8 };

// 编码器：特殊键 + 修饰 → 带修饰的 CSI/SS3 序列
// 可打印键 + ctrl → & 0x1f 控制字节
// 可打印键 + alt → \x1b + 字符
export function encodeKey(key: string, mods: { ctrl?: boolean; alt?: boolean; shift?: boolean }): string
```

UI 侧：`TerminalView.tsx` 快捷条从固定 7 键改为可横向滚动条（`overflow-x-auto`），
新增 modifier toggle（Ctrl / Alt / Shift 三态按钮，sticky）。

**验证**：快捷条按 F5 → TUI 收到 `\e[15~`；Ctrl+Shift+C → 收到 `\e[15;6~` 或合理等价

---

## 4. 断线重连状态事件（S6）

**问题**：WS 断线后 UI 无反馈（用户不知道是卡了还是断了）。

**改动**：`packages/ui/src/lib/terminalApi.ts` 的 `scheduleReconnect` 已有重连逻辑，
补对 subscriber 发 `{type:'reconnecting', attempt, maxAttempts}` 事件（实际已有！——查代码确认）。

如果已有 `reconnecting` 事件：只需在 `TerminalView.tsx` 消费它并显示状态条（"重连中…"/断线提示）。

**验证**：devtools Network 断开 WS → 终端上方显示"重连中"横条

---

## 5. Shell 集成注入 OSC 133（解锁通知）

**问题**：无命令边界检测，终端是哑管道——用户不知道命令什么时候跑完。

**改动**：`packages/web/server/lib/terminal/` 新增 `shell-integration.js` + `runtime.js` 接线

```
spawnPty 时：
  POSIX:
    zsh:  env ZDOTDIR=<wrapper-dir>; wrapper .zshenv 内容:
          - ZDOTDIR 早归还（防 HISTFILE 落错）
          - precmd: printf '\e]133;D;%s\a' "$?"（仅命令中）
          - preexec: printf '\e]133;C\a'
          - ready marker: OSC 777 orca-shell-start（走 zle-line-init 而非 azhw）
    bash: --rcfile <wrapper>（≥5.1 数组 / 4.4-5.0 标量分发 + trap DEBUG）
    pwsh: -EncodedCommand <base64 OSC133 bootstrap>
  输出侧:
    runtime.js 新增 OSC 133 扫描器 → {type:'command-finished', exitCode} WS 事件
  UI 侧:
    TerminalView 消费 command-finished → 已有 unreadTerminalPanes 通知基建
```

**Orca 参考**：`zsh-startup-wrapper-builder.ts`（ZDOTDIR 早归还 + deferred precmd 自替换）；
`terminal-osc133-command-finished.ts`（chunk 边界安全扫描，BEL/ST 双终止符）

**验证**：zsh 会话中 `sleep 3` → 3 秒后收到 `command-finished` 事件 → 通知/unread 标记

---

## 执行顺序

```
1 → 2 → 3 → 4 → 5
（1/2 是修 bug/补缺失，半天内；3 扩表纯 host 侧；4 可能已有只需 UI；
 5 最大但解锁通知能力——建议 1-4 一个 PR，5 单独 PR）
```

## fresh 补测

在 3 完成后（快捷条扩好），用 fresh 编辑器实测：
- [ ] 点击文件树定位光标（1002 button-motion）
- [ ] 拖动选中文本
- [ ] Ctrl+S/Ctrl+Q 等快捷键经扩展后的编码表
- [ ] 触摸双指滚动（fresh 开 1002 不开 1003，滚轮走 button 64/65）
