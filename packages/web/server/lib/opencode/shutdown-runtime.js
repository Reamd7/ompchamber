/**
 * 优雅关闭（graceful shutdown）运行时工厂。
 *
 * runShutdown 按固定顺序停掉各子运行时：会话类运行时与健康检查 -> 终端 ->
 * 消息流 -> OpenCode 子进程与端口回收 -> HTTP server（带超时兜底）-> UI 鉴权
 * 与隧道。外层 gracefulShutdown 做并发去重并挂看门狗：超过
 * shutdownWatchdogTimeoutMs 仍在关闭时，若允许退出则强制 exit(1)，
 * 否则复位关闭状态以便重试。
 */
// Every WebSocket upgrade listener (terminal, message stream, dictation) is
// removed early in this sequence, well before the stages that can hang on
// external processes. A shutdown wedged at one of those awaits therefore keeps
// serving HTTP while rejecting all realtime upgrades — a half-dead backend the
// next dev run happily proxies to. The watchdog exists to make that state
// impossible to linger in.
// 中文补充：所有 WebSocket upgrade 监听在本序列早期即被移除，若后续阶段卡死，
// 服务会呈现「HTTP 可用但拒绝实时升级」的半死状态；看门狗用于杜绝这种状态滞留。
const DEFAULT_SHUTDOWN_WATCHDOG_TIMEOUT_MS = 30_000;

/**
 * 创建优雅关闭运行时：dependencies 注入 process、超时配置、退出开关与各子
 * 运行时的存取回调（watcher、session、终端、消息流、OpenCode 进程/端口、
 * HTTP server、UI 鉴权、隧道等）；返回 gracefulShutdown 单一入口。
 */
export const createGracefulShutdownRuntime = (dependencies) => {
  const {
    process,
    shutdownTimeoutMs,
    shutdownWatchdogTimeoutMs = DEFAULT_SHUTDOWN_WATCHDOG_TIMEOUT_MS,
    getExitOnShutdown,
    getIsShuttingDown,
    setIsShuttingDown,
    syncToHmrState,
    openCodeWatcherRuntime,
    sessionRuntime,
    sessionAssistRuntime,
    sessionGoalRuntime,
    contextObligatoryRuntime,
    scheduledTasksRuntime,
    getHealthCheckInterval,
    clearHealthCheckInterval,
    getTerminalRuntime,
    setTerminalRuntime,
    getMessageStreamRuntime,
    setMessageStreamRuntime,
    shouldSkipOpenCodeStop,
    getOpenCodePort,
    getOpenCodeProcess,
    setOpenCodeProcess,
    killProcessOnPort,
    waitForPortRelease,
    getServer,
    getUiAuthController,
    setUiAuthController,
    getActiveTunnelController,
    setActiveTunnelController,
    tunnelAuthController,
  } = dependencies;

  // 进行中的关闭 Promise：并发调用 gracefulShutdown 时复用同一实例。
  let shutdownPromise = null;
  // 当前关闭阶段标签；看门狗超时日志用它报告卡在哪个阶段。
  let shutdownPhase = 'starting';
  /** 更新关闭阶段标签（仅用于诊断日志）。 */
  const enterShutdownPhase = (phase) => {
    shutdownPhase = phase;
  };

  /**
   * 执行完整关闭序列（幂等：已在关闭中则直接返回）。依次停止 watcher/会话/
   * 辅助/目标/上下文/定时任务运行时与健康检查定时器，关闭终端与消息流运行时；
   * 除非显式跳过（外部 server 模式），关闭 OpenCode 子进程、强杀端口占用进程
   * 并等待端口释放（5 秒超时仅告警）；关闭 HTTP server（与 shutdownTimeoutMs
   * 竞速，超时强制继续）；释放 UI 鉴权与活动隧道。全部完成后按配置决定是否
   * process.exit(0)。
   */
  const runShutdown = async (options = {}) => {
    if (getIsShuttingDown()) return;

    setIsShuttingDown(true);
    syncToHmrState();
    console.log('Starting graceful shutdown...');
    const exitProcess = typeof options.exitProcess === 'boolean' ? options.exitProcess : getExitOnShutdown();

    enterShutdownPhase('stopping session runtimes');

    openCodeWatcherRuntime.stop();
    sessionRuntime.dispose();
    sessionAssistRuntime?.stop?.();
    sessionGoalRuntime?.stop?.();
    contextObligatoryRuntime?.stop?.();
    scheduledTasksRuntime?.stop?.();

    const healthCheckInterval = getHealthCheckInterval();
    if (healthCheckInterval) {
      clearHealthCheckInterval(healthCheckInterval);
    }

    enterShutdownPhase('shutting down terminal runtime');
    const terminalRuntime = getTerminalRuntime();
    if (terminalRuntime) {
      try {
        await terminalRuntime.shutdown();
      } catch {
      } finally {
        setTerminalRuntime(null);
      }
    }

    enterShutdownPhase('closing message stream runtime');
    const messageStreamRuntime = getMessageStreamRuntime();
    if (messageStreamRuntime) {
      try {
        await messageStreamRuntime.close();
      } catch {
      } finally {
        setMessageStreamRuntime(null);
      }
    }

    if (!shouldSkipOpenCodeStop()) {
      const portToKill = getOpenCodePort();
      const openCodeProcess = getOpenCodeProcess();

      if (openCodeProcess) {
        console.log('Stopping OpenCode process...');
        enterShutdownPhase('stopping OpenCode process');
        try {
          await openCodeProcess.close();
        } catch (error) {
          console.warn('Error closing engine process:', error);
        }
        setOpenCodeProcess(null);
      }

      enterShutdownPhase('waiting for OpenCode port release');
      killProcessOnPort(portToKill);
      if (!(await waitForPortRelease(portToKill, 5000))) {
        console.warn(`Timed out waiting for OpenCode port ${portToKill} to be released during shutdown`);
      }
    } else {
      console.log('Skipping OpenCode shutdown (external server)');
    }

    enterShutdownPhase('closing HTTP server');
    const server = getServer();
    if (server) {
      let closeTimeout = null;
      try {
        await Promise.race([
          new Promise((resolve) => {
            server.close(() => {
              console.log('HTTP server closed');
              resolve();
            });
          }),
          new Promise((resolve) => {
            closeTimeout = setTimeout(() => {
              console.warn('Server close timeout reached, forcing shutdown');
              resolve();
            }, shutdownTimeoutMs);
          }),
        ]);
      } finally {
        clearTimeout(closeTimeout);
      }
    }

    enterShutdownPhase('disposing UI auth');
    const uiAuthController = getUiAuthController();
    if (uiAuthController) {
      uiAuthController.dispose();
      setUiAuthController(null);
    }

    enterShutdownPhase('stopping tunnel');
    const activeTunnelController = getActiveTunnelController();
    if (activeTunnelController) {
      console.log('Stopping active tunnel...');
      activeTunnelController.stop();
      setActiveTunnelController(null);
      tunnelAuthController.clearActiveTunnel();
    }
    enterShutdownPhase('complete');
    console.log('Graceful shutdown complete');
    if (exitProcess) {
      process.exit(0);
    }
  };

  /**
   * 对外的关闭入口（并发安全）：已在关闭中则复用 shutdownPromise；
   * 为本次关闭挂看门狗 —— 超过 shutdownWatchdogTimeoutMs 时报告当前阶段，
   * 允许退出则强制 exit(1)，否则清空 shutdownPromise 允许再次尝试。
   * options.exitProcess 可覆盖默认的退出策略。
   */
  const gracefulShutdown = (options = {}) => {
    if (shutdownPromise) return shutdownPromise;

    const exitProcess = typeof options?.exitProcess === 'boolean' ? options.exitProcess : getExitOnShutdown();
    let watchdogTimer = null;
    const shutdown = runShutdown(options).finally(() => {
      clearTimeout(watchdogTimer);
    });

    watchdogTimer = setTimeout(() => {
      console.error(
        `Graceful shutdown timed out after ${shutdownWatchdogTimeoutMs}ms (stuck at: ${shutdownPhase})`
      );
      if (exitProcess) {
        console.error('Forcing process exit');
        process.exit(1);
        return;
      }
      shutdownPromise = null;
    }, shutdownWatchdogTimeoutMs);
    watchdogTimer.unref?.();

    shutdownPromise = shutdown;
    return shutdownPromise;
  };

  return {
    gracefulShutdown,
  };
};
