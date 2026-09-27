/**
 * 项目 ID 生成工具。
 *
 * 将本地项目路径转换为稳定且可安全用于文件名与 URL 的项目 ID
 * （`path_` 前缀 + base64url），供 projects 配置目录与
 * `/api/project-context` 等路由按项目寻址。
 */

/** 统一路径分隔符：反斜杠转正斜杠并去掉末尾斜杠；非字符串输入返回空串。 */
const normalizeProjectPathForId = (value) => {
  if (typeof value !== 'string') return '';
  return value.replace(/\\/g, '/').replace(/\/+$/g, '') || value;
};

/**
 * 由项目路径生成稳定项目 ID：规范化并 trim 后做 base64url 编码，
 * 加 `path_` 前缀以区分其它来源的 ID。空路径返回空字符串，
 * 由调用方据此判定输入无效。
 */
export const createProjectIdFromPath = (projectPath) => {
  const normalized = normalizeProjectPathForId(projectPath).trim();
  if (!normalized) {
    return '';
  }

  return `path_${Buffer.from(normalized, 'utf8').toString('base64url')}`;
};
