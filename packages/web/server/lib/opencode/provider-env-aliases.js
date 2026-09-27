/**
 * Normalize provider credential env aliases for managed OpenCode.
 *
 * OpenCode may mark a provider as connected when any listed env key is present
 * (e.g. GEMINI_API_KEY), while the upstream AI SDK only reads a different name
 * (GOOGLE_GENERATIVE_AI_API_KEY). Mirror known aliases so chat works without
 * forcing the user to paste the same key again in Settings.
 */
/**
 * （中文说明）为托管 OpenCode 镜像 provider 凭证环境变量的别名。
 *
 * OpenCode 只要看到别名列（如 GEMINI_API_KEY）中任意一个键就认为 provider
 * 已连接，而上游 AI SDK 实际读取的是另一个名字。把已知别名互相镜像后，
 * 用户无需在设置里重复粘贴同一把 key 即可让会话正常工作。
 */

/** Google API key 的等价环境变量名集合（第一个为上游 SDK 实际读取的规范名）。 */
const GOOGLE_API_KEY_ALIASES = [
  'GOOGLE_GENERATIVE_AI_API_KEY',
  'GOOGLE_API_KEY',
  'GEMINI_API_KEY',
];

/**
 * 返回镜像了 Google API key 别名的新环境对象（不修改入参）。
 * 取别名列中第一个非空字符串值，回填到列内所有仍为空或未设置的键上；
 * 已有值（例如规范名已显式设置）不会被覆盖。env 非对象时返回空对象。
 * @param {object} env - 进程环境（或其子集）。
 * @returns {object} 镜像后的新环境对象。
 */
export function applyProviderEnvAliases(env) {
  if (!env || typeof env !== 'object') {
    return {};
  }

  const next = { ...env };
  const googleValue = GOOGLE_API_KEY_ALIASES
    .map((key) => next[key])
    .find((value) => typeof value === 'string' && value.trim().length > 0);

  if (googleValue) {
    for (const key of GOOGLE_API_KEY_ALIASES) {
      if (typeof next[key] !== 'string' || next[key].trim().length === 0) {
        next[key] = googleValue;
      }
    }
  }

  return next;
}
