/**
 * 服务端启动运行时工厂：端口绑定（含 EADDRINUSE 有限重试）、监听成功后的
 * 就绪 IPC 通知与可选隧道启动，以及进程级信号/异常处理器的挂接。
 * 全部依赖（server、隧道控制器、gracefulShutdown 等）经 dependencies 注入。
 */
/** One stray uncaught exception is survivable; a storm means the process is broken. */
// 中文补充：60 秒窗口内未捕获异常超过上限即判定为异常风暴，触发优雅停机。
const UNCAUGHT_STORM_LIMIT = 10;
const UNCAUGHT_STORM_WINDOW_MS = 60_000;
// A restart races the previous instance's graceful shutdown, which can hold
// the port for seconds (OpenCode teardown, socket drain). Without a bounded
// EADDRINUSE retry the new instance dies instantly and nodemon parks in
// "app crashed", leaving the dev proxy pointed at nothing.
// 中文补充：绑定重试的默认间隔与总窗口 —— 上一个实例的优雅关闭可能占用端口
// 数秒，新实例必须有限重试而不是立即退出（否则 nodemon 会停在崩溃态）。
const DEFAULT_BIND_RETRY_INTERVAL_MS = 500;
const DEFAULT_BIND_RETRY_WINDOW_MS = 20_000;


/**
 * 创建服务端启动运行时：dependencies 注入 process/crypto/server、隧道
 * bootstrap TTL 归一化、设置读取、隧道鉴权与启动器、gracefulShutdown、信号
 * 挂接状态存取、HMR 同步、隧道模式常量，以及可覆盖的绑定重试间隔/窗口；
 * 返回 resolveBindHost、startListeningAndMaybeTunnel 与 attachProcessHandlers。
 */
export const createServerStartupRuntime = (dependencies) => {
  const {
    process,
    crypto,
    server,
    normalizeTunnelBootstrapTtlMs,
    readSettingsFromDiskMigrated,
    tunnelAuthController,
    startTunnelWithNormalizedRequest,
    gracefulShutdown,
    getSignalsAttached,
    setSignalsAttached,
    syncToHmrState,
    TUNNEL_MODE_QUICK,
    TUNNEL_MODE_MANAGED_LOCAL,
    TUNNEL_MODE_MANAGED_REMOTE,
    bindRetryIntervalMs = DEFAULT_BIND_RETRY_INTERVAL_MS,
    bindRetryWindowMs = DEFAULT_BIND_RETRY_WINDOW_MS,
  } = dependencies;

  /**
   * 解析实际绑定主机：显式 host 优先，其次环境变量 OMPCHAMBER_HOST（非空白），
   * 最后回退 127.0.0.1（默认只监听本机，避免意外暴露）。
   */
  const resolveBindHost = (host) =>
    host
    || (typeof process.env.OMPCHAMBER_HOST === 'string' && process.env.OMPCHAMBER_HOST.trim().length > 0
      ? process.env.OMPCHAMBER_HOST.trim()
      : '127.0.0.1');

  /** 等待 ms 毫秒后 resolve；setTimeout 句柄尽可能 unref，不阻塞进程退出。 */
  const wait = (ms) => new Promise((resolve) => {
    setTimeout(resolve, ms).unref?.();
  });

  /**
   * 监听端口并在 EADDRINUSE 时有限重试：按 bindRetryIntervalMs 间隔重试，
   * 直至超过 bindRetryWindowMs 窗口或出现其它错误才抛出；首次遇到端口占用
   * 打一条日志（仅一次）。监听成功后 resolve，onListening 作为回调传给
   * server.listen。
   */
  const listenWithBindRetry = async ({ port, bindHost, onListening }) => {
    const deadline = Date.now() + bindRetryWindowMs;
    let busyLogged = false;

    for (;;) {
      try {
        await new Promise((resolve, reject) => {
          const onError = (error) => {
            server.off('error', onError);
            server.off('listening', onListeningEvent);
            reject(error);
          };
          const onListeningEvent = () => {
            server.off('error', onError);
            resolve();
          };
          server.once('error', onError);
          server.once('listening', onListeningEvent);
          server.listen(port, bindHost, onListening);
        });
        return;
      } catch (error) {
        const retriable = error && error.code === 'EADDRINUSE' && Date.now() + bindRetryIntervalMs <= deadline;
        if (!retriable) {
          throw error;
        }
        if (!busyLogged) {
          busyLogged = true;
          console.log(
            `Port ${port} is still in use (previous instance draining?); retrying for up to ${Math.round(bindRetryWindowMs / 1000)}s`
          );
        }
        await wait(bindRetryIntervalMs);
      }
    }
  };

  /**
   * 监听端口并执行启动期动作，返回 { activePort }（请求端口为 0 时是实际
   * 分配端口）。监听成功后：若运行于 IPC 子进程模式，先经 process.send 发送
   * ompchamber:ready 与端口（通道已断开则直接失败）；打印监听与健康检查地址；
   * 若携带 startupTunnelRequest 则按其模式启动隧道 —— 成功后登记活动隧道、
   * 签发一次性 bootstrap token 并生成 connect URL（经 onTunnelReady 回调或
   * 打印日志），失败仅告警并继续无隧道运行。
   */
  const startListeningAndMaybeTunnel = async ({
    port,
    bindHost,
    startupTunnelRequest,
    onTunnelReady,
  }) => {
    let activePort = port;

    await new Promise((resolve, reject) => {
      const onListening = async () => {
        try {
          const addressInfo = server.address();
          activePort = typeof addressInfo === 'object' && addressInfo ? addressInfo.port : port;

          if (typeof process.send === 'function') {
            if (!process.connected) {
              throw new Error('OMPChamber startup IPC channel disconnected before ready notification');
            }

            await new Promise((resolveReadyNotification, rejectReadyNotification) => {
              try {
                process.send({ type: 'ompchamber:ready', port: activePort }, (error) => {
                  if (error) {
                    rejectReadyNotification(error);
                    return;
                  }
                  resolveReadyNotification();
                });
              } catch (error) {
                rejectReadyNotification(error);
              }
            });
          }

          const displayHost = (bindHost === '0.0.0.0' || bindHost === '::' || bindHost === '[::]')
            ? 'localhost'
            : (bindHost.includes(':') ? `[${bindHost}]` : bindHost);
          console.log(`OMPChamber server listening on ${bindHost}:${activePort}`);
          console.log(`Health check: http://${displayHost}:${activePort}/health`);
          console.log(`Web interface: http://${displayHost}:${activePort}`);

          if (startupTunnelRequest) {
            const startupModeLabel = startupTunnelRequest.mode === TUNNEL_MODE_QUICK
              ? 'Quick Tunnel'
              : (startupTunnelRequest.mode === TUNNEL_MODE_MANAGED_LOCAL
                ? 'Managed Local Tunnel'
                : (startupTunnelRequest.mode === TUNNEL_MODE_MANAGED_REMOTE ? 'Managed Remote Tunnel' : 'Tunnel'));
            console.log(`\nInitializing ${startupModeLabel} for provider '${startupTunnelRequest.provider}'...`);
            try {
              const { publicUrl, mode } = await startTunnelWithNormalizedRequest({
                provider: startupTunnelRequest.provider,
                mode: startupTunnelRequest.mode,
                intent: startupTunnelRequest.intent,
                hostname: startupTunnelRequest.hostname,
                token: startupTunnelRequest.token,
                configPath: startupTunnelRequest.configPath,
                selectedPresetId: '',
                selectedPresetName: '',
              });
              if (publicUrl) {
                tunnelAuthController.setActiveTunnel({
                  tunnelId: crypto.randomUUID(),
                  publicUrl,
                  mode,
                });
                const settings = await readSettingsFromDiskMigrated();
                const bootstrapTtlMs = settings?.tunnelBootstrapTtlMs === null
                  ? null
                  : normalizeTunnelBootstrapTtlMs(settings?.tunnelBootstrapTtlMs);
                const bootstrapToken = tunnelAuthController.issueBootstrapToken({ ttlMs: bootstrapTtlMs });
                const connectUrl = `${publicUrl.replace(/\/$/, '')}/connect?t=${encodeURIComponent(bootstrapToken.token)}`;
                if (onTunnelReady) {
                  onTunnelReady(publicUrl, connectUrl);
                } else {
                  console.log(`\n🌐 Tunnel URL: ${connectUrl}`);
                  console.log('🔑 One-time connect link (expires after first use)\n');
                }
              } else if (onTunnelReady) {
                onTunnelReady(publicUrl, null);
              }
            } catch (error) {
              console.error(`Failed to start tunnel: ${error.message}`);
              console.log('Continuing without tunnel...');
            }
          }

          resolve();
        } catch (error) {
          reject(error);
        }
      };

      listenWithBindRetry({ port, bindHost, onListening }).catch(reject);
    });

    return { activePort };
  };

  /**
   * 挂接进程级处理器：按需注册信号处理器（SIGTERM/SIGINT/SIGQUIT/SIGHUP/
   * SIGUSR2 一律走 gracefulShutdown，确保托管的 OpenCode 子进程被优雅回收
   * 而非成为孤儿）；unhandledRejection 只记日志不退出；uncaughtException
   * 记日志并做 60 秒滑动窗口计数，超过 UNCAUGHT_STORM_LIMIT 个才触发停机
   * （孤立的偶发异常不至于让实例失联）。
   */
  const attachProcessHandlers = ({ attachSignals }) => {
    if (attachSignals && !getSignalsAttached()) {
      /** 信号统一入口：等待优雅关闭完成。 */
      const handleSignal = async () => {
        await gracefulShutdown();
      };
      // Cover every signal a shell or dev harness may use to stop/restart us, so
      // the managed OpenCode child is always torn down gracefully instead of
      // orphaned: SIGINT/SIGQUIT (Ctrl+C/Ctrl+\), SIGTERM (kill/default), SIGHUP
      // (terminal close), SIGUSR2 (nodemon restart for `dev:server:watch`).
      process.on('SIGTERM', handleSignal);
      process.on('SIGINT', handleSignal);
      process.on('SIGQUIT', handleSignal);
      process.on('SIGHUP', handleSignal);
      process.on('SIGUSR2', handleSignal);
      setSignalsAttached(true);
      syncToHmrState();
    }

    process.on('unhandledRejection', (reason, promise) => {
      console.error('Unhandled Rejection at:', promise, 'reason:', reason);
    });

    // A single stray exception — a socket teardown race, a Node-internal bug
    // like `setTypeOfService EINVAL` — must not take the server down. Nothing
    // restarts this process (it is embedded in the desktop app or run by hand
    // in a terminal), so shutting down turns every such stray into "the
    // instance is unreachable until I restart it". Mirror the
    // unhandledRejection policy above: log and keep serving. A sustained storm
    // of exceptions is a different situation — the process is genuinely
    // broken — so that still shuts down rather than limping along half-alive.
    const exceptionTimes = [];
    process.on('uncaughtException', (error) => {
      console.error('Uncaught Exception:', error);
      const now = Date.now();
      exceptionTimes.push(now);
      while (exceptionTimes.length > 0 && now - exceptionTimes[0] > UNCAUGHT_STORM_WINDOW_MS) {
        exceptionTimes.shift();
      }
      if (exceptionTimes.length > UNCAUGHT_STORM_LIMIT) {
        console.error(`More than ${UNCAUGHT_STORM_LIMIT} uncaught exceptions within a minute; shutting down.`);
        gracefulShutdown();
      }
    });
  };

  return {
    resolveBindHost,
    startListeningAndMaybeTunnel,
    attachProcessHandlers,
  };
};
