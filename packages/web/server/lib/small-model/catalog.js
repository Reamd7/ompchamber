/**
 * models.dev 模型目录的读取入口（small-model 模块的目录数据源）。
 * 目录数据与 /api/ompchamber/models-metadata 路由共享同一个进程内缓存，
 * 因此这里不产生额外的网络请求，也不落缓存文件。
 */
import { getModelsMetadata } from '../opencode/models-metadata.js';

// The models.dev catalog is shared with the /api/ompchamber/models-metadata
// route through one in-process cache — no extra fetches, no cache files.
/** 读取完整的 models.dev 目录；底层 getModelsMetadata 失败时向上抛错，由调用方降级为空目录。 */
export async function getModelCatalog() {
  const { metadata } = await getModelsMetadata();
  return metadata;
}

/**
 * 安全取出目录中某个 provider 的条目。
 * @param {Object|null} catalog getModelCatalog 的返回值。
 * @param {string} providerID provider 标识（如 'anthropic'）。
 * @returns {Object|null} provider 条目对象；目录缺失或条目不是对象时返回 null，不抛错。
 */
export function getCatalogProvider(catalog, providerID) {
  const entry = catalog?.[providerID];
  return entry && typeof entry === 'object' ? entry : null;
}
