/**
 * @module index
 * OMPChamber 旧版 Node/express web server 入口的公共类型声明。
 *
 * 该模块（`packages/web/server/index.js`）是 web server 的参考实现
 * （新实现为 `server-rs` Rust 版），此文件为其对外导出补充 TypeScript
 * 类型，供 desktop/Electron 等调用方以 `import type` 方式引用，无需
 * 运行 JS 即可获得补全与检查。
 */
import type { Express } from "express";
import type { Server } from "http";

/**
 * 启动后的 web server 控制器：持有底层 express/http 实例，并暴露端口、
 * 就绪状态、OpenCode 重启与停机等控制能力。
 */
export interface WebUiServerController {
  /** 已装配完所有中间件与路由的 express 应用实例。 */
  expressApp: Express;
  /** 底层 Node http server（含已挂载的 WebSocket 升级处理）。 */
  httpServer: Server;
  /** 取得实际监听端口；端口由内核自动分配（port=0）或隧道上下文决定，未监听时为 null。 */
  getPort: () => number | null;
  /** 受管 OpenCode 上游的端口；外部 OpenCode 或尚未就绪时可能为 null。 */
  getOpenCodePort: () => number | null;
  /** OpenCode 是否已完成启动并就绪（供 UI 决定何时发首批请求）。 */
  isReady: () => boolean;
  /** 重启受管 OpenCode 进程（可迁移到新端口，完成后 message-stream 会重绑）。 */
  restartOpenCode: () => Promise<void>;
  /** 优雅停机：停止各 runtime、关闭 server 与 OpenCode 进程；exitProcess 为 true 时最终退出进程。 */
  stop: (options?: { exitProcess?: boolean }) => Promise<void>;
}

/** startWebUiServer 的启动选项（全部可选，未提供时走环境变量/默认值）。 */
export interface StartWebUiServerOptions {
  /** 监听端口；0 表示由内核自动分配，缺省为 3000。 */
  port?: number;
  /** 绑定地址；缺省 127.0.0.1，绑定到非回环地址时要求设置 uiPassword。 */
  host?: string;
  /** 是否注册 SIGINT/SIGTERM 等信号处理器，缺省 true。 */
  attachSignals?: boolean;
  /** 停机完成后是否退出进程，缺省 true（嵌入宿主通常传 false）。 */
  exitOnShutdown?: boolean;
  /** UI 密码；为空时回退到 OMPCHAMBER_UI_PASSWORD 环境变量，仍为空则沿用/生成受管密码。 */
  uiPassword?: string | null;
}

/**
 * 启动 OMPChamber web server：装配 express 路由、隧道/relay 配线、启动
 * 受管 OpenCode 与各类后台 runtime，并返回控制器。
 *
 * @param options 启动选项，见 StartWebUiServerOptions
 * @returns 控制器（含 expressApp/httpServer 及运行时查询与停机接口）
 */
export declare function startWebUiServer(
  options?: StartWebUiServerOptions
): Promise<WebUiServerController>;

/** 优雅停机入口：可独立于控制器调用，清理后台任务并按需退出进程。 */
export declare function gracefulShutdown(options?: { exitProcess?: boolean }): Promise<void>;

/** 在给定的 express app 上挂载 OpenCode 反向代理相关路由与中间件。 */
export declare function setupProxy(app: Express): void;

/** 重启受管 OpenCode 进程并等待其重新就绪。 */
export declare function restartOpenCode(): Promise<void>;

/**
 * 解析 `ompchamber serve` 的命令行参数（argv），返回归一化后的启动配置。
 *
 * @param argv 参数数组，缺省使用 process.argv
 */
export declare function parseArgs(argv?: string[]): {
  /** 监听端口（已应用默认值与数值归一化）。 */
  port: number;
  /** 绑定地址，未指定时为 undefined（由 server 决定默认值）。 */
  host?: string;
  /** UI 密码；未提供时为 null。 */
  uiPassword: string | null;
  /** 是否尝试以 quick 模式启动 Cloudflare 隧道（旧版 --cf-tunnel 兼容路径）。 */
  tryCfTunnel: boolean;
  /** 隧道提供方（cloudflare/ngrok），未指定时为 undefined。 */
  tunnelProvider?: string;
  /** 隧道模式（quick/managed-local/managed-remote），未指定时为 undefined。 */
  tunnelMode?: string;
  /** 隧道配置文件路径；null 表示显式要求使用 canonical 配置。 */
  tunnelConfigPath?: string | null;
  /** managed-remote 隧道的 token（可选）。 */
  tunnelToken?: string;
  /** managed-remote 隧道的主机名（可选）。 */
  tunnelHostname?: string;
};
