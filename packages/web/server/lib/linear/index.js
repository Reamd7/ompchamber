/**
 * Linear 集成的统一出口（barrel）模块。
 *
 * 汇总导出授权持久化（auth.js）、OAuth 流程（oauth.js）、GraphQL 客户端（client.js）、
 * issue 操作（issues.js）、团队列表（teams.js）、路径映射（mapping.js）与
 * 会话状态评论（status.js）的公开 API，供路由层经动态 import 一次性获取。
 */
/** 授权与配置（auth.js）：凭据读写、workspace 激活/清除与 OAuth 配置读取。 */
export {
  getLinearAuth,
  getLinearAuthByWorkspaceId,
  getLinearAuthWorkspaces,
  setLinearAuth,
  activateLinearAuth,
  clearLinearAuth,
  toLinearPublicStatus,
  getLinearClientId,
  getLinearClientSecret,
  getLinearScopes,
  getLinearBrokerUrl,
  getLinearRedirectUri,
  isLinearAccessTokenStale,
  getLinearAuthFilePath,
  getLinearSessionCommentsEnabled,
  setLinearSessionCommentsEnabled,
  DEFAULT_LINEAR_CLIENT_ID_VALUE,
} from './auth.js';

/** OAuth 授权流程（oauth.js）：发起、回调消费、broker 轮询、刷新、撤销与统一错误类型。 */
export {
  startAuthorization,
  consumeAuthorizationCallback,
  pollAuthorizationBroker,
  completeAuthorizationBroker,
  refreshAccessToken,
  revokeToken,
  LinearOAuthError,
} from './oauth.js';

/** GraphQL 客户端（client.js）：身份查询与有效 access token 获取（含自动刷新）。 */
export {
  fetchLinearIdentity,
  getValidLinearAccessToken,
  LinearApiError,
} from './client.js';

/** issue 操作（issues.js）：列表、详情、工作流状态查询与状态更新。 */
export {
  listLinearIssues,
  getLinearIssue,
  listLinearIssueStates,
  updateLinearIssue,
} from './issues.js';

/** 团队列表（teams.js）。 */
export {
  listLinearTeams,
} from './teams.js';

/** 团队-项目路径映射（mapping.js）：读写、合并视图与按团队解析项目路径。 */
export {
  LinearMappingError,
  getLinearMappingFilePath,
  mergeLinearMappingView,
  readStoredLinearMapping,
  resolveMappedProjectPath,
  setStoredLinearMapping,
} from './mapping.js';

/** 会话状态评论（status.js）：发布入口、公网 origin 校验与统一错误类型。 */
export {
  LinearSessionStatusError,
  isPublicSessionOrigin,
  postLinearSessionStatus,
} from './status.js';
