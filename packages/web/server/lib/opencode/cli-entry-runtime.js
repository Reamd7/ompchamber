/**
 * CLI 入口运行时：当本模块作为命令行入口直接执行时启动服务器。
 *
 * 判断依据是 process.argv[1] 与当前文件路径一致（node server.js 形态）；
 * 被其它模块 import 时（argv[1] 不同）不产生任何副作用。
 */
/**
 * 若作为 CLI 主入口执行，则解析 serve 选项并启动服务器。
 *
 * 非 CLI 执行时直接返回；是 CLI 时解析 --port/--host 与隧道相关选项，
 * 设置“关闭时退出进程”，随后以 attachSignals 与 exitOnShutdown 均开启的
 * 方式调用 startServer。启动失败打印错误并以退出码 1 结束进程。
 * @param {object} dependencies - process、当前文件名、parseServeCliOptions、
 *   defaultPort、cloudflareProvider、managedLocalMode、setExitOnShutdown
 *   与 startServer。
 */
export const runCliEntryIfMain = (dependencies) => {
  const {
    process,
    currentFilename,
    parseServeCliOptions,
    defaultPort,
    cloudflareProvider,
    managedLocalMode,
    setExitOnShutdown,
    startServer,
  } = dependencies;

  const isCliExecution = process.argv[1] === currentFilename;
  if (!isCliExecution) {
    return;
  }

  const cliOptions = parseServeCliOptions({
    argv: process.argv.slice(2),
    env: process.env,
    defaultPort,
    cloudflareProvider,
    managedLocalMode,
  });

  setExitOnShutdown(true);
  startServer({
    port: cliOptions.port,
    host: cliOptions.host,
    tryCfTunnel: cliOptions.tryCfTunnel,
    tunnelProvider: cliOptions.tunnelProvider,
    tunnelMode: cliOptions.tunnelMode,
    tunnelConfigPath: cliOptions.tunnelConfigPath,
    tunnelToken: cliOptions.tunnelToken,
    tunnelHostname: cliOptions.tunnelHostname,
    attachSignals: true,
    exitOnShutdown: true,
    uiPassword: cliOptions.uiPassword,
    apiOnly: cliOptions.apiOnly,
  }).catch((error) => {
    console.error('Failed to start server:', error);
    process.exit(1);
  });
};
