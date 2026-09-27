/**
 * 包管理器探测与自更新模块。
 *
 * 职责：识别当前 web 包（@ompchamber/web）是通过哪个 package manager
 * （npm/pnpm/yarn/bun）全局安装的；组装对应 PM 的更新命令；从本仓库的
 * GitHub Releases（本 fork 不发布到 npm registry）拉取最新版本号、变更日志
 * 并做 semver 比较，供 CLI 的 `ompchamber update` 与更新检查流程使用。
 * 所有外部探测都通过 spawnSync 同步执行且带超时。
 */
import { spawnSync } from 'child_process';
import crypto from 'crypto';
import fs from 'fs';
import os from 'os';
import path from 'path';
import { fileURLToPath } from 'url';

/** ESM 环境下还原 CommonJS 的 __filename（当前源文件绝对路径）。 */
const __filename = fileURLToPath(import.meta.url);
/** ESM 环境下还原 CommonJS 的 __dirname（当前源文件所在目录）。 */
const __dirname = path.dirname(__filename);

/** 本包在 npm 生态中的包名，用于在各 PM 的全局安装列表中定位自己。 */
const PACKAGE_NAME = '@ompchamber/web';
/** 包名按 '/' 拆出的路径段，用于拼出全局 node_modules 根之下的包目录。 */
const PACKAGE_PATH_SEGMENTS = PACKAGE_NAME.split('/');
/** 本 fork 的 GitHub 仓库坐标（owner/repo），发布产物与 changelog 均来自这里。 */
const GITHUB_REPO = 'Reamd7/ompchamber';
/** main 分支 CHANGELOG.md 的 raw 地址，用于拼两个版本之间的更新说明。 */
const CHANGELOG_URL = `https://raw.githubusercontent.com/${GITHUB_REPO}/main/CHANGELOG.md`;
/** GitHub Releases 页面地址，作为 releaseUrl 返回给调用方展示。 */
const GITHUB_RELEASES_URL = `https://github.com/${GITHUB_REPO}/releases`;
/** GitHub Releases REST API 地址，用于查询最新版本与指定 tag 的资产列表。 */
const GITHUB_RELEASES_API_URL = `https://api.github.com/repos/${GITHUB_REPO}/releases`;
// OMPChamber is distributed as release tarballs, not on the npm registry;
// the version-less URL always resolves to the newest published release.
/** 指向最新 release 的 web 包 tarball 下载地址（不带版本号，始终解析到最新发布）。 */
const RELEASE_TARBALL_LATEST_URL = `https://github.com/${GITHUB_REPO}/releases/latest/download/ompchamber-web-latest.tgz`;
/** 首次探测成功后缓存的 package manager 名称，进程内复用，避免重复 spawn 探测。 */
let cachedDetectedPm = null;

/**
 * spawnSync 的平台基础选项：Windows 上隐藏子进程控制台窗口，其它平台为空。
 * 供本模块所有同步子进程调用展开合并。
 */
function getSpawnSyncBaseOptions() {
  return process.platform === 'win32' ? { windowsHide: true } : {};
}
// Optional hosted update-check API (upstream telemetry service is disabled
// for this fork; set OMPCHAMBER_UPDATE_API_URL to use a custom one).
/** 可选的自托管更新检查 API 地址；空字符串表示禁用远程检查，仅走 GitHub Releases。 */
const UPDATE_CHECK_URL = process.env.OMPCHAMBER_UPDATE_API_URL || '';

/**
 * 返回 OMPChamber 的用户级配置目录：Windows 优先 %APPDATA%\ompchamber，
 * 其它平台为 ~/.config/ompchamber。本函数不负责创建目录。
 */
function getOMPChamberConfigDir() {
  if (process.platform === 'win32') {
    const appData = process.env.APPDATA;
    if (appData) return path.join(appData, 'ompchamber');
  }

  return path.join(os.homedir(), '.config', 'ompchamber');
}

/**
 * 校验安装 ID 的作用域标签，仅接受 desktop-electron/vscode/web/mobile-capacitor
 * 四种；非法值一律回退为 'web'，避免把任意字符串拼进文件名。
 */
function sanitizeInstallScope(scope) {
  if (scope === 'desktop-electron' || scope === 'vscode' || scope === 'web' || scope === 'mobile-capacitor') return scope;
  return 'web';
}

/**
 * 读取（不存在则生成并落盘）某个作用域的匿名安装 ID，供更新检查统计使用。
 * 幂等：优先复用 <configDir>/install-id-<scope> 中已有的 UUID；新建时递归创建
 * 目录并以 0o600 权限写入仅含 UUID 的文件。读失败一律按"不存在"处理并重新生成。
 */
function getOrCreateInstallId(scope = 'web') {
  const configDir = getOMPChamberConfigDir();
  const normalizedScope = sanitizeInstallScope(scope);
  const idPath = path.join(configDir, `install-id-${normalizedScope}`);

  try {
    const existing = fs.readFileSync(idPath, 'utf8').trim();
    if (existing) return existing;
  } catch {
    // Generate new id.
  }

  const installId = crypto.randomUUID();
  fs.mkdirSync(configDir, { recursive: true });
  fs.writeFileSync(idPath, `${installId}\n`, { encoding: 'utf8', mode: 0o600 });
  return installId;
}

/** 把 process.platform 归一为上报用的平台名：darwin→macos、win32→windows、linux→linux，其余→web。 */
function mapPlatform(value) {
  if (value === 'darwin') return 'macos';
  if (value === 'win32') return 'windows';
  if (value === 'linux') return 'linux';
  return 'web';
}

/** 把架构名（含 aarch64/amd64 等别名）归一为 arm64/x64，无法识别时返回 'unknown'。 */
function mapArch(value) {
  if (value === 'arm64' || value === 'aarch64') return 'arm64';
  if (value === 'x64' || value === 'amd64') return 'x64';
  return 'unknown';
}

/** 校验 app 类型，仅接受 web/desktop-electron/vscode/mobile-capacitor，非法值回退 'web'。 */
function normalizeAppType(value) {
  if (value === 'web' || value === 'desktop-electron' || value === 'vscode' || value === 'mobile-capacitor') return value;
  return 'web';
}

/** 校验设备类别，仅接受 mobile/tablet/desktop/unknown，非法值回退 'unknown'。 */
function normalizeDeviceClass(value) {
  if (value === 'mobile' || value === 'tablet' || value === 'desktop' || value === 'unknown') return value;
  return 'unknown';
}

/** 校验上报的平台名；非法时回退到经 mapPlatform 归一的当前宿主平台。 */
function normalizePlatform(value) {
  if (value === 'macos' || value === 'windows' || value === 'linux' || value === 'web' || value === 'android' || value === 'ios') return value;
  return mapPlatform(process.platform);
}

/** 校验上报的架构名；非法时回退到经 mapArch 归一的当前宿主架构。 */
function normalizeArch(value) {
  if (value === 'arm64' || value === 'x64' || value === 'unknown') return value;
  return mapArch(process.arch);
}

/**
 * 为 Android 客户端解析可直接安装的 APK 下载地址。
 * candidateUrl 本身指向 .apk 时直接采用（可解析为 URL 即可）；否则查询
 * GitHub Releases 的 v<version> tag 资产，优先匹配 `OMPChamber-*-android.apk`
 * 命名，退回任一 .apk 资产。网络失败、tag 缺失或没有 APK 资产时返回 undefined，
 * 调用方据此省略 downloadUrl。
 */
async function resolveAndroidApkUrl(version, candidateUrl) {
  if (typeof candidateUrl === 'string') {
    try {
      if (new URL(candidateUrl).pathname.toLowerCase().endsWith('.apk')) return candidateUrl;
    } catch {
      // Resolve malformed or non-APK values from the authoritative release assets below.
    }
  }

  try {
    const response = await fetch(`${GITHUB_RELEASES_API_URL}/tags/v${version}`, {
      headers: {
        Accept: 'application/vnd.github+json',
        'User-Agent': 'ompchamber-update-check',
      },
      signal: AbortSignal.timeout(10000),
    });
    if (!response.ok) return undefined;

    const release = await response.json();
    const apkAssets = Array.isArray(release?.assets)
      ? release.assets.filter((asset) => (
        typeof asset?.name === 'string'
        && asset.name.toLowerCase().endsWith('.apk')
        && typeof asset.browser_download_url === 'string'
      ))
      : [];
    const canonicalAsset = apkAssets.find((asset) => /^OMPChamber-.+-android\.apk$/i.test(asset.name));
    return (canonicalAsset || apkAssets[0])?.browser_download_url;
  } catch {
    return undefined;
  }
}

/**
 * 向可选的自托管更新检查 API（OMPCHAMBER_UPDATE_API_URL）上报环境并获取最新版本。
 * 未配置 API 时直接返回 null，由调用方回落到 GitHub Releases 比对。请求体含
 * appType、平台/架构（仅桌面与移动 app 类型信任客户端上报值，其余强制用宿主
 * 真实值，防伪造）、当前版本与匿名安装 ID（reportUsage 为 false 时不携带）。
 * 响应的 latestVersion 不小于当前版本才被采纳；任何网络或解析失败都静默返回
 * null。web 场景的 available 还需调用方用 GitHub 最新 tag 二次确认。
 */
async function checkForUpdatesFromApi(currentVersion, options = {}) {
  if (!UPDATE_CHECK_URL) return null;
  try {
    const appType = normalizeAppType(options.appType);
    const hostPlatform = mapPlatform(process.platform);
    const hostArch = mapArch(process.arch);
    const shouldTrustClientPlatform = appType === 'desktop-electron' || appType === 'vscode' || appType === 'mobile-capacitor';
    const platform = shouldTrustClientPlatform ? normalizePlatform(options.platform) : hostPlatform;
    const arch = shouldTrustClientPlatform ? normalizeArch(options.arch) : hostArch;
    const reportUsage = options.reportUsage !== false;
    const payload = {
      appType,
      deviceClass: normalizeDeviceClass(options.deviceClass),
      platform,
      arch,
      channel: 'stable',
      currentVersion,
      installId: reportUsage ? (options.installId || getOrCreateInstallId(appType)) : undefined,
      instanceMode: options.instanceMode || 'unknown',
      reportUsage,
    };

    const response = await fetch(UPDATE_CHECK_URL, {
      method: 'POST',
      headers: {
        Accept: 'application/json',
        'Content-Type': 'application/json',
      },
      body: JSON.stringify(payload),
      signal: AbortSignal.timeout(10000),
    });

    if (!response.ok) return null;
    const data = await response.json();
    if (typeof data?.latestVersion !== 'string') return null;

    const versionComparison = compareVersions(data.latestVersion, currentVersion);
    if (versionComparison < 0) return null;

    const releaseUrl = `${GITHUB_RELEASES_URL}/tag/v${data.latestVersion}`;
    const downloadUrl = typeof data.downloadUrl === 'string'
      ? data.downloadUrl
      : typeof data.download?.url === 'string'
        ? data.download.url
        : undefined;
    const updateAvailable = Boolean(data.updateAvailable) && versionComparison > 0;
    const mobileDownloadUrl = updateAvailable && appType === 'mobile-capacitor' && platform === 'android'
      ? await resolveAndroidApkUrl(data.latestVersion, downloadUrl)
      : undefined;
    return {
      available: updateAvailable,
      version: data.latestVersion,
      currentVersion,
      body: typeof data.releaseNotes === 'string' ? data.releaseNotes : undefined,
      releaseUrl: typeof data.releaseNotesUrl === 'string' ? data.releaseNotesUrl : releaseUrl,
      downloadUrl: mobileDownloadUrl,
      nextSuggestedCheckInSec:
        typeof data.nextSuggestedCheckInSec === 'number' && Number.isFinite(data.nextSuggestedCheckInSec)
          ? data.nextSuggestedCheckInSec
          : undefined,
    };
  } catch {
    return null;
  }
}

/** 把路径归一为可比形式：resolve + normalize，Windows 下再转小写忽略大小写；输入非法返回 null。 */
function normalizePathForComparison(filePath) {
  if (!filePath || typeof filePath !== 'string') return null;
  const normalized = path.normalize(path.resolve(filePath));
  return process.platform === 'win32' ? normalized.toLowerCase() : normalized;
}

/**
 * 收集一条路径的全部可比形式：归一后的字面路径，加上 realpathSync（优先 native
 * 版本，可穿透符号链接）解析出的真实路径。realpath 失败（目标不存在等）时静默
 * 跳过，仅保留字面路径。供安装归属判断做路径集合求交。
 */
function getComparablePaths(filePath) {
  const paths = new Set();
  const normalized = normalizePathForComparison(filePath);
  if (normalized) {
    paths.add(normalized);
  }

  try {
    const realPath = fs.realpathSync.native ? fs.realpathSync.native(filePath) : fs.realpathSync(filePath);
    const normalizedRealPath = normalizePathForComparison(realPath);
    if (normalizedRealPath) {
      paths.add(normalizedRealPath);
    }
  } catch {
  }

  return paths;
}

/** 判断两个路径集合是否存在共同元素：a 中任一成员出现在 b 中即返回 true。 */
function pathSetContains(a, b) {
  for (const value of a) {
    if (b.has(value)) {
      return true;
    }
  }
  return false;
}

/** 当前包根目录：lib 目录的上两级（即 packages/web）。 */
function getCurrentPackagePath() {
  return path.resolve(__dirname, '..', '..');
}

/** 给定某个 PM 的全局 node_modules 根，拼出本包在其下的安装目录；rootPath 为空返回 null。 */
function getPackagePathForGlobalRoot(rootPath) {
  if (!rootPath) return null;
  return path.join(rootPath, ...PACKAGE_PATH_SEGMENTS);
}

/**
 * 对路径列表去重：按 normalizePathForComparison 的归一形式判重，保留首次出现的
 * 输入（仅做 path.resolve），返回去重后的数组。
 */
function getUniquePaths(paths) {
  const seen = new Set();
  const result = [];
  for (const value of paths) {
    const normalized = normalizePathForComparison(value);
    if (!normalized || seen.has(normalized)) continue;
    seen.add(normalized);
    result.push(path.resolve(value));
  }
  return result;
}

/**
 * 同步执行命令并返回 trim 后的 stdout；退出码非 0、输出为空或抛错（命令不存在、
 * 超过 10s 超时）一律返回 null。是本模块所有探测类命令的统一出口。
 */
function getCommandOutput(command, args) {
  try {
    const result = spawnSync(command, args, {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
      timeout: 10000,
      ...getSpawnSyncBaseOptions(),
    });

    if (result.status !== 0) {
      return null;
    }

    const stdout = result.stdout.trim();
    return stdout || null;
  } catch {
    return null;
  }
}

/**
 * 枚举指定 package manager 的全局可执行文件目录（已去重）。
 * 策略：pnpm 用 `bin -g` 与 `prefix -g`；yarn 用 `global bin`；bun 用
 * `pm bin -g`；npm（default 分支）用 `prefix -g`（Windows 即该目录本身，
 * 类 Unix 需拼上 /bin）。PM 命令不可用时返回空数组。
 */
function getGlobalBinDirs(pm) {
  const pmCommand = resolvePackageManagerCommand(pm);
  if (!isCommandAvailable(pmCommand)) {
    return [];
  }

  const dirs = [];
  switch (pm) {
    case 'pnpm': {
      const pnpmBin = getCommandOutput(pmCommand, ['bin', '-g']);
      if (pnpmBin) dirs.push(pnpmBin);
      const pnpmPrefix = getCommandOutput(pmCommand, ['prefix', '-g']);
      if (pnpmPrefix) dirs.push(process.platform === 'win32' ? pnpmPrefix : path.join(pnpmPrefix, 'bin'));
      break;
    }
    case 'yarn': {
      const yarnBin = getCommandOutput(pmCommand, ['global', 'bin']);
      if (yarnBin) dirs.push(yarnBin);
      break;
    }
    case 'bun': {
      const bunBin = getCommandOutput(pmCommand, ['pm', 'bin', '-g']);
      if (bunBin) dirs.push(bunBin);
      break;
    }
    default: {
      const npmPrefix = getCommandOutput(pmCommand, ['prefix', '-g']);
      if (npmPrefix) dirs.push(process.platform === 'win32' ? npmPrefix : path.join(npmPrefix, 'bin'));
      break;
    }
  }

  return getUniquePaths(dirs);
}

/**
 * 枚举指定 package manager 的全局 node_modules 根目录（已去重）。
 * pnpm：`root -g` 加 prefix 推导；yarn：`global dir` 下拼 node_modules；
 * bun：由全局 bin 目录反推 install/global/node_modules 与上两级两个候选；
 * npm：`root -g` 加 prefix 推导。任何异常都吞掉并返回空数组，探测失败
 * 不影响后续的回退判定路径。
 */
function getGlobalNodeModulesRoots(pm) {
  try {
    const pmCommand = resolvePackageManagerCommand(pm);
    if (!isCommandAvailable(pmCommand)) {
      return [];
    }

    const roots = [];

    switch (pm) {
      case 'pnpm': {
        const pnpmRoot = getCommandOutput(pmCommand, ['root', '-g']);
        if (pnpmRoot) roots.push(pnpmRoot);
        const pnpmPrefix = getCommandOutput(pmCommand, ['prefix', '-g']);
        if (pnpmPrefix) roots.push(process.platform === 'win32' ? path.join(pnpmPrefix, 'node_modules') : path.join(pnpmPrefix, 'lib', 'node_modules'));
        break;
      }
      case 'yarn': {
        const yarnDir = getCommandOutput(pmCommand, ['global', 'dir']);
        if (yarnDir) roots.push(path.join(yarnDir, 'node_modules'));
        break;
      }
      case 'bun': {
        const bunBinDir = getCommandOutput(pmCommand, ['pm', 'bin', '-g']);
        if (bunBinDir) {
          roots.push(path.resolve(bunBinDir, '..', 'install', 'global', 'node_modules'));
          roots.push(path.resolve(bunBinDir, '..', '..', 'node_modules'));
        }
        break;
      }
      default:
      {
        const npmRoot = getCommandOutput(pmCommand, ['root', '-g']);
        if (npmRoot) roots.push(npmRoot);
        const npmPrefix = getCommandOutput(pmCommand, ['prefix', '-g']);
        if (npmPrefix) roots.push(process.platform === 'win32' ? path.join(npmPrefix, 'node_modules') : path.join(npmPrefix, 'lib', 'node_modules'));
        break;
      }
    }

    return getUniquePaths(roots);
  } catch {
    return [];
  }
}

/**
 * 在各全局 bin 目录中查找本包的启动脚本（Windows 为 ompchamber.cmd，
 * 其它平台为 ompchamber），对脚本做 realpath 后回溯两级
 * （bin 软链 → 包内 bin → 包根）得到包安装目录。目录中无脚本或 realpath
 * 失败则跳过；返回去重后的包路径数组。
 */
function getOwnedPackagePathsFromGlobalBins(pm) {
  const packagePaths = [];
  for (const binDir of getGlobalBinDirs(pm)) {
    const binaryName = process.platform === 'win32' ? 'ompchamber.cmd' : 'ompchamber';
    const binaryPath = path.join(binDir, binaryName);
    if (!fs.existsSync(binaryPath)) continue;

    try {
      const realBinaryPath = fs.realpathSync.native ? fs.realpathSync.native(binaryPath) : fs.realpathSync(binaryPath);
      packagePaths.push(path.resolve(realBinaryPath, '..', '..'));
    } catch {
    }
  }

  return getUniquePaths(packagePaths);
}

/** 用当前包安装路径做一次基于路径特征的 PM 探测（委托 detectPackageManagerFromInstallPath）。 */
function detectPackageManagerFromCurrentInstallPath() {
  return detectPackageManagerFromInstallPath(getCurrentPackagePath());
}

/**
 * 判断给定 package manager 是否"拥有"当前安装：把当前包路径的 comparable 集合，
 * 与该 PM 全局 node_modules 根下的本包目录、以及全局 bin 反查得到的包目录逐一
 * 求交。这是优先级最高的判定依据，能在多 PM 共存环境中给出可靠结论。
 */
function packageManagerOwnsCurrentInstall(pm) {
  const currentPackagePaths = getComparablePaths(getCurrentPackagePath());
  const candidatePackagePaths = [
    ...getGlobalNodeModulesRoots(pm).map(getPackagePathForGlobalRoot),
    ...getOwnedPackagePathsFromGlobalBins(pm),
  ];

  for (const candidatePath of candidatePackagePaths) {
    if (!candidatePath) continue;
    if (pathSetContains(currentPackagePaths, getComparablePaths(candidatePath))) {
      return true;
    }
  }

  return false;
}

/**
 * 探测当前安装归属的 package manager，返回完整明细：PM 名、判定原因 reason、
 * 包路径、PM 可执行命令、全局 node_modules 根。判定按优先级依次为：
 * desktop 运行时短路返回 'electron'（避免一连串 spawnSync 冻结 Electron 主循环）；
 * 进程级缓存；OMPCHAMBER_PACKAGE_MANAGER 强制指定（命令需可用）；
 * 安装路径特征加归属验证；pnpm/yarn/bun/npm 逐一归属验证；
 * npm_config_user_agent、npm_execpath、入口脚本路径等弱提示（需命令可用且能
 * 看到本包）；运行时路径提示；最后任选一个能看到本包的 PM；兜底 'npm'。
 * 成功判定的结果写入 cachedDetectedPm 供进程内复用。
 */
export function detectPackageManagerDetails() {
  // In desktop (Electron) runtime, package-manager detection is worthless —
  // the app ships as a .app bundle, not installed via npm/pnpm/yarn/bun, and
  // updates are handled by electron-updater. The detection path does up to a
  // dozen spawnSync(pm, ['bin', '-g']) calls with 10s timeouts each; under
  // the in-process server every one blocks the Electron main event loop and
  // manifests as a multi-second UI freeze. Short-circuit here.
  if (process.env.OMPCHAMBER_RUNTIME === 'desktop') {
    return {
      packageManager: 'electron',
      reason: 'desktop-runtime',
      packagePath: null,
      packageManagerCommand: null,
      globalNodeModulesRoot: null,
    };
  }

  if (cachedDetectedPm) {
      return {
        packageManager: cachedDetectedPm,
        reason: 'cached',
        packagePath: getCurrentPackagePath(),
        packageManagerCommand: resolvePackageManagerCommand(cachedDetectedPm),
        globalNodeModulesRoot: getGlobalNodeModulesRoots(cachedDetectedPm)[0] || null,
      };
  }

  const forcedPm = process.env.OMPCHAMBER_PACKAGE_MANAGER?.trim();
  if (forcedPm && ['npm', 'pnpm', 'yarn', 'bun'].includes(forcedPm)) {
    const forcedPmCommand = resolvePackageManagerCommand(forcedPm);
    if (isCommandAvailable(forcedPmCommand)) {
      cachedDetectedPm = forcedPm;
      return {
        packageManager: cachedDetectedPm,
        reason: 'forced-env',
        packagePath: getCurrentPackagePath(),
        packageManagerCommand: forcedPmCommand,
        globalNodeModulesRoot: getGlobalNodeModulesRoots(cachedDetectedPm)[0] || null,
      };
    }
  }

  // First prefer the package manager that demonstrably owns the current install.
  const installPathPm = detectPackageManagerFromCurrentInstallPath();
  if (installPathPm && packageManagerOwnsCurrentInstall(installPathPm)) {
    cachedDetectedPm = installPathPm;
    return {
      packageManager: cachedDetectedPm,
      reason: 'install-path-owner',
      packagePath: getCurrentPackagePath(),
      packageManagerCommand: resolvePackageManagerCommand(cachedDetectedPm),
      globalNodeModulesRoot: getGlobalNodeModulesRoots(cachedDetectedPm)[0] || null,
    };
  }

  const ownershipCandidates = ['pnpm', 'yarn', 'bun', 'npm'];
  for (const candidate of ownershipCandidates) {
    if (packageManagerOwnsCurrentInstall(candidate)) {
      cachedDetectedPm = candidate;
      return {
        packageManager: cachedDetectedPm,
        reason: 'global-root-owner',
        packagePath: getCurrentPackagePath(),
        packageManagerCommand: resolvePackageManagerCommand(cachedDetectedPm),
        globalNodeModulesRoot: getGlobalNodeModulesRoots(cachedDetectedPm)[0] || null,
      };
    }
  }

  // Fall back to weaker hints only when ownership cannot be established.
  const userAgent = process.env.npm_config_user_agent || '';
  let hintedPm = null;
  if (userAgent.startsWith('pnpm')) hintedPm = 'pnpm';
  else if (userAgent.startsWith('yarn')) hintedPm = 'yarn';
  else if (userAgent.startsWith('bun')) hintedPm = 'bun';
  else if (userAgent.startsWith('npm')) hintedPm = 'npm';

  // Check execpath.
  const execPath = process.env.npm_execpath || '';
  if (!hintedPm) {
    if (execPath.includes('pnpm')) hintedPm = 'pnpm';
    else if (execPath.includes('yarn')) hintedPm = 'yarn';
    else if (execPath.includes('bun')) hintedPm = 'bun';
    else if (execPath.includes('npm')) hintedPm = 'npm';
  }

  // Detect from invoked binary path.
  const invokedPm = detectPackageManagerFromInvocationPath(process.argv?.[1]);
  if (!hintedPm) {
    hintedPm = invokedPm;
  }

  if (!hintedPm) {
    hintedPm = installPathPm;
  }

  // Validate the hint against package visibility, but only after ownership checks failed.
  if (hintedPm && isCommandAvailable(resolvePackageManagerCommand(hintedPm)) && isPackageInstalledWith(hintedPm)) {
    cachedDetectedPm = hintedPm;
    return {
      packageManager: cachedDetectedPm,
      reason: 'hinted-visible-install',
      packagePath: getCurrentPackagePath(),
      packageManagerCommand: resolvePackageManagerCommand(cachedDetectedPm),
      globalNodeModulesRoot: getGlobalNodeModulesRoots(cachedDetectedPm)[0] || null,
    };
  }

  const runtimePm = detectPackageManagerFromRuntimePath(process.execPath);
  if (runtimePm && isCommandAvailable(resolvePackageManagerCommand(runtimePm)) && isPackageInstalledWith(runtimePm)) {
    cachedDetectedPm = runtimePm;
    return {
      packageManager: cachedDetectedPm,
      reason: 'runtime-visible-install',
      packagePath: getCurrentPackagePath(),
      packageManagerCommand: resolvePackageManagerCommand(cachedDetectedPm),
      globalNodeModulesRoot: getGlobalNodeModulesRoots(cachedDetectedPm)[0] || null,
    };
  }

  // Last resort: pick a PM that can at least see the package.
  const pmChecks = [
    { name: 'pnpm', check: () => isCommandAvailable(resolvePackageManagerCommand('pnpm')) },
    { name: 'yarn', check: () => isCommandAvailable(resolvePackageManagerCommand('yarn')) },
    { name: 'bun', check: () => isCommandAvailable(resolvePackageManagerCommand('bun')) },
    { name: 'npm', check: () => isCommandAvailable(resolvePackageManagerCommand('npm')) },
  ];

  for (const { name, check } of pmChecks) {
    if (check()) {
      // Verify this PM actually has the package installed globally
      if (isPackageInstalledWith(name)) {
        cachedDetectedPm = name;
        return {
          packageManager: cachedDetectedPm,
          reason: 'last-resort-visible-install',
          packagePath: getCurrentPackagePath(),
          packageManagerCommand: resolvePackageManagerCommand(cachedDetectedPm),
          globalNodeModulesRoot: getGlobalNodeModulesRoots(cachedDetectedPm)[0] || null,
        };
      }
    }
  }

  cachedDetectedPm = 'npm';
  return {
    packageManager: cachedDetectedPm,
    reason: 'default-fallback',
    packagePath: getCurrentPackagePath(),
    packageManagerCommand: resolvePackageManagerCommand(cachedDetectedPm),
    globalNodeModulesRoot: getGlobalNodeModulesRoots(cachedDetectedPm)[0] || null,
  };
}

/** 便捷封装：仅取 detectPackageManagerDetails 结果中的 packageManager 字段。 */
export function detectPackageManager() {
  return detectPackageManagerDetails().packageManager;
}

/** 按安装路径特征猜 PM：/.pnpm/ 或 /pnpm/→pnpm、/.yarn/→yarn、/.bun/ 或 /bun/install/→bun、/node_modules/→npm；无法识别返回 null。 */
function detectPackageManagerFromInstallPath(pkgPath) {
  if (!pkgPath) return null;
  const normalized = pkgPath.replace(/\\/g, '/').toLowerCase();
  if (normalized.includes('/.pnpm/') || normalized.includes('/pnpm/')) return 'pnpm';
  if (normalized.includes('/.yarn/')) return 'yarn';
  if (normalized.includes('/.bun/') || normalized.includes('/bun/install/')) return 'bun';
  if (normalized.includes('/node_modules/')) return 'npm';
  return null;
}

/** 按 Node 运行时可执行文件路径猜 PM：bun 安装目录→bun、/pnpm/→pnpm、/yarn/→yarn、node→npm；无法识别返回 null。 */
function detectPackageManagerFromRuntimePath(runtimePath) {
  if (!runtimePath || typeof runtimePath !== 'string') return null;
  const normalized = runtimePath.replace(/\\/g, '/').toLowerCase();
  if (normalized.includes('/.bun/bin/bun') || normalized.endsWith('/bun') || normalized.endsWith('/bun.exe')) {
    return 'bun';
  }
  if (normalized.includes('/pnpm/')) return 'pnpm';
  if (normalized.includes('/yarn/')) return 'yarn';
  if (normalized.includes('/node') || normalized.endsWith('/node.exe')) return 'npm';
  return null;
}

/** 按被调用的入口脚本路径（process.argv[1]）猜 PM：/.bun/bin/、/.pnpm/、/.yarn/ 三种特征；无法识别返回 null。 */
function detectPackageManagerFromInvocationPath(invokedPath) {
  if (!invokedPath || typeof invokedPath !== 'string') return null;
  const normalized = invokedPath.replace(/\\/g, '/').toLowerCase();
  if (normalized.includes('/.bun/bin/')) return 'bun';
  if (normalized.includes('/.pnpm/')) return 'pnpm';
  if (normalized.includes('/.yarn/')) return 'yarn';
  return null;
}

/**
 * 生成某个 PM 可执行命令的候选列表：bun 额外尝试 BUN_INSTALL、HOME、USERPROFILE
 * 下的绝对路径（应对未加入 PATH 的安装），末尾总是追加裸命令名。结果已去重。
 */
function getPackageManagerCommandCandidates(pm) {
  const candidates = [];
  if (pm === 'bun') {
    const bunExecutable = process.platform === 'win32' ? 'bun.exe' : 'bun';
    if (process.env.BUN_INSTALL) {
      candidates.push(path.join(process.env.BUN_INSTALL, 'bin', bunExecutable));
    }
    if (process.env.HOME) {
      candidates.push(path.join(process.env.HOME, '.bun', 'bin', bunExecutable));
    }
    if (process.env.USERPROFILE) {
      candidates.push(path.join(process.env.USERPROFILE, '.bun', 'bin', bunExecutable));
    }
  }
  candidates.push(pm);
  return [...new Set(candidates.filter(Boolean))];
}

/** 依次用 --version 检查候选命令是否可执行，返回第一个可用的；全部不可用则原样返回 PM 名。 */
function resolvePackageManagerCommand(pm) {
  const candidates = getPackageManagerCommandCandidates(pm);
  for (const candidate of candidates) {
    if (isCommandAvailable(candidate)) {
      return candidate;
    }
  }
  return pm;
}

/**
 * 为 shell 拼接给含空白的命令路径加引号：Windows 内层用 "" 转义双引号并整体
 * 包双引号，类 Unix 用 '\'' 转义单引号并整体包单引号；无空白或空值原样返回。
 */
function quoteCommand(command) {
  if (!command) return command;
  if (!/\s/.test(command)) return command;
  if (process.platform === 'win32') {
    return `"${command.replace(/"/g, '""')}"`;
  }
  return `'${command.replace(/'/g, "'\\''")}'`;
}

/** 同步运行 <command> --version 验证命令可用（5s 超时）；任何异常按不可用处理。 */
function isCommandAvailable(command) {
  try {
    const result = spawnSync(command, ['--version'], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
      timeout: 5000,
      ...getSpawnSyncBaseOptions(),
    });
    return result.status === 0;
  } catch {
    return false;
  }
}

/**
 * 判断本包是否被指定 PM 全局安装：pnpm/npm 用 `list -g --depth=0 @ompchamber/web`，
 * yarn 用 `global list --depth=0`，bun 用 `pm ls -g`；退出码为 0 且 stdout 中出现
 * 包名或 'ompchamber' 即认为已安装。任何失败返回 false。
 */
function isPackageInstalledWith(pm) {
  try {
    const pmCommand = resolvePackageManagerCommand(pm);
    let args;
    switch (pm) {
      case 'pnpm':
        args = ['list', '-g', '--depth=0', PACKAGE_NAME];
        break;
      case 'yarn':
        args = ['global', 'list', '--depth=0'];
        break;
      case 'bun':
        args = ['pm', 'ls', '-g'];
        break;
      default:
        args = ['list', '-g', '--depth=0', PACKAGE_NAME];
    }

    const result = spawnSync(pmCommand, args, {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
      timeout: 10000,
      ...getSpawnSyncBaseOptions(),
    });

    if (result.status !== 0) return false;
    return result.stdout.includes(PACKAGE_NAME) || result.stdout.includes('ompchamber');
  } catch {
    return false;
  }
}

/**
 * Get the update command for the detected package manager
 */
/**
 * 组装更新命令字符串（展示给用户或经 shell 执行）：因为本包以 GitHub release
 * tarball 分发而非 npm registry，命令固定让 PM 全局安装 tarball——指定 version 时
 * 用对应 tag 的 ompchamber-web-<version>.tgz，否则用 latest 地址。按 PM 选择
 * add -g / global add / install -g 子命令；PM 命令路径含空白时已加引号。
 */
export function getUpdateCommand(pm = detectPackageManager(), version = null) {
  const pmCommand = quoteCommand(resolvePackageManagerCommand(pm));
  const tarballUrl = typeof version === 'string' && version.trim()
    ? `https://github.com/${GITHUB_REPO}/releases/download/v${version.trim()}/ompchamber-web-${version.trim()}.tgz`
    : RELEASE_TARBALL_LATEST_URL;
  switch (pm) {
    case 'pnpm':
      return `${pmCommand} add -g ${tarballUrl}`;
    case 'yarn':
      return `${pmCommand} global add ${tarballUrl}`;
    case 'bun':
      return `${pmCommand} add -g ${tarballUrl}`;
    default:
      return `${pmCommand} install -g ${tarballUrl}`;
  }
}

/**
 * Get current installed version from package.json
 */
/** 从包根 package.json 读取当前版本号；读取或解析失败返回 'unknown'。 */
export function getCurrentVersion() {
  try {
    const pkgPath = path.resolve(__dirname, '..', '..', 'package.json');
    const pkg = JSON.parse(fs.readFileSync(pkgPath, 'utf8'));
    return pkg.version || 'unknown';
  } catch {
    return 'unknown';
  }
}

/**
 * Fetch latest published version from this repository's GitHub releases.
 */
/** 查询 GitHub Releases API 的 latest release，去掉 tag 的 'v' 前缀返回版本号；任何失败返回 null。 */
async function getLatestVersion() {
  try {
    const response = await fetch(`${GITHUB_RELEASES_API_URL}/latest`, {
      headers: { Accept: 'application/vnd.github+json' },
      signal: AbortSignal.timeout(10000),
    });

    if (!response.ok) {
      throw new Error(`GitHub releases API responded with ${response.status}`);
    }

    const data = await response.json();
    const tag = typeof data?.tag_name === 'string' ? data.tag_name.replace(/^v/, '') : '';
    return tag || null;
  } catch (error) {
    return null;
  }
}

/**
 * Compare semver-like version strings.
 */
/**
 * 把形如 v1.2.3-beta.1+build 的版本串拆为数字段数组：去掉 'v' 前缀与 '+build'
 * 元数据，prerelease 后缀不参与数值比较、仅以布尔标记保留（比较时视为小于同号正式版）。
 */
function parseVersionForComparison(value) {
  const normalized = String(value || '').replace(/^v/, '').split('+')[0];
  const prereleaseIndex = normalized.indexOf('-');
  const core = prereleaseIndex >= 0 ? normalized.slice(0, prereleaseIndex) : normalized;
  const parts = core.split('.').map((part) => {
    const parsed = Number.parseInt(part || '0', 10);
    return Number.isFinite(parsed) ? parsed : 0;
  });

  return {
    parts,
    prerelease: prereleaseIndex >= 0,
  };
}

/**
 * 逐段比较两个 semver 风格版本串：各数字段按数值比较，缺失段按 0 处理；
 * 数字段全部相同时，prerelease 版本小于正式版。返回负数/0/正数。
 */
function compareVersions(left, right) {
  const a = parseVersionForComparison(left);
  const b = parseVersionForComparison(right);
  const length = Math.max(a.parts.length, b.parts.length);

  for (let index = 0; index < length; index += 1) {
    const diff = (a.parts[index] || 0) - (b.parts[index] || 0);
    if (diff !== 0) return diff;
  }

  if (a.prerelease !== b.prerelease) {
    return a.prerelease ? -1 : 1;
  }

  return 0;
}

/**
 * Fetch changelog notes between versions
 */
/**
 * 抓取 main 分支 CHANGELOG.md，截取版本号落在 (fromVersion, toVersion] 区间的
 * `## [x.y.z]` 段落并拼接为更新说明。请求失败或找不到任何相关段落时返回
 * undefined，调用方据此省略 body 字段。
 */
async function fetchChangelogNotes(fromVersion, toVersion) {
  try {
    const response = await fetch(CHANGELOG_URL, {
      signal: AbortSignal.timeout(10000),
    });

    if (!response.ok) return undefined;

    const changelog = await response.text();
    const sections = changelog.split(/^## /m).slice(1);

    const relevantSections = sections.filter((section) => {
      const match = section.match(/^\[(\d+\.\d+\.\d+)\]/);
      if (!match) return false;
      return compareVersions(match[1], fromVersion) > 0 && compareVersions(match[1], toVersion) <= 0;
    });

    if (relevantSections.length === 0) return undefined;

    return relevantSections
      .map((s) => '## ' + s.trim())
      .join('\n\n');
  } catch {
    return undefined;
  }
}

/**
 * 更新检查主入口。流程：先尝试自托管更新 API（需设置 OMPCHAMBER_UPDATE_API_URL），
 * 其结果在 web 场景还要与 GitHub latest tag 交叉验证（API 报告版本超前于已发布
 * release 时降级为不可用）；API 不可用或未配置时回落到 GitHub Releases 比对，
 * 并附带 changelog 说明。Android 移动端在有更新时额外解析 APK 下载地址。
 * 返回结果统一携带 packageManager 与固定为 'ompchamber update' 的 updateCommand
 * （向用户展示 CLI 命令而非原始 PM 命令）。版本无法确定时返回
 * available:false 及 error 字段，不抛错。
 */
export async function checkForUpdates(options = {}) {
  const currentVersion = options.currentVersion || getCurrentVersion();
  const pm = detectPackageManager();
  const appType = normalizeAppType(options.appType);
  const platform = normalizePlatform(options.platform);

  if (currentVersion !== 'unknown') {
    const remote = await checkForUpdatesFromApi(currentVersion, options);
    if (remote) {
      if (remote.available && appType === 'web') {
        const npmLatest = await getLatestVersion();
        if (!npmLatest || compareVersions(npmLatest, remote.version) < 0) {
          remote.available = false;
        }
      }
      return {
        ...remote,
        packageManager: pm,
        updateCommand: 'ompchamber update',
      };
    }
  }

  const latestVersion = await getLatestVersion();

  if (!latestVersion || currentVersion === 'unknown') {
    return {
      available: false,
      currentVersion,
      error: 'Unable to determine versions',
    };
  }

  const available = compareVersions(latestVersion, currentVersion) > 0;
  let changelog;
  let downloadUrl;
  if (available) {
    changelog = await fetchChangelogNotes(currentVersion, latestVersion);
    if (appType === 'mobile-capacitor' && platform === 'android') {
      downloadUrl = await resolveAndroidApkUrl(latestVersion);
    }
  }

  return {
    available,
    version: latestVersion,
    currentVersion,
    body: changelog,
    releaseUrl: `${GITHUB_RELEASES_URL}/tag/v${latestVersion}`,
    downloadUrl,
    packageManager: pm,
    // Show our CLI command, not raw package manager command
    updateCommand: 'ompchamber update',
  };
}
/**
 * 实际执行更新：经 getUpdateCommand 得到命令后同步经 shell 运行（stdio 直通
 * 终端）。非 silent 模式先打印提示与即将运行的命令。返回 { success, exitCode }，
 * 命令失败不抛错。
 */
export function executeUpdate(pm = detectPackageManager(), options = {}) {
  const command = getUpdateCommand(pm, options?.version);
  if (!options?.silent) {
    console.log(`Updating ${PACKAGE_NAME} using ${pm}...`);
    console.log(`Running: ${command}`);
  }

  const result = spawnSync(command, {
    stdio: 'inherit',
    shell: true,
    ...getSpawnSyncBaseOptions(),
  });

  return {
    success: result.status === 0,
    exitCode: result.status,
  };
}
