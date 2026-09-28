/**
 * Linear issue 的 GraphQL 数据层。
 *
 * 在 client.js 的 fetchLinearGraphql 之上实现 issue 的列表/搜索/详情/
 * 状态更新/评论，以及 team workflow 状态列表；所有出参都经严格的
 * readXxx 读取器清洗，保证结构稳定且绝不泄漏 token。未连接或 token
 * 失效（401）时统一返回 { connected: false } 并清除对应凭据。
 */
import { clearLinearAuth, getLinearAuth, getLinearAuthByWorkspaceId } from './auth.js';
import { fetchLinearGraphql, getValidLinearAccessToken } from './client.js';
import { isPlainObject, isString, readFiniteNumber, readTrimmedString } from './parse.js';

/** 列表/搜索分页的每页条数。 */
const PAGE_SIZE = 50;

/**
 * 面板状态到 Linear IssueFilter.state 的映射：优先按 workflow type
 * 过滤，个别状态（In Review、Duplicate）按工作流名称匹配。
 */
const LIST_STATUS_STATE = {
  open: { type: { nin: ['completed', 'canceled', 'duplicate'] } },
  backlog: { type: { eq: 'backlog' } },
  todo: { type: { eq: 'unstarted' } },
  started: { type: { eq: 'started' }, name: { neqIgnoreCase: 'In Review' } },
  inReview: { name: { eqIgnoreCase: 'In Review' } },
  completed: { type: { eq: 'completed' } },
  canceled: { type: { eq: 'canceled' }, name: { neqIgnoreCase: 'Duplicate' } },
  duplicate: { or: [{ type: { eq: 'duplicate' } }, { name: { eqIgnoreCase: 'Duplicate' } }] },
};

/** 读取列表状态参数：仅接受 all 与 LIST_STATUS_STATE 的键，其余归一为 open。 */
function readListStatus(value) {
  const status = readTrimmedString(value);
  if (status === 'all' || Object.hasOwn(LIST_STATUS_STATE, status)) {
    return status;
  }
  return 'open';
}

/** 读取负责人参数：仅 me/any 合法，其余归一为 any（不过滤）。 */
function readListAssignee(value) {
  const assignee = readTrimmedString(value);
  if (assignee === 'me' || assignee === 'any') {
    return assignee;
  }
  return 'any';
}

/** 面板优先级名到 Linear 数值优先级的映射（0=none、1=urgent … 4=low）。 */
const LIST_PRIORITY_EQ = {
  none: 0,
  urgent: 1,
  high: 2,
  medium: 3,
  low: 4,
};

/** 读取优先级参数：仅接受五档之一，其余归一为 all（不过滤）。 */
function readListPriority(value) {
  const priority = readTrimmedString(value);
  if (priority === 'none' || priority === 'urgent' || priority === 'high' || priority === 'medium' || priority === 'low') {
    return priority;
  }
  return 'all';
}

/**
 * 组装列表的 IssueFilter：status/assignee/team/priority 各自可选，
 * 全部缺省时返回 undefined（不带 filter，查询全部 issue）。
 */
function buildIssueListFilter({ status, assignee, teamId, priority } = {}) {
  const filter = {};
  const resolvedStatus = readListStatus(status);
  const resolvedAssignee = readListAssignee(assignee);
  const resolvedPriority = readListPriority(priority);
  const team = readTrimmedString(teamId);
  if (resolvedStatus !== 'all') {
    filter.state = LIST_STATUS_STATE[resolvedStatus];
  }
  if (resolvedAssignee === 'me') {
    filter.assignee = { isMe: { eq: true } };
  }
  if (team) {
    filter.team = { id: { eq: team } };
  }
  if (resolvedPriority !== 'all') {
    filter.priority = { eq: LIST_PRIORITY_EQ[resolvedPriority] };
  }
  return Object.keys(filter).length > 0 ? filter : undefined;
}

/** 各查询复用的 issue 摘要字段片段（不含 description 与 comments）。 */
const ISSUE_SUMMARY_FIELDS = `
  id
  identifier
  title
  url
  priority
  state { id name type }
  assignee { name displayName avatarUrl }
  team { id key name }
  labels { nodes { id name color } }
`;
/** 按 updatedAt 排序分页列出 issue 的 GraphQL 查询。 */
const LIST_QUERY = `
  query ListLinearIssues($first: Int!, $after: String, $filter: IssueFilter) {
    issues(first: $first, after: $after, filter: $filter, orderBy: updatedAt) {
      nodes { ${ISSUE_SUMMARY_FIELDS} }
      pageInfo { hasNextPage endCursor }
    }
  }
`;
/** 按关键词全文搜索 issue 的 GraphQL 查询。 */
const SEARCH_QUERY = `
  query SearchLinearIssues($term: String!, $first: Int!, $after: String, $filter: IssueFilter) {
    searchIssues(term: $term, first: $first, after: $after, filter: $filter) {
      nodes { ${ISSUE_SUMMARY_FIELDS} }
      pageInfo { hasNextPage endCursor }
    }
  }
`;
/** 按 id/identifier 取单条 issue（含 description 与前 50 条评论）的查询。 */
const GET_QUERY = `
  query GetLinearIssue($id: String!) {
    issue(id: $id) {
      ${ISSUE_SUMMARY_FIELDS}
      description
      comments(first: 50) {
        nodes {
          id
          body
          createdAt
          user { name displayName avatarUrl }
        }
      }
    }
  }
`;
/** 在 issue 上创建评论的 GraphQL mutation。 */
const COMMENT_CREATE = `
  mutation CommentCreate($input: CommentCreateInput!) {
    commentCreate(input: $input) {
      success
      comment { id }
    }
  }
`;
/** 查询 team workflow 状态列表的 GraphQL 查询。 */
const STATES_QUERY = `
  query TeamWorkflowStates($id: String!) {
    team(id: $id) {
      states(first: 50) {
        nodes { id name type position }
      }
    }
  }
`;
/** 更新 issue（当前仅 stateId）并回读完整 issue 的 GraphQL mutation。 */
const ISSUE_UPDATE = `
  mutation IssueUpdate($id: String!, $input: IssueUpdateInput!) {
    issueUpdate(id: $id, input: $input) {
      success
      issue {
        ${ISSUE_SUMMARY_FIELDS}
        description
        comments(first: 50) {
          nodes {
            id
            body
            createdAt
            user { name displayName avatarUrl }
          }
        }
      }
    }
  }
`;
/** Linear issue 标识符（如 ENG-12）的正则形态。 */
const IDENTIFIER_RE = /^[A-Za-z][A-Za-z0-9]*-\d+$/;
/** Linear 实体 UUID 的正则形态。 */
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
/** 从 linear.app 的 issue URL 提取标识符（容忍 workspace 前缀与标题后缀）。 */
const URL_IDENTIFIER_RE = /linear\.app\/(?:[^/]+\/)?issue\/([A-Za-z][A-Za-z0-9]*-\d+)/i;

/**
 * 解析用户输入的 issue 引用：URL 与裸标识符归一为大写 identifier，
 * UUID 归一为小写 id；无法识别返回 null。
 */
export function parseLinearIssueRef(value) {
  const trimmed = readTrimmedString(value);
  if (!trimmed) return null;
  const urlMatch = trimmed.match(URL_IDENTIFIER_RE);
  if (urlMatch) {
    return { kind: 'identifier', value: urlMatch[1].toUpperCase() };
  }
  if (IDENTIFIER_RE.test(trimmed)) {
    return { kind: 'identifier', value: trimmed.toUpperCase() };
  }
  if (UUID_RE.test(trimmed)) {
    return { kind: 'id', value: trimmed.toLowerCase() };
  }
  return null;
}

/** 清洗 workflow state 节点；id/name/type 全部缺失时返回 null。 */
function readState(value) {
  if (!isPlainObject(value)) return null;
  const id = readTrimmedString(value.id) || null;
  const name = readTrimmedString(value.name) || null;
  const type = readTrimmedString(value.type) || null;
  if (!id && !name && !type) return null;
  return { id, name, type };
}

/** workflow type 到展示排序权重的映射（triage 最前、canceled 最后）。 */
const WORKFLOW_TYPE_ORDER = {
  triage: 0,
  backlog: 1,
  unstarted: 2,
  started: 3,
  completed: 4,
  canceled: 5,
};

/** 取 workflow type 的排序权重；未知 type 一律排最后（99）。 */
function workflowTypeRank(type) {
  if (type === 'triage' || type === 'backlog' || type === 'unstarted' || type === 'started' || type === 'completed' || type === 'canceled') {
    return WORKFLOW_TYPE_ORDER[type];
  }
  return 99;
}

/** 状态排序比较器：先按 type 权重、再按 position、最后按名称字典序。 */
function compareWorkflowStates(left, right) {
  const typeDelta = workflowTypeRank(left.type) - workflowTypeRank(right.type);
  if (typeDelta !== 0) return typeDelta;
  if (left.position !== right.position) return left.position - right.position;
  return left.name.localeCompare(right.name);
}

/** 清洗 workflow 状态节点；缺 id 或 name 判为无效，position 缺省为 0。 */
function readWorkflowState(value) {
  if (!isPlainObject(value)) return null;
  const id = readTrimmedString(value.id);
  const name = readTrimmedString(value.name);
  if (!id || !name) return null;
  const position = readFiniteNumber(value.position);
  return {
    id,
    name,
    type: readTrimmedString(value.type) || null,
    position: position ?? 0,
  };
}

/** 清洗 assignee 节点；name/displayName/avatarUrl 全缺时返回 null。 */
function readAssignee(value) {
  if (!isPlainObject(value)) return null;
  const name = readTrimmedString(value.name) || null;
  const displayName = readTrimmedString(value.displayName) || null;
  const avatarUrl = readTrimmedString(value.avatarUrl) || null;
  if (!name && !displayName && !avatarUrl) return null;
  return { name, displayName, avatarUrl };
}

/** 清洗 team 节点；id/key/name 任一缺失返回 null。 */
function readTeam(value) {
  if (!isPlainObject(value)) return null;
  const id = readTrimmedString(value.id);
  const key = readTrimmedString(value.key);
  const name = readTrimmedString(value.name);
  if (!id || !key || !name) return null;
  return { id, key, name };
}

/** 清洗优先级：仅接受 0–4 的整数，其余返回 null。 */
function readPriority(value) {
  if (!Number.isInteger(value) || value < 0 || value > 4) return null;
  return value;
}

/** 清洗 label 颜色为 #rrggbb 小写格式；非法颜色返回 null。 */
function readLabelColor(value) {
  const raw = readTrimmedString(value);
  if (!raw) return null;
  const hex = raw.startsWith('#') ? raw.slice(1) : raw;
  if (!/^[0-9A-Fa-f]{6}$/.test(hex)) return null;
  return `#${hex.toLowerCase()}`;
}

/** 清洗 label 节点；缺 id 或 name 返回 null，color 允许为 null。 */
function readLabel(value) {
  if (!isPlainObject(value)) return null;
  const id = readTrimmedString(value.id);
  const name = readTrimmedString(value.name);
  if (!id || !name) return null;
  return {
    id,
    name,
    color: readLabelColor(value.color),
  };
}

/** 兼容 { nodes: [...] } 连接结构与裸数组两种形态，清洗并剔除无效 label。 */
function readLabels(value) {
  const nodes = isPlainObject(value) && Array.isArray(value.nodes)
    ? value.nodes
    : Array.isArray(value)
      ? value
      : [];
  return nodes.map(readLabel).filter(Boolean);
}

/**
 * 清洗 issue 摘要（列表与详情共用）；id/identifier/title/url 任一缺失
 * 判为无效返回 null，其余字段各自独立清洗、允许为 null。
 */
function readIssueSummary(node) {
  if (!isPlainObject(node)) return null;
  const id = readTrimmedString(node.id);
  const identifier = readTrimmedString(node.identifier);
  const title = readTrimmedString(node.title);
  const url = readTrimmedString(node.url);
  if (!id || !identifier || !title || !url) return null;
  return {
    id,
    identifier,
    title,
    url,
    state: readState(node.state),
    assignee: readAssignee(node.assignee),
    team: readTeam(node.team),
    priority: readPriority(node.priority),
    labels: readLabels(node.labels),
  };
}

/** 清洗评论节点；缺 id 返回 null，作者仅保留可展示字段。 */
function readComment(node) {
  if (!isPlainObject(node)) return null;
  const id = readTrimmedString(node.id);
  if (!id) return null;
  const body = isString(node.body) ? node.body : '';
  const user = isPlainObject(node.user)
    ? {
      name: readTrimmedString(node.user.name) || null,
      displayName: readTrimmedString(node.user.displayName) || null,
      avatarUrl: readTrimmedString(node.user.avatarUrl) || null,
    }
    : null;
  return {
    id,
    body,
    createdAt: readTrimmedString(node.createdAt) || null,
    user: user && (user.name || user.displayName) ? user : null,
  };
}

/** 清洗完整 issue（摘要 + description + 评论数组），无效时返回 null。 */
function readIssue(node) {
  const summary = readIssueSummary(node);
  if (!summary) return null;
  const commentsPayload = isPlainObject(node.comments) ? node.comments.nodes : null;
  const comments = Array.isArray(commentsPayload)
    ? commentsPayload.map(readComment).filter(Boolean)
    : [];
  return {
    ...summary,
    description: isString(node.description) ? node.description : null,
    comments,
  };
}

/** 读取分页信息；结构缺失时返回 { hasMore: false, cursor: null }。 */
function readPageInfo(connection) {
  const pageInfo = isPlainObject(connection) ? connection.pageInfo : null;
  if (!isPlainObject(pageInfo)) {
    return { hasMore: false, cursor: null };
  }
  return {
    hasMore: pageInfo.hasNextPage === true,
    cursor: readTrimmedString(pageInfo.endCursor) || null,
  };
}

/** 读取连接结构的 nodes 并清洗为 issue 摘要数组，剔除无效节点。 */
function readIssueNodes(connection) {
  const nodes = isPlainObject(connection) ? connection.nodes : null;
  if (!Array.isArray(nodes)) return [];
  return nodes.map(readIssueSummary).filter(Boolean);
}

/**
 * 统一的 token 获取与 401 处理包装：拿不到有效 token 直接返回
 * { connected: false }；执行中收到 401 则清除对应 workspace 的凭据
 * 并同样返回未连接；其它错误原样抛出。
 */
async function withLinearToken(run, workspaceId) {
  try {
    const token = await getValidLinearAccessToken(workspaceId);
    if (!token) {
      return { connected: false };
    }
    return await run(token);
  } catch (error) {
    if (error?.status === 401) {
      const failed = workspaceId
        ? getLinearAuthByWorkspaceId(workspaceId)
        : getLinearAuth();
      clearLinearAuth(failed?.workspaceId || workspaceId);
      return { connected: false };
    }
    throw error;
  }
}

/** 按 ref（identifier 或 UUID）查询单条 issue 并清洗。 */
async function fetchIssueByRef(token, ref) {
  const data = await fetchLinearGraphql(token, GET_QUERY, { id: ref.value });
  return readIssue(data.issue);
}

/**
 * 列出/搜索 Linear issue（面向面板列表）。
 * query 是 issue 引用（identifier/URL/UUID）时直接按 id 精确查询并
 * 忽略其它过滤条件；是普通关键词时走 searchIssues；为空时按 updatedAt
 * 分页列出。返回 { connected, issues, cursor, hasMore }。
 */
export async function listLinearIssues({ query, cursor, status, assignee, teamId, priority } = {}) {
  return withLinearToken(async (token) => {
    const ref = parseLinearIssueRef(query);
    if (ref) {
      const issue = await fetchIssueByRef(token, ref);
      return {
        connected: true,
        issues: issue ? [issue] : [],
        cursor: null,
        hasMore: false,
      };
    }

    const after = readTrimmedString(cursor) || null;
    const term = readTrimmedString(query);
    const filter = buildIssueListFilter({ status, assignee, teamId, priority });
    const variables = {
      first: PAGE_SIZE,
    };
    if (filter) {
      variables.filter = filter;
    }
    if (after) {
      variables.after = after;
    }

    if (term) {
      variables.term = term;
      const data = await fetchLinearGraphql(token, SEARCH_QUERY, variables);
      const connection = isPlainObject(data.searchIssues) ? data.searchIssues : null;
      const page = readPageInfo(connection);
      return {
        connected: true,
        issues: readIssueNodes(connection),
        cursor: page.cursor,
        hasMore: page.hasMore,
      };
    }

    const data = await fetchLinearGraphql(token, LIST_QUERY, variables);
    const connection = isPlainObject(data.issues) ? data.issues : null;
    const page = readPageInfo(connection);
    return {
      connected: true,
      issues: readIssueNodes(connection),
      cursor: page.cursor,
      hasMore: page.hasMore,
    };
  });
}

/**
 * 按 id 或 identifier 获取单条 issue（含 description 与评论）；
 * 引用无法识别时返回 { connected: true, issue: null }。
 */
export async function getLinearIssue(id) {
  const ref = parseLinearIssueRef(id) || (readTrimmedString(id) ? { kind: 'id', value: readTrimmedString(id) } : null);
  if (!ref) {
    return { connected: true, issue: null };
  }
  return withLinearToken(async (token) => {
    const issue = await fetchIssueByRef(token, ref);
    return { connected: true, issue };
  });
}

/**
 * 列出某 team 的 workflow 状态并按 Linear 顺序排序（type 权重 →
 * position → 名称）；teamId 缺失抛出 code 为 INVALID 的错误。
 */
export async function listLinearIssueStates(teamId) {
  const id = readTrimmedString(teamId);
  if (!id) {
    const error = new Error('teamId is required');
    error.code = 'INVALID';
    throw error;
  }
  return withLinearToken(async (token) => {
    const data = await fetchLinearGraphql(token, STATES_QUERY, { id });
    const team = isPlainObject(data.team) ? data.team : null;
    const connection = isPlainObject(team) ? team.states : null;
    const nodes = isPlainObject(connection) && Array.isArray(connection.nodes)
      ? connection.nodes
      : [];
    const states = nodes
      .map(readWorkflowState)
      .filter(Boolean)
      .sort(compareWorkflowStates);
    return { connected: true, states };
  });
}

/**
 * 更新 issue 的 workflow 状态：id 可为 UUID 或 identifier（后者先解析
 * 成 UUID），成功后返回更新后的完整 issue；参数缺失抛 INVALID，
 * issue 不存在时返回 { connected: true, issue: null }。
 */
export async function updateLinearIssue({ id, stateId } = {}) {
  const issueId = readTrimmedString(id);
  const nextStateId = readTrimmedString(stateId);
  if (!issueId || !nextStateId) {
    const error = new Error('id and stateId are required');
    error.code = 'INVALID';
    throw error;
  }
  const ref = parseLinearIssueRef(issueId) || { kind: 'id', value: issueId };
  return withLinearToken(async (token) => {
    const resolved = ref.kind === 'identifier'
      ? await fetchIssueByRef(token, ref)
      : null;
    const resolvedId = resolved?.id || (ref.kind === 'id' ? ref.value : '');
    if (!resolvedId) {
      return { connected: true, issue: null };
    }
    const data = await fetchLinearGraphql(token, ISSUE_UPDATE, {
      id: resolvedId,
      input: { stateId: nextStateId },
    });
    const payload = isPlainObject(data.issueUpdate) ? data.issueUpdate : null;
    return {
      connected: true,
      issue: payload ? readIssue(payload.issue) : null,
    };
  });
}

/**
 * 在指定 issue 上创建评论：issueId 可为 identifier/URL/UUID，body 为
 * 空或 issue 不存在时静默返回 { connected: true, comment: null }，
 * 成功返回新评论 id。organizationId 用于多 workspace 时选择对应凭据。
 */
export async function createLinearIssueComment({ issueId, body, organizationId } = {}) {
  const text = isString(body) ? body : '';
  const ref = parseLinearIssueRef(issueId)
    || (readTrimmedString(issueId) ? { kind: 'id', value: readTrimmedString(issueId) } : null);
  if (!ref || !text.trim()) {
    return { connected: true, comment: null };
  }
  return withLinearToken(async (token) => {
    const issue = await fetchIssueByRef(token, ref);
    if (!issue) {
      return { connected: true, comment: null };
    }
    const data = await fetchLinearGraphql(token, COMMENT_CREATE, {
      input: { issueId: issue.id, body: text },
    });
    const payload = isPlainObject(data.commentCreate) ? data.commentCreate : null;
    const comment = isPlainObject(payload?.comment) ? payload.comment : null;
    const id = comment ? readTrimmedString(comment.id) : '';
    return {
      connected: true,
      comment: id ? { id } : null,
    };
  }, organizationId);
}
