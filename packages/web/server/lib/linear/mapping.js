/**
 * Linear 团队与本地项目路径映射的持久化模块。
 *
 * 以 linear-mapping.json 存储"Linear 团队 → OpenChamber 项目路径"映射
 * （支持默认路径与按团队覆盖），并按 workspace（组织）分区隔离；
 * 同时提供把存储映射与 Linear 实时团队数据合并为前端视图、
 * 以及按团队 id/key 解析项目路径的能力。
 */
import fs from 'fs';
import path from 'path';
import { getLinearAuth, getLinearAuthFilePath } from './auth.js';
import { isPlainObject, readTrimmedString } from './parse.js';

/**
 * 映射读写流程统一错误类型：code 为 INVALID 表示请求体不合法（路由层映射 400），
 * MALFORMED 表示本地映射文件损坏（映射 500）。
 */
export class LinearMappingError extends Error {
  /**
   * @param {string} message 人类可读的错误信息
   * @param {string} code 机器可读错误码（INVALID 或 MALFORMED）
   */
  constructor(message, code) {
    super(message);
    this.name = 'LinearMappingError';
    this.code = code;
  }
}

/** 返回映射存储文件 linear-mapping.json 的路径（与授权文件同目录）。 */
function mappingFile() {
  return path.join(path.dirname(getLinearAuthFilePath()), 'linear-mapping.json');
}

/** 未连接 Linear（无 workspaceId）时使用的映射分区兜底键。 */
const UNSCOPED_MAPPING_KEY = '__unscoped__';

/** 计算当前映射分区键：优先取当前激活授权的 workspaceId，否则使用兜底键。 */
function mappingOrgKey() {
  const auth = getLinearAuth();
  return readTrimmedString(auth?.workspaceId) || UNSCOPED_MAPPING_KEY;
}

/** 构造一个空映射（无默认项目路径、无团队覆盖）。 */
function emptyMapping() {
  return {
    defaultProjectPath: null,
    teamProjectPaths: {},
  };
}

/**
 * 规范化 teamId → projectPath 映射对象：逐条 trim 并过滤键或值为空
 * （或非字符串）的条目，返回干净的新对象。
 */
function readTeamProjectPaths(value) {
  if (!isPlainObject(value)) {
    return {};
  }
  const next = {};
  for (const key of Object.keys(value)) {
    const teamId = readTrimmedString(key);
    const projectPath = readTrimmedString(value[key]);
    if (teamId && projectPath) {
      next[teamId] = projectPath;
    }
  }
  return next;
}

/** 将单个 workspace 的原始映射规范为 { defaultProjectPath, teamProjectPaths } 结构。 */
function normalizeMappingSlice(raw) {
  if (!isPlainObject(raw)) {
    return emptyMapping();
  }
  return {
    defaultProjectPath: readTrimmedString(raw.defaultProjectPath) || null,
    teamProjectPaths: readTeamProjectPaths(raw.teamProjectPaths),
  };
}

/**
 * 将磁盘上的原始 JSON 规范为按 workspace 分区的文档结构：
 * 含 workspaces 对象时逐分区规范化；旧版扁平结构（顶层即映射对象）
 * 则整体归入当前分区键下。
 */
function readMappingDocument(raw) {
  if (!isPlainObject(raw)) {
    return { workspaces: {} };
  }
  if (isPlainObject(raw.workspaces)) {
    const workspaces = {};
    for (const key of Object.keys(raw.workspaces)) {
      const orgKey = readTrimmedString(key);
      if (!orgKey) continue;
      workspaces[orgKey] = normalizeMappingSlice(raw.workspaces[key]);
    }
    return { workspaces };
  }
  return {
    workspaces: {
      [mappingOrgKey()]: normalizeMappingSlice(raw),
    },
  };
}

/**
 * 以接近原子的方式写入 JSON：先写带 pid/时间戳后缀的临时文件（权限 0600），
 * 再 rename 覆盖目标文件；目录不存在时递归创建，权限设置失败仅尽力而为。
 */
function writeJsonFile(filePath, payload) {
  const dir = path.dirname(filePath);
  if (!fs.existsSync(dir)) {
    fs.mkdirSync(dir, { recursive: true });
  }
  const tmpFile = `${filePath}.${process.pid}.${Date.now()}.tmp`;
  fs.writeFileSync(tmpFile, JSON.stringify(payload, null, 2), 'utf8');
  try {
    fs.chmodSync(tmpFile, 0o600);
  } catch {
    // best-effort
  }
  fs.renameSync(tmpFile, filePath);
  try {
    fs.chmodSync(filePath, 0o600);
  } catch {
    // best-effort
  }
}

/** 返回映射存储文件路径（供诊断与测试使用）。 */
export function getLinearMappingFilePath() {
  return mappingFile();
}

/**
 * 读取当前 workspace 的映射：文件不存在或为空返回空映射；
 * JSON 损坏或顶层不是普通对象抛 MALFORMED；最后按 mappingOrgKey 取出对应分区。
 * @returns {{ defaultProjectPath: string | null, teamProjectPaths: object }}
 */
export function readStoredLinearMapping() {
  const filePath = mappingFile();
  if (!fs.existsSync(filePath)) {
    return emptyMapping();
  }
  let parsed;
  try {
    const raw = fs.readFileSync(filePath, 'utf8');
    const trimmed = raw.trim();
    if (!trimmed) {
      return emptyMapping();
    }
    parsed = JSON.parse(trimmed);
  } catch {
    throw new LinearMappingError('Linear mapping file is malformed', 'MALFORMED');
  }
  if (!isPlainObject(parsed)) {
    throw new LinearMappingError('Linear mapping file is malformed', 'MALFORMED');
  }
  const document = readMappingDocument(parsed);
  return document.workspaces[mappingOrgKey()] || emptyMapping();
}

/**
 * 保存当前 workspace 的映射：先读取整个文档以保留其它 workspace 分区
 * （文件损坏抛 MALFORMED），规范化输入后写入当前分区并原子落盘，返回写入的映射。
 * @param {object} input 含 defaultProjectPath 与 teamProjectPaths 的请求体
 * @throws {LinearMappingError} 请求体不是普通对象时抛 INVALID
 */
export function setStoredLinearMapping(input) {
  if (!isPlainObject(input)) {
    throw new LinearMappingError('Mapping body must be an object', 'INVALID');
  }
  const filePath = mappingFile();
  let document = { workspaces: {} };
  if (fs.existsSync(filePath)) {
    try {
      const raw = fs.readFileSync(filePath, 'utf8');
      const trimmed = raw.trim();
      if (trimmed) {
        const parsed = JSON.parse(trimmed);
        if (!isPlainObject(parsed)) {
          throw new LinearMappingError('Linear mapping file is malformed', 'MALFORMED');
        }
        document = readMappingDocument(parsed);
      }
    } catch (error) {
      if (error instanceof LinearMappingError) {
        throw error;
      }
      throw new LinearMappingError('Linear mapping file is malformed', 'MALFORMED');
    }
  }
  const next = {
    defaultProjectPath: readTrimmedString(input.defaultProjectPath) || null,
    teamProjectPaths: readTeamProjectPaths(input.teamProjectPaths),
  };
  document.workspaces[mappingOrgKey()] = next;
  writeJsonFile(filePath, document);
  return next;
}

/**
 * 合并存储映射与 Linear 团队列表为前端视图：每个团队附带其映射的
 * projectPath（未配置为 null），同时透出默认路径。
 * @param {object} stored readStoredLinearMapping 的结果
 * @param {object[]} teams listLinearTeams 返回的团队数组
 */
export function mergeLinearMappingView(stored, teams) {
  const mapping = stored || emptyMapping();
  const nodes = Array.isArray(teams) ? teams : [];
  return {
    defaultProjectPath: mapping.defaultProjectPath,
    teams: nodes.map((team) => ({
      id: team.id,
      key: team.key,
      name: team.name,
      projectPath: mapping.teamProjectPaths[team.id] || null,
    })),
  };
}

/**
 * 从映射视图解析某团队应使用的项目路径：先按团队 id 匹配，
 * 再按团队 key 匹配，均未命中时回退默认路径（可能为 null）。
 * @param {object} view mergeLinearMappingView 生成的视图
 * @param {{ id?: string, key?: string }} team Linear 团队对象
 * @returns {string | null} 项目路径
 */
export function resolveMappedProjectPath(view, team) {
  const teams = Array.isArray(view?.teams) ? view.teams : [];
  const teamId = team ? readTrimmedString(team.id) : '';
  if (teamId) {
    const row = teams.find((entry) => entry.id === teamId);
    if (row?.projectPath) {
      return row.projectPath;
    }
  }
  const teamKey = team ? readTrimmedString(team.key) : '';
  if (teamKey) {
    const row = teams.find((entry) => entry.key === teamKey);
    if (row?.projectPath) {
      return row.projectPath;
    }
  }
  return view?.defaultProjectPath || null;
}
