/**
 * OpenCode 二进制解析运行时：产出“当前如何解析与启动 OpenCode”的快照。
 *
 * 快照同时携带持久化状态中的解析结果与“此刻重新探测”的结果，供设置页
 * 展示二进制来源（env/path 等）是否变化，以及实际的启动规格
 * （launchBinary/launchArgs/wrapperType）与 node/bun 运行时路径。
 */
/**
 * 创建解析快照运行时。
 * @param {object} dependencies - path 工具、resolveOpencodeCliPath（探测）、
 *   ensureOpencodeCliEnv（环境准备）、resolveManagedOpenCodeLaunchSpec
 *   （启动规格）、getResolvedState / setResolvedOpencodeBinarySource
 *   （解析状态存取）。
 */
export const createOpenCodeResolutionRuntime = (dependencies) => {
  const {
    path,
    resolveOpencodeCliPath,
    ensureOpencodeCliEnv,
    resolveManagedOpenCodeLaunchSpec,
    getResolvedState,
    setResolvedOpencodeBinarySource,
  } = dependencies;

  /**
   * 生成 OpenCode 解析快照。
   *
   * 探测（resolveOpencodeCliPath）会写入 resolved state，因此先保存旧来源、
   * 探测后立即恢复，避免探测副作用污染持久状态。detectedSourceNow 的修正
   * 规则：当探测路径与已解析路径一致、探测来源是 env 而持久来源不是 env
   * 时，沿用持久来源（env 指向的正是已锁定的那个二进制）。
   * @returns 解析路径与来源（resolved/source/detectedNow/detectedSourceNow）、
   *   启动规格（launchBinary/launchArgs/launchWrapperType）及 node/bun 路径；
   *   未解析出二进制时相应字段为 null 或空数组。
   */
  const getOpenCodeResolutionSnapshot = async () => {
    const { resolvedOpencodeBinarySource: previousSource } = getResolvedState();
    const detectedNow = resolveOpencodeCliPath();
    const { resolvedOpencodeBinarySource: rawDetectedSourceNow } = getResolvedState();
    setResolvedOpencodeBinarySource(previousSource);

    ensureOpencodeCliEnv();

    const {
      resolvedOpencodeBinary,
      resolvedOpencodeBinarySource,
      resolvedNodeBinary,
      resolvedBunBinary,
    } = getResolvedState();

    const resolved = resolvedOpencodeBinary || null;
    const source = resolvedOpencodeBinarySource || null;
    const detectedSourceNow =
      detectedNow &&
      resolved &&
      detectedNow === resolved &&
      rawDetectedSourceNow === 'env' &&
      source &&
      source !== 'env'
        ? source
        : rawDetectedSourceNow;
    const launchSpec = resolved
      ? resolveManagedOpenCodeLaunchSpec(resolved)
      : null;

    return {
      resolved,
      resolvedDir: resolved ? path.dirname(resolved) : null,
      source,
      detectedNow,
      detectedSourceNow,
      launchBinary: launchSpec?.binary || null,
      launchArgs: launchSpec?.args || [],
      launchWrapperType: launchSpec?.wrapperType || null,
      node: resolvedNodeBinary || null,
      bun: resolvedBunBinary || null,
    };
  };

  // 导出：唯一的快照入口
  return {
    getOpenCodeResolutionSnapshot,
  };
};
