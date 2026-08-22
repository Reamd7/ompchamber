# OpenChamber 本地开发与改动验证指南

> 面向在本仓库直接改代码、然后想看到效果的人。覆盖:环境 → 日常开发循环 → 测试门禁 → 起真实服务器做验收 → 调试技巧(含本机特有的坑)。
> 约定:仓库根 = `<repo>`;所有命令在 Windows / PowerShell 或 Git Bash 下均适用(路径用正斜杠)。

---

## 1. 环境准备

| 依赖 | 版本要求 | 说明 |
|---|---|---|
| Bun | 1.3.x(仓库锁定 `bun@1.3.14`) | 包管理 + 运行时;`package.json` 的 `packageManager` 字段是权威 |
| Node | ≥22 | 少量脚本(vitest、isolated 测试运行器、**CDP 浏览器驱动**)必须用 node 跑,见 §6 |
| omp 引擎 | 自动 | web 服务器会自行 spawn `packages/web/server/lib/omp-host/host.js` 子进程,无需手动启动 |

```bash
git clone https://github.com/Reamd7/openchamber
cd openchamber
bun install        # workspace 安装(packages/* 全部就位)
```

## 2. 仓库结构(改哪里)

```
packages/
  ui/        共享 React UI、状态、同步层 —— 绝大多数界面改动在这里
  web/       web 服务器 + OpenChamber server + omp 引擎宿主(server/lib/omp-host)+ CLI
  electron/  桌面壳(Electron 主进程,内嵌 web 后端)
  vscode/    VS Code 扩展
  mobile/    Capacitor iOS/Android 壳
docs/        产品文档;omp 对齐规格在 docs/omp-parity/
scripts/     根工具:check:events 事件门禁、run-isolated-tests.mjs 等
```

**引擎侧 vs UI 侧**:`packages/web/server/lib/omp-host/` 是嵌入 `@oh-my-pi/pi-coding-agent` 的宿主(域模块 `domain-*.js`);改 UI 数据流通常横跨两侧(服务端域模块 → 事件/端点 → UI reducer/store → 组件)。

## 3. 日常开发循环

### 3.1 HMR 开发(改 UI 首选)

```bash
bun run dev            # 根目录:web HMR(web:rsbuild --watch + 服务器)
# 或分开两个终端:
cd packages/web && bun run dev          # 构建 UI,变更热更
cd packages/web && bun run dev:server   # bun server/index.js
```

### 3.2 改动后的固定门禁(每包按需)

```bash
# UI(共享组件/同步层)
cd packages/ui
bun run test          # 隔离运行器:每个测试文件独立进程(module 级单例不互相污染)
bun run type-check    # tsc --noEmit

# web(含 omp 引擎宿主)
cd packages/web
bun test server/lib/omp-host/    # 引擎域模块 bun:test
bun run test                     # 完整(vitest + 上面)

# 根级守卫
bun run check:events             # omp 事件注册表 CI 门(新增 omp.* 事件必须登记)
bun run dead-code                # 文件/导出增删后跑;报告是提示不是阻断,但要检视
bun run type-check               # 五包全查
```

**注意**:直接 `bun test`(单进程全量)在本仓库有既知的 mock 串扰基线,官方口径以 `bun run test`(隔离运行器)为准。

### 3.3 改 UI 后要"真机看一眼"的构建

HMR 之外,验收一个独立服务器要用产物:

```bash
cd packages/web && bun run build     # rsbuild → dist/
```

## 4. 起真实服务器做验收

```bash
cd packages/web
OPENCHAMBER_PORT=3903 bun server/index.js
# 就绪标志:OpenChamber server listening on 127.0.0.1:3903
```

### 4.0 绑定地址:默认仅本机,LAN / VPN(easytier 等)访问要显式开

默认绑定 `127.0.0.1` —— 只有本机能访问。用 mesh VPN(easytier / Tailscale 等)或局域网 IP 访问时,必须显式改绑定:

```bash
# 方式 A:环境变量(推荐,配合 §4 的直接起法)
OPENCHAMBER_HOST=0.0.0.0 OPENCHAMBER_PORT=3903 bun server/index.js

# 方式 B:CLI 旗标
# openchamber serve --host 0.0.0.0
```

**安全护栏**:仅给 `OPENCHAMBER_HOST=0.0.0.0` 会启动失败 —— 服务器拒绝无认证暴露在网络上,必须再二选一:

```bash
# 选项 1:设 UI 密码(不受信网络用这个)
OPENCHAMBER_HOST=0.0.0.0 OPENCHAMBER_UI_PASSWORD=你的密码 bun server/index.js

# 选项 2:明示接受无认证(私有 mesh VPN 里可接受)
OPENCHAMBER_HOST=0.0.0.0 OPENCHAMBER_ALLOW_UNAUTHENTICATED_LAN=true bun server/index.js
```

验证:`netstat -ano | grep 3903` 应见 `0.0.0.0:3903 LISTENING`;从 mesh 另一节点 `curl http://<mesh-ip>:3903/health` 应 200。Windows 首次绑定可能弹防火墙,允许 bun 即可。
```

验证:`netstat -ano | grep 3903` 应见 `0.0.0.0:3903 LISTENING`;从 mesh 另一节点 `curl http://<mesh-ip>:3903/health` 应 200。Windows 首次绑定可能弹防火墙,允许 bun 即可。

- 浏览器开 `http://localhost:3903`(LAN/mesh 场景用 `http://<mesh-ip>:3903`)
- **PWA 有 Service Worker 缓存**:改了代码但页面行为没变时,先 DevTools → Application → Service Workers → Unregister + 清缓存,再硬刷新
- omp 引擎由该服务器自动 spawn(独立子进程、独立端口、随机口令),不用管

### 4.1 常用验收端点(curl 直查服务端真值)

```bash
DIR="C:/Users/you/path/to/project"     # 换成你会话所在目录

curl "http://localhost:3903/api/omp/capabilities"                     # 能力面(feature keys)
curl --get --data-urlencode "directory=$DIR" "http://localhost:3903/api/omp/commands"    # 斜杠命令注册表(含扩展命令)
curl --get --data-urlencode "directory=$DIR" "http://localhost:3903/api/omp/chrome"      # 扩展 widget 快照
curl --get --data-urlencode "directory=$DIR" "http://localhost:3903/api/session"         # 会话列表
curl -X POST "http://localhost:3903/api/session" -H "Content-Type: application/json" \
     --data-binary "{\"directory\":\"$DIR\",\"title\":\"验收会话\"}"    # 建会话
curl -X POST "http://localhost:3903/api/omp/dialogs/lease" -H "Content-Type: application/json" \
     --data-binary "{\"directory\":\"$DIR\",\"sessionId\":\"<SID>\",\"clientId\":\"manual\"}"  # UI 租约(触发扩展初始化)
```

**目录参数必须 URL 编码**(`--data-urlencode` 或 `%2F`);web 代理会对查询里的目录做 realpath 规范化,服务端域模块统一 `normalizeDirectoryKey`,两侧不一致时表现为"明明有数据却查空"。

## 5. omp 扩展(用户级插件)

- 位置:`~/.omp/agent/extensions/*.ts`(即 `C:/Users/<you>/.omp/agent/extensions/`),引擎启动自动加载
- 扩展经宿主边界暴露面(widget/status/命令/工具/对话框);宿主实现见 `packages/web/server/lib/omp-host/domain-chrome.js`(chrome 投影)与 `domain-dialogs.js`(对话框桥)
- 验证扩展是否被识别:lease 后查 `/api/omp/commands`,自定义命令应带 `"source":"extension"`
- 环境变量随服务器进程继承(如扩展读 `ZHIPU_API_KEY`),重启服务器才生效

## 6. 浏览器自动化驱动(本机可用方案)

本机无头浏览器(puppeteer/Playwright 启动)因显卡/虚拟显示驱动**全部卡死在 CDP 握手**;可用方案是**驱动你自己的窗口化 Edge**:

```powershell
# 1. 带 CDP 端口起一个独立 profile 的 Edge
"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe" --remote-debugging-port=9223 "--remote-allow-origins=*" --user-data-dir="$env:TEMP\edge-cdp-profile" --no-first-run about:blank
```

```js
// 2. node 连接驱动(必须 node —— bun 的 WebSocket 实现与 CDP 不兼容!)
const { chromium } = await import("playwright");   // 或指向缓存里的 playwright-core
const browser = await chromium.connectOverCDP("http://127.0.0.1:9223");
const page = await browser.contexts()[0].newPage();
await page.goto("http://localhost:3903");
```

要点:
- **必须用 `node` 跑脚本**;同脚本 bun 跑会在 WS 握手超时(2026-08-22 实测定位)
- playwright 包不在本仓库依赖里;可从 bun 全局缓存目录借用,或临时 `bun add -d playwright-core`
- 截图 + 视觉判读是可靠的验收手段(截到 `%TEMP%/ui-*.png`,人眼看或喂给视觉模型)

## 7. 调试技巧与已知的坑

| 症状 | 原因 | 处置 |
|---|---|---|
| 改了服务端代码,日志一片寂静 | **引擎子进程的 stdio 只在启动握手期被接管,之后全部丢弃**(`server/lib/opencode/lifecycle.js`) | 要看引擎日志就单飞引擎,见下 |
| 想直接调试引擎(扩展/域模块) | — | `cd packages/web && OPENCODE_SERVER_PASSWORD=dev bun server/lib/omp-host/host.js serve --hostname 127.0.0.1 --port 3905`,日志直落终端;认证 = Basic `opencode:<密码>` |
| 会话 widget/数据"明明有却查不到" | 目录键形态不一致(正斜杠/反斜杠/大小写) | 全链路统一走 `normalizeDirectoryKey`;curl 用 `--get --data-urlencode` |
| 扩展命令/工具没出现 | 扩展 runner 只存在于**物化会话**;headless 命令发现天然不含扩展 | 先建会话 + 拿 UI 租约,再查 `/api/omp/commands` |
| UI 租约(lease)拿到但扩展没初始化 | 租约可能先于会话惰性物化 | 已修复:attach 会自行物化;若复现,查 `#attachDialogUi` 路径 |
| 斜杠命令提交后"没反应" | 输入框自动补全菜单开着时,Enter 是"选中建议"不是"提交" | 先 Esc 关菜单再 Enter,或直接点发送按钮 |
| 单测/页面行为对不上 | Service Worker 缓存了旧 bundle | §4 的清缓存步骤 |

## 8. 提交前清单

1. 改动包的 `bun run test` + `type-check` 绿
2. 新增/删除文件或导出:`bun run dead-code` 检视报告
3. 动了 omp 事件(新增 `omp.*`):`bun run check:events` 必须过(注册表 + 生产者登记)
4. UI 改动:i18n 新 key ×11 语言全量真实翻译(`packages/ui/src/lib/i18n/messages/`)
5. 用户可见行为:起真实服务器按 §4 验收,截图/端点输出留证
6. 提交信息沿用 conventional commits(`feat:` / `fix:` / `docs:` / `chore:`)

> 更深的建设规范:根 `CONTRIBUTING.md`;omp 对齐规格与验收证据:`docs/omp-parity/`(00-MASTER 总纲,PROGRESS-*.md 用户口径进度)。
