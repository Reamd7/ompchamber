# Playwright Red-Green 复现流水线（ompchamber）— 中文镜像

> 本文件是 `SKILL.md` 的中文镜像，供维护者阅读；agent 以同目录英文 `SKILL.md` 为准，两者不一致时以英文版为准。

2026-10-18 会话 01a14cf5 验证有效（三 bug 一次闭环）。工具 kit 权威位置：**本 skill 目录 `tools/`**（harness / bootstrap / tree / react-app / scenario.example / fixtures）。用法：**一任务一目录**——每个任务把 `tools/` 整 kit 复制进一个独立的 gitignored 目录（如 `references/e2e-<task-slug>/`，需可解析 `playwright-core`），fixture 命令植入 scratch 项目，场景脚本写在其旁。两条结构性保证由 harness.mjs 在 `newRun` 时强制：拒绝在 skill 原地运行（`.agents/skills`/`.claude/skills` 下），拒绝 artifacts 落在 git 仓库内但未 gitignore 的路径；每个 `newRun` 还各自拥有先清空的 `artifacts/<run-name>/` 目录，run 之间零共享。应用侧事实（浏览器引擎结论、bootstrap 陷阱、dev 运行时、判据基线）canonical 在 `ompchamber-web-e2e-playwright` skill。

## 流水线（顺序执行，勿跳步）

1. **根因先行**：先由并行只读 scout 拿到 file:line 级根因假设，再写任何复现脚本。Red 脚本 = 假设的编码，不是探索。
2. **指纹断言**：每个 bug 一个低成本可判定的指纹，判定收敛为单个 regex：
   - 数据通道类：植入 marker 命令（`fixtures/e2e-echo.md` → `E2E-ECHO-OK [args]` 模式），参数丢失一眼可见；
   - 门禁类：文件系统哨兵——目标文件在批准前不得存在 + overlay 文案存在；
   - 交互类：问题→作答→模型复述（transcript 往返断言）。
3. **harness 分层**：场景只写"步骤 + 指纹"，其余全用 harness.mjs：
   - `newRun(name, {engine})`：每次 run 独立 artifacts 目录、`shot(step, note)` 编号截图、同 context 录屏（finish 改名 `recording.webm`）、`settle(sample)` 稳定轮询（连续 3 次采样相同才算回合结束；禁止固定 sleep——LLM 回合时长不可预测）、`verdict(tag, ok)` 可 grep 判定常量。
   - 引擎是参数不是承诺：`engine: 'chromium' | 'obscura'`，切引擎零改写场景。
   - fixture scratch 项目放仓库外、家目录一级（Add-project 对话框目录树会过滤 gitignore 目录）。
   - Green 复跑 = `RUN_NAME=green-x node 同一脚本`；Red/Green 对照 = artifacts 目录对照。
4. **Red 判据校准**（Red 不是写完就完）：
   - 实际失败比预期更隐蔽时收紧判据：模型翻文件自猜出空参数 marker（`E2E-ECHO-OK []`）→ 判据改为"marker 必须带参数"；
   - UI 状态阻塞流程时重排步骤：模式选择器草稿态禁用 → 先发首条消息启动会话再切模式；
   - Red 面比假设广时记录变体并让修复双覆盖（ask 在正常 UI 流与无头物化后挂 UI 两个变体下都坏）。
5. **双层 Red-Green**：E2E 层 pin 用户可见契约（artifacts 即交付证据，gitignored）；单测层 pin 引擎契约（如 `omp-host.redgreen.test.ts`），接缝复用既有 mock-SDK-再动态导入模式，写于修复前先跑出 Red——它是唯一进仓库的测试。
6. **Green**：
   - 先 kill 旧 dev server 再 `bun run dev`——改过 server 代码必须重启，Green 必须跑在新代码上。
   - 多通道验证：截图 `read <png>?q=<具体判定问题>`（探索期问判定性问题，交付期按用户语言叙述，从不问开放式 "describe"）+ grep dev 日志 + 直读引擎会话 JSONL（`~/.omp/agent/sessions/-<proj>`）。DOM 截图会撒谎（乐观 UI），服务端真相源不会。
   - Green 阶段捞出的新集成 bug（如缺 `initThemeSync()` → `theme.status undefined`）修完必须重跑全部 green run。
7. **交付**：REPORT.md 含引擎不兼容证据链（若切过引擎）、根因 file:line、Red/Green 截图引用 + 逐步 LLM 描述。

## 负样本规则（剪枝）

drop-in 浏览器引擎出现**全应用级**交互死亡时：先跑最小 React 对照应用（`tools/react-app.mjs`，约 2 分钟）+ 点击前后 `document.documentElement.outerHTML.length` 字节级对比。对照应用正常 + 真应用字节不变 = 引擎兼容缺口 → 切 `engine:'chromium'` 并把证据写进 REPORT。不要从应用侧事件语义（isTrusted / visibilityState / React 调度器）查起。

## 工具（tools/，按需取用）

- `harness.mjs`：场景骨架（newRun/shot/settle/verdict）。跑 node 不跑 bun——Bun 的 WS 客户端握不上 obscura CDP。环境旋钮：`APP_URL`、`E2E_SCRATCH`、`CHROMIUM_BIN`（自动探测系统 Chromium，回退 Playwright 内置）、`OBSCURA_URL`。守卫：拒绝原地运行与未 gitignore 的 artifacts 路径（即上述两条保证；本仓库根 `.gitignore` 有全局 `artifacts/` 规则，其他仓库没有）。录屏需 Playwright 自带 ffmpeg：录屏失败时查 `node_modules/playwright-core/browsers.json` 的 ffmpeg revision，把系统 ffmpeg 软链到 `~/.cache/ms-playwright/ffmpeg-<rev>/ffmpeg-linux`。
- `bootstrap.mjs`：OpenChamber 启动驱动（Add-project 对话框、选 scratch、新 draft、composer 定位）。scratch 必须在家目录下，否则显式报错。
- `tree.mjs`：DOM 结构转储（tag.class[role] + rects + 文本）——截图告诉你界面上有什么，它告诉你选择器怎么写。
- `react-app.mjs`：最小 React 19 对照应用，负样本规则的决胜实验。
- `scenario.example.mjs`：指纹断言场景范本（bootstrap → 编号截图 → settle → verdict，RUN_NAME Green 复跑）。
- `fixtures/e2e-echo.md`：marker 命令，植入 scratch 项目的 `.opencode/commands/e2e-echo.md`。
