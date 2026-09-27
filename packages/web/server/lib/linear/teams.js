/**
 * Linear 团队列表查询模块。
 *
 * 通过 GraphQL 分页拉取当前组织全部团队（id/key/name），供团队-项目路径映射
 * 与前端团队选择器使用；token 失效时清除本地凭据并返回未连接状态。
 */
import { clearLinearAuth, getLinearAuth } from './auth.js';
import { fetchLinearGraphql, getValidLinearAccessToken } from './client.js';
import { isPlainObject, readTrimmedString } from './parse.js';

/** 分页查询 Linear 团队列表的 GraphQL 查询（按 cursor 翻页）。 */
const TEAMS_QUERY = `
  query ListLinearTeams($first: Int!, $after: String) {
    teams(first: $first, after: $after) {
      nodes { id key name }
      pageInfo { hasNextPage endCursor }
    }
  }
`;
/** 每页拉取的团队数量。 */
const PAGE_SIZE = 50;
/** 最多翻页次数（防呆上限，防止死循环拉取）。 */
const MAX_PAGES = 20;

/**
 * 将 GraphQL 节点规范为团队对象（id/key/name）；
 * 入参非普通对象或任一字段缺失时返回 null（该节点被跳过）。
 */
function readTeam(node) {
  if (!isPlainObject(node)) {
    return null;
  }
  const id = readTrimmedString(node.id);
  const key = readTrimmedString(node.key);
  const name = readTrimmedString(node.name);
  if (!id || !key || !name) {
    return null;
  }
  return { id, key, name };
}

/**
 * 拉取当前组织的全部 Linear 团队（自动翻页，单页 PAGE_SIZE、至多 MAX_PAGES 页）。
 * 未连接（无有效 token）返回 { connected: false }；请求遇 401 时清除当前
 * workspace 凭据并同样返回未连接；其余错误原样抛出。
 * @returns {Promise<{ connected: boolean, teams?: Array<{ id: string, key: string, name: string }> }>}
 */
export async function listLinearTeams() {
  try {
    const token = await getValidLinearAccessToken();
    if (!token) {
      return { connected: false };
    }

    const teams = [];
    let after = null;
    for (let page = 0; page < MAX_PAGES; page += 1) {
      const variables = { first: PAGE_SIZE };
      if (after) {
        variables.after = after;
      }
      const data = await fetchLinearGraphql(token, TEAMS_QUERY, variables);
      const connection = isPlainObject(data.teams) ? data.teams : null;
      const nodes = isPlainObject(connection) && Array.isArray(connection.nodes)
        ? connection.nodes
        : [];
      for (const node of nodes) {
        const team = readTeam(node);
        if (team) {
          teams.push(team);
        }
      }
      const pageInfo = isPlainObject(connection) ? connection.pageInfo : null;
      if (!isPlainObject(pageInfo) || pageInfo.hasNextPage !== true) {
        break;
      }
      after = readTrimmedString(pageInfo.endCursor);
      if (!after) {
        break;
      }
    }

    return { connected: true, teams };
  } catch (error) {
    if (error?.status === 401) {
      clearLinearAuth(getLinearAuth()?.workspaceId);
      return { connected: false };
    }
    throw error;
  }
}
