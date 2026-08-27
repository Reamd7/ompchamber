export const createOpenCodeResolutionRuntime = (dependencies) => {
  const {
    path,
    resolveOpencodeCliPath,
    ensureOpencodeCliEnv,
    resolveManagedOpenCodeLaunchSpec,
    getResolvedState,
    setResolvedOpencodeBinarySource,
  } = dependencies;

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

  return {
    getOpenCodeResolutionSnapshot,
  };
};
