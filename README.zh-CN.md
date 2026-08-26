# <picture><source media="(prefers-color-scheme: dark)" srcset="docs/references/badges/openchamber-logo-dark.svg"><img src="docs/references/badges/openchamber-logo-light.svg" width="32" height="32" align="absmiddle" /></picture> OMPChamber

[English](README.md) | 简体中文

[![GitHub stars](https://img.shields.io/github/stars/Reamd7/ompchamber?style=flat&labelColor=100F0F&color=66800B)](https://github.com/Reamd7/ompchamber/stargazers)
[![GitHub release](https://img.shields.io/github/v/release/Reamd7/ompchamber?style=flat&labelColor=100F0F&color=205EA6)](https://github.com/Reamd7/ompchamber/releases/latest)
[![Discord](https://img.shields.io/badge/Discord-join.svg?style=flat&labelColor=100F0F&color=8B7EC8&logo=discord&logoColor=FFFCF0)](https://discord.gg/ZYRSdnwwKA)
[![Support the project](https://img.shields.io/badge/Support-Project-black?style=flat&labelColor=100F0F&color=EC8B49&logo=ko-fi&logoColor=FFFCF0)](https://ko-fi.com/G2G41SAWNS)

## 驱动 Agent 工作。保持掌控。随处交付。

**OMPChamber 继承 OpenChamber 的工作区体验——在桌面、浏览器、编辑器和移动端运行、监督并评审 AI 编码工作——并将引擎替换为内嵌的 omp 引擎（`@oh-my-pi/pi-coding-agent`），不再依赖外部 OpenCode CLI。**

本仓库在 OpenChamber 之上新增的内容：

- **内嵌 omp host 引擎**——每个桌面构建都自带独立完整的引擎及其原生插件，无需安装任何单独的 agent CLI。
- **omp 自身的产品面**——模型角色（default/smol/slow/plan/vision 等）、personas、会话模式、实时 agent 运行视图、逐轮 token 用量，以及完整的引擎设置编辑器。
- **独立发布通道**——面向 macOS、Windows 和 Linux（x64 与 ARM64）的 OMPChamber 桌面构建，由本仓库的 GitHub Actions 构建与发布，应用内更新直接读取本仓库的 Releases。

OMPChamber 为你提供一个统一的地方来指挥 agent 工作、理解变更并将其推向发布。切换设备或离开时，你的项目依然可用。

![OMPChamber Chat](docs/references/chat_example.png)

<details>
<summary>更多截图</summary>

![VS Code Extension](packages/vscode/extension.jpg)

<p>
<img src="docs/references/pwa_chat_example.png" width="45%" alt="OMPChamber PWA chat">
<img src="docs/references/pwa_diff_example.png" width="45%" alt="OMPChamber PWA diff review">
</p>

</details>

## OMPChamber 能做什么

### 自主推进的目标

用 **会话目标（Session Goals）** 为会话设定终点线。OpenChamber 在每一轮之后检查结果，让 agent 持续工作直到目标完成、被阻塞或达到你设定的上限——即使你已经关闭应用。

### 对比与融合多次运行

使用 **Multi-run** 把同一个任务交给最多五个模型，各自运行在独立会话（可选独立 worktree）中。查看每个模型实际构建了什么，选出最好的结果，或用 **Fusion** 把各自最强的部分融合成一个新会话。

### 引导式变更走读

**变更走读（Changes Walkthrough）** 把大 diff 变成一次 AI 引导的变更之旅：把相关编辑分组成步骤、按变更自身的逻辑排序，并解释各部分如何衔接。

### 检查运行中的应用

用 **Preview** 在对话旁边打开你的应用。指向某个元素，即可把截图、样式、位置和浏览器报错一并发给 agent——"这里这个东西"背后的全部上下文。桌面版通过内置浏览器把同样的工作流带到任意网页。

### 从 issue 到 pull request 的 GitHub 上下文

带着完整上下文从 GitHub issue 或 pull request 发起会话。把失败的检查或评审意见回传给 agent，然后在 OpenChamber 中更新或合并 pull request。

### 在其他设备上继续

在桌面、Web/PWA、VS Code、iOS 或 Android 上打开相同的项目与会话。查看进度、回答问题、评审变更，并重新接入运行中的终端。

### 私有远程访问

用一次性二维码配对设备，通过 **Private Relay** 连接，无需开放端口或暴露公网服务器。连接端到端加密，可随时撤销。同时支持直连、LAN/VPN 访问、Cloudflare/Ngrok 隧道以及 SSH。

### 跨项目跟踪工作

查看哪些会话在工作中、等待中、已完成或已失败，以及审批、计划任务、服务商限额、token 用量和成本。把会话组织进文件夹，笔记、待办和可复用的项目动作近在手边。

### 计划周期性工作

按一次、每日、每周或 cron 计划运行提示词。计划任务可以配合会话目标（Session Goals），从而持续朝目标推进，而不是在一条回复后停下。

## 在你工作的地方使用

| 端 | 定位 |
| --- | --- |
| **桌面** | 面向 macOS、Windows 和 Linux 的完整工作区：多窗口、Mini Chat、远程机器、SSH 与原生通知 |
| **Web / PWA** | 在浏览器中打开工作区，可安装为应用，通过后台通知保持更新 |
| **VS Code** | 会话就在代码旁：把选区发给 agent、在编辑器中打开结果、对比并行运行 |
| **iOS / Android** | 离开工位也能评审和引导工作，接收完成提醒，用触控操作终端 |
| **CLI / 服务器** | 在工作站或服务器上运行 OMPChamber，计划任务、管理远程访问，登录后依然可用 |

## 快速开始

### 桌面 — macOS、Windows 和 Linux

从 [GitHub Releases](https://github.com/Reamd7/ompchamber/releases/latest) 下载最新版本。桌面版自带独立完整的 omp host 引擎，无需单独安装 OpenCode。

Linux 提供 x86_64 和 ARM64 的 AppImage。为下载的 AppImage 添加可执行权限，并保存在可写位置以支持应用内更新：

```bash
chmod +x OMPChamber-*.AppImage
./OMPChamber-*.AppImage
```

Linux AppImage 需要 FUSE（`libfuse.so.2`）。没有 FUSE 时，使用 `APPIMAGE_EXTRACT_AND_RUN=1` 运行。

### VS Code

从 [Visual Studio Marketplace](https://marketplace.visualstudio.com/items?itemName=fedaykindev.openchamber) 安装 OpenChamber，或在扩展市场搜索 "OpenChamber"。

### CLI — Web 与 PWA

需要 Node.js 22+ 和 [Bun](https://bun.sh) 运行时（omp 引擎运行在 Bun 之上；无需单独安装任何 agent CLI）。

```bash
curl -fsSL https://raw.githubusercontent.com/Reamd7/ompchamber/main/scripts/install.sh | bash
ompchamber --ui-password be-creative-here
```

常用操作：

```bash
ompchamber status
ompchamber connect-url --qr
ompchamber tunnel start --provider cloudflare --mode quick --qr
ompchamber startup enable
ompchamber logs
ompchamber stop
ompchamber update
```

OMPChamber 默认只绑定 localhost。仅在可信网络中使用 `--lan`，并用 `--ui-password` 保护浏览器访问。


## 指南

深入了解：

- [快速开始](packages/docs/content/docs/quickstart.mdx)
- [安装](packages/docs/content/docs/install.mdx)
- [连接设备](packages/docs/content/docs/connect-devices.mdx)
- [Private Relay](packages/docs/content/docs/private-relay.mdx)
- [Multi-run](packages/docs/content/docs/multi-run.mdx)
- [会话目标](packages/docs/content/docs/session-goals.mdx)
- [变更走读](packages/docs/content/docs/walkthrough.mdx)
- [预览与开发服务器](packages/docs/content/docs/preview.mdx)
- [GitHub 工作流](packages/docs/content/docs/github.mdx)
- [移动端](packages/docs/content/docs/mobile.mdx)
- [安全](packages/docs/content/docs/security.mdx)
- [故障排查](packages/docs/content/docs/troubleshooting.mdx)

自托管细节见[反向代理指南](docs/REVERSE_PROXY.md)；自定义主题编写见[自定义主题指南](docs/CUSTOM_THEMES.md)。

## 为什么是 omp？

OMPChamber 的目标是保留 [OpenChamber](https://github.com/openchamber/openchamber) 的工作区体验，同时让它运行在 [omp](https://github.com/can1357/oh-my-pi)（`@oh-my-pi/pi-coding-agent`）之上。引擎以 omp host 的形式内嵌发行——一个由 omp 支撑的 OpenCode 兼容 API 面——因此应用直接讲 omp 自己的产品语言：模型角色、personas、会话模式，以及它的审批与询问流程。

OMPChamber 是独立项目，与 OpenChamber、OpenCode 及 omp 团队均无隶属关系。

## 参与贡献

开发环境搭建与贡献规范见 [CONTRIBUTING.md](./CONTRIBUTING.md)。文档撰写指引见 [`packages/docs`](packages/docs/README.md)。

## 致谢

特别感谢：

- **[OpenChamber](https://github.com/openchamber/openchamber) 及其贡献者**——OMPChamber 继承了它的工作区、界面与服务端；你在这里使用的体验源自他们的工作，这一基础的荣誉属于他们
- [OpenCode](https://opencode.ai) 优秀的 API 与可扩展的开源架构
- [omp](https://github.com/can1357/oh-my-pi) 为本构建提供动力的编码 agent 引擎
- [Pierre](https://pierrejs-docs.vercel.app/) 高速的 diff 查看器与语法高亮
- [Ghostty-web](https://github.com/coder/ghostty-web) 的 Ghostty 网页渲染器
- [Yulia Ivashko](https://github.com/yulia-ivashko) 为每次成功 push 带来的烟花庆祝
- 每一位用代码、想法与细致打磨塑造了 OpenChamber 的贡献者

## 许可证

MIT
