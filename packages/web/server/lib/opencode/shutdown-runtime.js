// Every WebSocket upgrade listener (terminal, message stream, dictation) is
// removed early in this sequence, well before the stages that can hang on
// external processes. A shutdown wedged at one of those awaits therefore keeps
// serving HTTP while rejecting all realtime upgrades — a half-dead backend the
// next dev run happily proxies to. The watchdog exists to make that state
// impossible to linger in.
const DEFAULT_SHUTDOWN_WATCHDOG_TIMEOUT_MS = 30_000;

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

  let shutdownPromise = null;
  let shutdownPhase = 'starting';
  const enterShutdownPhase = (phase) => {
    shutdownPhase = phase;
  };

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
          console.warn('Error closing OpenCode process:', error);
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
