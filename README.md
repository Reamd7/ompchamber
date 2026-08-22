# <picture><source media="(prefers-color-scheme: dark)" srcset="docs/references/badges/openchamber-logo-dark.svg"><img src="docs/references/badges/openchamber-logo-light.svg" width="32" height="32" align="absmiddle" /></picture> OMPChamber

English | [简体中文](README.zh-CN.md)

[![GitHub stars](https://img.shields.io/github/stars/Reamd7/openchamber?style=flat&labelColor=100F0F&color=66800B)](https://github.com/Reamd7/openchamber/stargazers)
[![GitHub release](https://img.shields.io/github/v/release/Reamd7/openchamber?style=flat&labelColor=100F0F&color=205EA6)](https://github.com/Reamd7/openchamber/releases/latest)
[![Discord](https://img.shields.io/badge/Discord-join.svg?style=flat&labelColor=100F0F&color=8B7EC8&logo=discord&logoColor=FFFCF0)](https://discord.gg/ZYRSdnwwKA)
[![Support the project](https://img.shields.io/badge/Support-Project-black?style=flat&labelColor=100F0F&color=EC8B49&logo=ko-fi&logoColor=FFFCF0)](https://ko-fi.com/G2G41SAWNS)

## Run agent work. Keep control. Ship from anywhere.

**OMPChamber keeps OpenChamber's workspace experience — running, supervising, and reviewing AI coding work across desktop, browser, editor, and mobile — and powers it with the omp engine (`@oh-my-pi/pi-coding-agent`) instead of an external OpenCode CLI.**

What this repository adds on top of OpenChamber:

- **Embedded omp host engine** — a self-contained engine bundled with every desktop build together with its native addons; there is no separate agent CLI to install.
- **omp's own product surface** — model roles (default/smol/slow/plan/vision and the rest), personas, session modes, live agent-run views, per-turn token usage, and a full engine-settings editor.
- **Independent release channel** — OMPChamber desktop builds for macOS, Windows, and Linux (x64 and ARM64), built and published by this repository's GitHub Actions, with in-app updates served from its Releases.

OMPChamber gives you one place to direct agent work, understand the changes, and move them toward release. Your projects stay available when you switch devices or step away.

![OMPChamber Chat](docs/references/chat_example.png)

<details>
<summary>More screenshots</summary>

![VS Code Extension](packages/vscode/extension.jpg)

<p>
<img src="docs/references/pwa_chat_example.png" width="45%" alt="OMPChamber PWA chat">
<img src="docs/references/pwa_diff_example.png" width="45%" alt="OMPChamber PWA diff review">
</p>

</details>

## What you can do with OMPChamber

### Goals that continue on their own

Give a session a finish line with **Session Goals**. OpenChamber checks the result after every turn and keeps the agent working until the goal is complete, blocked, or reaches the limit you set — even after you close the app.

### Compare and combine runs

Use **Multi-run** to give the same task to up to five models, each in its own session and optionally its own worktree. See what each one actually built, choose the best result, or use **Fusion** to combine the strongest parts into a new session.

### Guided changes walkthroughs

**Changes Walkthrough** turns a large diff into an AI-guided tour of the change. It groups related edits into steps, puts them in the order the change makes sense, and explains how the pieces fit together.

### Inspect a running app

Open your app beside the conversation with **Preview**. Point at an element and send the agent its screenshot, styles, position, and browser errors — all the context behind “this thing here.” Desktop brings the same workflow to any web page through its built-in browser.

### GitHub context from issue to pull request

Start a session from a GitHub issue or pull request with its context attached. Send failed checks or review comments back to the agent, then update or merge the pull request from OpenChamber.

### Continue on another device

Open the same projects and sessions from Desktop, Web/PWA, VS Code, iOS, or Android. Check progress, answer questions, review changes, and reattach to a running terminal.

### Private remote access

Pair a device with a one-time QR code and connect through **Private Relay** without opening ports or exposing a public server. The connection is end-to-end encrypted and can be revoked at any time. Direct connections, LAN/VPN access, Cloudflare/Ngrok tunnels, and SSH are also supported.

### Track work across projects

See which sessions are working, waiting, finished, or failed, along with approvals, scheduled tasks, provider limits, token use, and costs. Organize sessions into folders and keep notes, todos, and reusable project actions nearby.

### Schedule recurring work

Run a prompt once, daily, weekly, or on a cron schedule. Scheduled tasks can use Session Goals, so they continue toward an outcome instead of stopping after one response.

## Use it where you work

| Surface | Role |
| --- | --- |
| **Desktop** | The complete workspace for macOS, Windows, and Linux, with multiple windows, Mini Chat, remote machines, SSH, and native notifications |
| **Web / PWA** | Open your workspace in a browser, install it as an app, and stay up to date through background notifications |
| **VS Code** | Keep sessions beside your code, send selections to the agent, open results in the editor, and compare parallel runs |
| **iOS / Android** | Review and steer work away from your desk, receive completion alerts, and use the terminal with touch controls |
| **CLI / Server** | Run OpenChamber on a workstation or server, schedule work, manage remote access, and keep it available after login |

## Quick start

### Desktop — macOS, Windows, and Linux

Download the latest release from [GitHub Releases](https://github.com/Reamd7/openchamber/releases/latest). Desktop bundles a self-contained omp host engine, so no separate OpenCode installation is required.

Linux releases are available as x86_64 and ARM64 AppImages. Make the downloaded AppImage executable and keep it in a writable location for in-app updates:

```bash
chmod +x OMPChamber-*.AppImage
./OMPChamber-*.AppImage
```

Linux AppImages require FUSE (`libfuse.so.2`). Without FUSE, run with `APPIMAGE_EXTRACT_AND_RUN=1`.

### VS Code

Install [OpenChamber from the Visual Studio Marketplace](https://marketplace.visualstudio.com/items?itemName=fedaykindev.openchamber), or search for “OpenChamber” in Extensions.

### CLI — Web and PWA

Requires Node.js 22+. CLI/Web and VS Code use your installed [OpenCode CLI](https://opencode.ai).

```bash
curl -fsSL https://raw.githubusercontent.com/Reamd7/openchamber/main/scripts/install.sh | bash
openchamber --ui-password be-creative-here
```

Common operations:

```bash
openchamber status
openchamber connect-url --qr
openchamber tunnel start --provider cloudflare --mode quick --qr
openchamber startup enable
openchamber logs
openchamber stop
openchamber update
```

OMPChamber binds to localhost by default. Use `--lan` only on a trusted network and protect browser access with `--ui-password`.

## Guides

Go deeper with the guides:

- [Quick start](packages/docs/content/docs/quickstart.mdx)
- [Installation](packages/docs/content/docs/install.mdx)
- [Connect devices](packages/docs/content/docs/connect-devices.mdx)
- [Private Relay](packages/docs/content/docs/private-relay.mdx)
- [Multi-run](packages/docs/content/docs/multi-run.mdx)
- [Session Goals](packages/docs/content/docs/session-goals.mdx)
- [Changes Walkthrough](packages/docs/content/docs/walkthrough.mdx)
- [Preview and dev servers](packages/docs/content/docs/preview.mdx)
- [GitHub workflows](packages/docs/content/docs/github.mdx)
- [Mobile](packages/docs/content/docs/mobile.mdx)
- [Security](packages/docs/content/docs/security.mdx)
- [Troubleshooting](packages/docs/content/docs/troubleshooting.mdx)

For self-hosting details, see the [reverse proxy guide](docs/REVERSE_PROXY.md). For custom theme authoring, see the [custom themes guide](docs/CUSTOM_THEMES.md).

## Why omp?

OMPChamber exists to keep [OpenChamber](https://github.com/openchamber/openchamber)'s workspace experience while running it on [omp](https://github.com/can1357/oh-my-pi) (`@oh-my-pi/pi-coding-agent`). The engine ships embedded as the omp host — an OpenCode-compatible API surface backed by omp — so the app speaks omp's product surface directly: model roles, personas, session modes, and its approval and ask flows.

OMPChamber is an independent project and is not affiliated with the OpenChamber, OpenCode, or omp teams.

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for development setup and contribution guidelines. Documentation authoring guidance lives in [`packages/docs`](packages/docs/README.md).

## Acknowledgments

Special thanks to:

- **[OpenChamber](https://github.com/openchamber/openchamber) and its contributors** — OMPChamber inherits its workspace, UI, and server; the experience you use here is their work, and the credit for that foundation belongs to them
- [OpenCode](https://opencode.ai) for its excellent API and extensible open-source architecture
- [omp](https://github.com/can1357/oh-my-pi) for the coding-agent engine powering this build
- [Pierre](https://pierrejs-docs.vercel.app/) for its fast diff viewer and syntax highlighting
- [Ghostty-web](https://github.com/coder/ghostty-web) for its Ghostty web renderer
- [Yulia Ivashko](https://github.com/yulia-ivashko), who built the firework celebration that plays on every successful push
- Every contributor who shaped OpenChamber with code, ideas, and attention to detail

## License

MIT
