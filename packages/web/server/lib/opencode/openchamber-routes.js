/**
 * OMPChamber 自有运维路由：版本更新检查 / 安装、models.dev 模型元数据代理
 * 与 Zen 免费模型列表。全部外部能力（fs、path、process、server、设置读取、
 * Zen 模型拉取）经 registerOMPChamberRoutes 的 dependencies 注入，宿主
 * （web / desktop / VS Code）可替换实现以便测试与适配运行环境。
 */

/** 合法 systemd unit 名称的校验正则：限定字符集且必须以 .service 结尾。 */
const SYSTEMD_SERVICE_UNIT_PATTERN = /^[A-Za-z0-9:_.@-]+\.service$/;

/**
 * 从进程环境推断当前 OMPChamber 所属的 systemd user service unit 名称。
 * 仅当存在 INVOCATION_ID（systemd 拉起标志）时才认定运行于 systemd 之下；
 * 优先取 OMPCHAMBER_SYSTEMD_UNIT 显式配置，缺省回退 ompchamber.service；
 * 名称不匹配 SYSTEMD_SERVICE_UNIT_PATTERN 时返回 null（视为不受 systemd 管理）。
 * @param {NodeJS.ProcessEnv} environment 进程环境变量
 * @returns {string | null} 合法的 unit 名称；不适用时返回 null
 */
function resolveSystemdServiceUnit(environment) {
  if (!environment.INVOCATION_ID) {
    return null;
  }

  const configuredUnit = typeof environment.OMPCHAMBER_SYSTEMD_UNIT === 'string'
    ? environment.OMPCHAMBER_SYSTEMD_UNIT.trim()
    : '';
  const unit = configuredUnit || 'ompchamber.service';
  return SYSTEMD_SERVICE_UNIT_PATTERN.test(unit) ? unit : null;
}

/**
 * 按 POSIX shell 单引号规则转义值：内部单引号替换为 '\'' 后整体包裹单引号，
 * 保证值可安全嵌入 sh 命令行。
 * @param {*} value 待转义的值（先经 String() 归一）
 * @returns {string} 带单引号的转义结果
 */
function quotePosixShell(value) {
  return `'${String(value).replace(/'/g, "'\\''")}'`;
}

/**
 * 在 Express app 上注册 /api/ompchamber 与 /api/zen 系列路由。
 * @param {import('express').Express} app Express 应用实例
 * @param {object} dependencies 宿主注入的依赖：fs/path/process、server（取当前端口）、
 *   __dirname（定位 CLI 入口）、ompchamberDataDir（实例与日志文件根目录）、
 *   modelsDevApiUrl 与 modelsMetadataCacheTtl（元数据代理参数）、设置读取、
 *   fetchFreeZenModels / getCachedZenModels（Zen 模型拉取与进程内缓存）
 * @returns {void}
 */
export const registerOMPChamberRoutes = (app, dependencies) => {
  const {
    fs,
    path,
    process,
    server,
    __dirname,
    ompchamberDataDir,
    modelsDevApiUrl,
    modelsMetadataCacheTtl,
    readSettingsFromDiskMigrated,
    fetchFreeZenModels,
    getCachedZenModels,
  } = dependencies;

  // GET /api/ompchamber/update-check：检查是否有新版本。动态加载 package-manager，
  // 规范化 query 参数（appType / platform / arch / instanceMode / 版本 / installId），
  // reportUsage 仅在显式 false/0/no 时关闭，否则依据 User-Agent 推断设备类型兜底；
  // 检查失败时返回 500 且 available:false，不让客户端误判有更新。
  app.get('/api/ompchamber/update-check', async (req, res) => {
    try {
      const { checkForUpdates } = await import('../package-manager.js');
      // 归一化 query 字符串：非空字符串去空白后返回，否则 undefined。
      const parseString = (value) => (typeof value === 'string' && value.trim().length > 0 ? value.trim() : undefined);
      // 解析 reportUsage：仅显式 false/0/no 视为关闭，其余（含缺省）均为 true。
      const parseReportUsage = (value) => {
        if (typeof value !== 'string') return true;
        const normalized = value.trim().toLowerCase();
        if (normalized === 'false' || normalized === '0' || normalized === 'no') return false;
        return true;
      };
      // 依据 User-Agent 粗分设备类型：tablet（ipad/tablet）> mobile（mobi/android/iphone）> desktop。
      const inferDeviceClass = (ua) => {
        const value = (ua || '').toLowerCase();
        if (!value) return 'unknown';
        if (value.includes('ipad') || value.includes('tablet')) return 'tablet';
        if (value.includes('mobi') || value.includes('android') || value.includes('iphone')) return 'mobile';
        return 'desktop';
      };
      const userAgent = typeof req.headers['user-agent'] === 'string' ? req.headers['user-agent'] : '';

      const updateInfo = await checkForUpdates({
        appType: parseString(req.query.appType),
        deviceClass: parseString(req.query.deviceClass) || inferDeviceClass(userAgent),
        platform: parseString(req.query.platform),
        arch: parseString(req.query.arch),
        instanceMode: parseString(req.query.instanceMode),
        currentVersion: parseString(req.query.currentVersion),
        installId: parseString(req.query.installId),
        reportUsage: parseReportUsage(parseString(req.query.reportUsage)),
      });
      res.json(updateInfo);
    } catch (error) {
      console.error('Failed to check for updates:', error);
      res.status(500).json({
        available: false,
        error: error instanceof Error ? error.message : 'Failed to check for updates',
      });
    }
  });

  // POST /api/ompchamber/update-install：安装更新并按运行形态分派重启策略。
  // 无可用更新返回 400；容器环境（/.dockerenv 或 container 标记）detach 执行
  // 更新命令且不做自动重启；systemd 前台服务必须由服务管理器重启——经
  // systemd-run --user 排队"更新 + systemctl restart"临时任务（排队失败 409）；
  // 普通守护进程则拼出"更新 + 自重启（主命令失败时回退 ompchamber CLI）"的
  // shell 脚本 detached 执行，输出追加到 ompchamberDataDir/update-install.log，
  // 随后当前进程退出。重启命令按平台用 quotePosix/quoteCmd 转义端口、host、
  // uiPassword 等参数，避免注入与断词。
  app.post('/api/ompchamber/update-install', async (_req, res) => {
    try {
      const { spawn: spawnChild, spawnSync } = await import('child_process');
      const {
        checkForUpdates,
        getUpdateCommand,
        detectPackageManagerDetails,
      } = await import('../package-manager.js');

      const updateInfo = await checkForUpdates();
      if (!updateInfo.available) {
        return res.status(400).json({ error: 'No update available' });
      }

      const pmDetails = detectPackageManagerDetails();
      const pm = pmDetails.packageManager;
      const updateCmd = getUpdateCommand(pm);
      const isContainer =
        fs.existsSync('/.dockerenv') ||
        Boolean(process.env.CONTAINER) ||
        process.env.container === 'docker';

      if (isContainer) {
        res.json({
          success: true,
          message: 'Update starting, server will stay online',
          version: updateInfo.version,
          packageManager: pm,
          autoRestart: false,
        });

        setTimeout(() => {
          console.log(`\nInstalling update using ${pm} (container mode)...`);
          console.log(`Running: ${updateCmd}`);

          const shell = process.platform === 'win32' ? (process.env.ComSpec || 'cmd.exe') : 'sh';
          const shellFlag = process.platform === 'win32' ? '/c' : '-c';
          const child = spawnChild(shell, [shellFlag, updateCmd], {
            detached: true,
            stdio: 'ignore',
            env: process.env,
          });
          child.unref();
        }, 500);

        return;
      }

      const currentPort = server.address()?.port || 3000;
      const instanceFilePath = path.join(ompchamberDataDir, 'run', `ompchamber-${currentPort}.json`);
      let storedOptions = { port: currentPort, daemon: true };
      try {
        const content = await fs.promises.readFile(instanceFilePath, 'utf8');
        storedOptions = JSON.parse(content);
      } catch {
      }
      const launchMode = storedOptions.launchMode === 'foreground' ? 'foreground' : 'daemon';
      const isForegroundService = launchMode === 'foreground';
      const systemdServiceUnit = isForegroundService ? resolveSystemdServiceUnit(process.env) : null;

      if (isForegroundService) {
        if (!systemdServiceUnit) {
          return res.status(409).json({
            error: 'Foreground servers must be updated by their service manager. Set OMPCHAMBER_SYSTEMD_UNIT when running under systemd, or run ompchamber update and restart the service.',
          });
        }

        const updateJobName = `ompchamber-update-${Date.now()}`;
        const updateLogPath = `journalctl --user-unit ${updateJobName}.service`;
        const updateScript = [
          'set -eu',
          updateCmd,
          `systemctl --user restart ${quotePosixShell(systemdServiceUnit)}`,
        ].join('\n');
        const systemdRun = spawnSync('systemd-run', [
          '--user',
          `--unit=${updateJobName}`,
          '--collect',
          '--service-type=exec',
          `--setenv=PATH=${process.env.PATH || ''}`,
          '/bin/sh',
          '-c',
          updateScript,
        ], {
          encoding: 'utf8',
          stdio: ['ignore', 'pipe', 'pipe'],
          timeout: 5000,
        });

        if (systemdRun.status !== 0) {
          const detail = (systemdRun.stderr || systemdRun.stdout || '').trim();
          return res.status(409).json({
            error: detail || `Could not queue update job for ${systemdServiceUnit}`,
          });
        }

        return res.json({
          success: true,
          message: 'Update queued; OMPChamber will restart after installation completes',
          version: updateInfo.version,
          packageManager: pm,
          autoRestart: true,
          restartManager: 'systemd',
          jobId: updateJobName,
          logPath: updateLogPath,
        });
      }

      const isWindows = process.platform === 'win32';
      // POSIX 单引号转义（同模块级 quotePosixShell，用于拼 sh 重启命令）。
      const quotePosix = (value) => `'${String(value).replace(/'/g, "'\\''")}'`;
      // Windows cmd 双引号转义：内部双引号翻倍。
      const quoteCmd = (value) => {
        const stringValue = String(value);
        return `"${stringValue.replace(/"/g, '""')}"`;
      };

      const cliPath = path.resolve(__dirname, '..', 'bin', 'cli.js');
      const restartParts = [
        isWindows ? quoteCmd(process.execPath) : quotePosix(process.execPath),
        isWindows ? quoteCmd(cliPath) : quotePosix(cliPath),
        'serve',
        '--port',
        String(storedOptions.port),
      ];
      let restartCmdPrimary = restartParts.join(' ');
      let restartCmdFallback = `ompchamber serve --port ${storedOptions.port}`;
      if (storedOptions.host) {
        if (isWindows) {
          const escapedHost = storedOptions.host.replace(/"/g, '""');
          restartCmdPrimary += ` --host "${escapedHost}"`;
          restartCmdFallback += ` --host "${escapedHost}"`;
        } else {
          const escapedHost = storedOptions.host.replace(/'/g, "'\\''");
          restartCmdPrimary += ` --host '${escapedHost}'`;
          restartCmdFallback += ` --host '${escapedHost}'`;
        }
      }
      if (storedOptions.uiPassword) {
        if (isWindows) {
          const escapedPw = storedOptions.uiPassword.replace(/"/g, '""');
          restartCmdPrimary += ` --ui-password "${escapedPw}"`;
          restartCmdFallback += ` --ui-password "${escapedPw}"`;
        } else {
          const escapedPw = storedOptions.uiPassword.replace(/'/g, "'\\''");
          restartCmdPrimary += ` --ui-password '${escapedPw}'`;
          restartCmdFallback += ` --ui-password '${escapedPw}'`;
        }
      }
      if (storedOptions.apiOnly === true) {
        restartCmdPrimary += ' --api-only';
        restartCmdFallback += ' --api-only';
      }
      const restartCmd = isForegroundService ? '' : `(${restartCmdPrimary}) || (${restartCmdFallback})`;
      const updateLogPath = path.join(ompchamberDataDir, 'update-install.log');
      const logPreamble = [
        '',
        `=== OMPChamber update ${new Date().toISOString()} ===`,
        `currentVersion=${updateInfo.currentVersion || 'unknown'}`,
        `targetVersion=${updateInfo.version || 'unknown'}`,
        `packageManager=${pm}`,
        `packageManagerReason=${pmDetails.reason || 'unknown'}`,
        `packageManagerCommand=${pmDetails.packageManagerCommand || 'unknown'}`,
        `packagePath=${pmDetails.packagePath || 'unknown'}`,
        `globalNodeModulesRoot=${pmDetails.globalNodeModulesRoot || 'unknown'}`,
        `mode=${isContainer ? 'container' : 'restart'}`,
        `launchMode=${launchMode}`,
        `updateCommand=${updateCmd}`,
        `restartCommand=${restartCmd || 'service-manager'}`,
        `logPath=${updateLogPath}`,
      ].join('\n');

      res.json({
        success: true,
        message: 'Update starting, server will restart shortly',
        version: updateInfo.version,
        packageManager: pm,
        autoRestart: true,
        restartManager: isForegroundService ? 'service' : 'cli',
      });

        setTimeout(() => {
          console.log(`\nInstalling update using ${pm}...`);
          console.log(`Running: ${updateCmd}`);
          console.log(logPreamble);

          const shell = isWindows ? (process.env.ComSpec || 'cmd.exe') : 'sh';
          const shellFlag = isWindows ? '/c' : '-c';
          const script = isWindows
            ? `
            echo ${quoteCmd(logPreamble)}
            timeout /t 2 /nobreak >nul
            ${updateCmd}
            if %ERRORLEVEL% EQU 0 (
              echo Update successful, restarting OMPChamber...
              ${restartCmd || 'echo Service manager will restart OMPChamber.'}
            ) else (
              echo Update failed
              exit /b 1
            )
            `
          : `
            printf '%s\n' ${quotePosix(logPreamble)}
            sleep 2
            ${updateCmd}
            if [ $? -eq 0 ]; then
              echo "Update successful, restarting OMPChamber..."
              ${restartCmd || 'echo "Service manager will restart OMPChamber."'}
            else
              echo "Update failed"
              exit 1
            fi
          `;

        let logFd = null;
        try {
          fs.mkdirSync(path.dirname(updateLogPath), { recursive: true });
          logFd = fs.openSync(updateLogPath, 'a');
        } catch (logError) {
          console.warn('Failed to open update log file, continuing without log capture:', logError);
        }

        const child = spawnChild(shell, [shellFlag, script], {
          detached: true,
          stdio: logFd !== null ? ['ignore', logFd, logFd] : 'ignore',
          env: process.env,
        });
        child.unref();

        if (logFd !== null) {
          try {
            fs.closeSync(logFd);
          } catch {
          }
        }

        console.log('Update process spawned, shutting down server...');

        setTimeout(() => {
          process.exit(0);
        }, 500);
      }, 500);
    } catch (error) {
      console.error('Failed to install update:', error);
      res.status(500).json({
        error: error instanceof Error ? error.message : 'Failed to install update',
      });
    }
  });

  // GET /api/ompchamber/models-metadata：代理拉取 models.dev 元数据（带 TTL 缓存）。
  // 命中且未过期缓存时 Cache-Control max-age=60，否则 300；上游超时/中止返回
  // 504，其余错误返回 502。
  app.get('/api/ompchamber/models-metadata', async (_req, res) => {
    try {
      const { getModelsMetadata } = await import('./models-metadata.js');
      const { metadata, fromCache, stale } = await getModelsMetadata({
        url: modelsDevApiUrl,
        ttlMs: modelsMetadataCacheTtl,
      });
      res.setHeader('Cache-Control', fromCache && !stale ? 'public, max-age=60' : 'public, max-age=300');
      res.json(metadata);
    } catch (error) {
      console.warn('Failed to fetch models.dev metadata via server:', error);
      const statusCode = error?.name === 'TimeoutError' || error?.name === 'AbortError' ? 504 : 502;
      res.status(statusCode).json({ error: 'Failed to retrieve model metadata' });
    }
  });

  // GET /api/zen/models：拉取 Zen 免费模型列表（缓存 5 分钟）。拉取失败时回退
  // 返回进程内缓存的列表（max-age=60）；仍无缓存则按 AbortError→504、其余→502 报错。
  app.get('/api/zen/models', async (_req, res) => {
    try {
      const models = await fetchFreeZenModels();
      res.setHeader('Cache-Control', 'public, max-age=300');
      res.json({ models });
    } catch (error) {
      console.warn('Failed to fetch zen models:', error);
      const cachedZenModels = getCachedZenModels();
      if (cachedZenModels) {
        res.setHeader('Cache-Control', 'public, max-age=60');
        res.json(cachedZenModels);
      } else {
        const statusCode = error?.name === 'AbortError' ? 504 : 502;
        res.status(statusCode).json({ error: 'Failed to retrieve zen models' });
      }
    }
  });
};
