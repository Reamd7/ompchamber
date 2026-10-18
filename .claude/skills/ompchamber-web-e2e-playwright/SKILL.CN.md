# OpenChamber Web UI E2E（Playwright）— 中文镜像

> 本文件是 `SKILL.md` 的中文镜像，供维护者阅读；agent 以同目录英文 `SKILL.md` 为准，两者不一致时以英文版为准。

复现/验证 OpenChamber Web UI 行为的端到端流程（真模型回合、编号截图、录屏）。本 skill 是应用侧事实的 canonical owner：引擎结论、bootstrap 陷阱、dev 运行时、判据基线。方法论与工具 kit 见 `e2e-redgreen-playwright` skill。

## 浏览器引擎选择（先读）

- **用系统 Chromium**：`chromium.launch({ executablePath: CHROMIUM_BIN, headless: true })`——harness.mjs 自动探测常见系统路径（`CHROMIUM_BIN` 可覆盖）。
- **obscura 0.2.5 不能用于本应用的交互复现**（2026-10-18 实测）：应用外壳/DOM/网络链加载正常，但交互层完全无响应——点击/键盘前后 DOM 字节级相同、React 零 commit；同一实例上最小 React 19 应用交互正常（引擎对该应用启动路径的兼容缺口；证据见该会话 `references/e2e` REPORT §0）。仍要做静态冒烟时的 obscura 要点：`obscura serve --port 9223 --allow-private-network` + Playwright `connectOverCDP`；其 CDP `--v8-flags` 是全局参数必须放子命令前；Playwright 必须用 node 跑（Bun 的 WS 客户端握手超时）。
- 录屏需 Playwright 自带 ffmpeg：缺失时查 `node_modules/playwright-core/browsers.json` 的 ffmpeg revision，把系统 ffmpeg 软链到 `~/.cache/ms-playwright/ffmpeg-<rev>/ffmpeg-linux`（该软链曾静默丢失过——录屏失败先查它）。

## 运行时

1. 仓库根 `bun run dev` → UI http://127.0.0.1:5180，API http://127.0.0.1:3902（改 server 代码后必须重启）。
2. scratch 项目放**家目录一级**（如 `~/omp-e2e-proj`，内含 `.opencode/commands/*.md` 测试命令）：Add-project 对话框的目录树会过滤 gitignore 目录，放 `references/` 下选不到。

## bootstrap 陷阱（fresh context 每次都弹）

- 启动必弹 "Add project directory" 对话框（全屏遮挡，挡住所有点击）：逐级点 `[role=option]` 导航到 scratch 目录 → 点 "add project" 按钮（不是 "Add"）；已添加过时 add 是 no-op 且 Escape 常关不掉——循环兜底：Escape + 点 `[data-slot=dialog-close]` 直到消失。
- "New session" 按钮在 `aside` 内（不是行尾 hover 的 + 号）；选中命令=插入文本并立即提交。
- composer 是 `contenteditable` div，不是 textarea（`[role=textbox]` 定位）。
- 模式选择器是 composer 底部 "Default" 芯片；**草稿态禁用**（提示 "Start the session to change its mode"）——先发一条消息启动会话再切模式。
- plan 评审 overlay 与 ask 对话框渲染在 portal（`[role=dialog]`），**不在 `<main>` 里**：transcript 断言用 main.innerText 会漏，必须另查 `[role=dialog]`。
- ask 对话框是 radio 选项列表（非输入框）：点选项 label 再点 submit。
- 等回合结束：轮询 transcriptText 连续 3 次采样相同（不是固定 sleep）。

## 场景脚本与工具 kit

权威工具 kit 在 `e2e-redgreen-playwright` skill 的 `tools/`（harness.mjs：newRun 录屏+编号截图+settle+verdict；bootstrap.mjs：上述陷阱的代码化；tree.mjs：DOM 结构转储；react-app.mjs：引擎对照实验；scenario.example.mjs：指纹断言范本；fixtures/e2e-echo.md：marker 命令）。复制进 gitignored 工作目录后使用。`references/e2e/` 里 2026-10-18 会话的实例含全部场景，是 disposable 副本：red1-slash（斜杠命令通道，判据 `E2E-ECHO-OK [args]`）、red2-plan（plan 全链：计划→提案→overlay→approve→文件落地）、red3-ask（ask 双变体：正常 UI 流 vs 无头物化后挂 UI，API 先 POST /session + prompt_async）。Green 复跑：`RUN_NAME=green1-slash node red1-slash.mjs`。

## 判据基线

- 斜杠 Green：用户气泡=展开模板，回答 `E2E-ECHO-OK [hello world]`；Red：气泡退化为裸命令名+空参数。
- plan Green：NOTES.md 只在 approve 后落盘；Red：Plan 徽章下直接写文件。
- ask Green：双变体都弹框且答案回填；Red：BOTH-BROKEN（模型自述没有 ask 工具）。
