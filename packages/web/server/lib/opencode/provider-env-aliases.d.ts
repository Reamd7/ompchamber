/**
 * provider-env-aliases 的类型声明（实现见 ./provider-env-aliases.js）：
 * 镜像 provider 凭证环境变量别名，返回新的环境对象。
 */
/**
 * 镜像 Google API key 等别名到全部已知环境变量名；env 非对象时返回空对象。
 * @param env - 进程环境（或其子集），不会被修改。
 */
export function applyProviderEnvAliases(
  env: Record<string, string | undefined> | null | undefined,
): Record<string, string | undefined>;
