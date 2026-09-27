/**
 * GitHub 集成的聚合出口（barrel 模块）。
 *
 * 汇总再导出认证（auth.js）、OAuth device flow（device-flow.js）、
 * Octokit 工厂（octokit.js）与 remote/仓库解析（repo/index.js）的公开
 * 接口，上层统一从本模块导入。
 */
export {
  getGitHubAuth,
  getGitHubAuthAccounts,
  setGitHubAuth,
  activateGitHubAuth,
  clearGitHubAuth,
  getGitHubClientId,
  getGitHubScopes,
  GH_CLI_ACCOUNT_ID,
  isGhCliDisabled,
  isGhCliActive,
  setGhCliActive,
  setGhCliDisabled,
  GITHUB_AUTH_FILE,
} from './auth.js';

/** OAuth device flow 的两个端点封装（来自 device-flow.js）。 */
export {
  startDeviceFlow,
  exchangeDeviceCode,
} from './device-flow.js';

/** Octokit 实例工厂（来自 octokit.js）。 */
export {
  getOctokitOrNull,
  createOctokit,
} from './octokit.js';

/** remote URL 解析与目录级 GitHub 仓库解析（来自 repo/index.js）。 */
export {
  parseGitHubRemoteUrl,
  resolveGitHubRepoFromDirectory,
} from './repo/index.js';
