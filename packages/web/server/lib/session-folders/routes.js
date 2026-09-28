/**
 * 会话文件夹（UI 侧目录树分组）快照的存取路由：GET / POST
 * /api/session-folders。快照持久化为数据目录下的单文件
 * sessions-directories.json，写入走“唯一临时文件 + rename”的原子替换，
 * 并按 updatedAt 单调递增（旧快照晚到被忽略）；结构校验失败宁可向
 * 浏览器报错，也不把磁盘上的畸形状态当作权威空快照去覆盖有效状态。
 */
/** 快照序列化后的大小上限（4 MiB），超出返回 413。 */
const MAX_BODY_BYTES = 4 * 1024 * 1024;

/** 值为非数组的纯对象时为 true。 */
const isObjectRecord = (value) => Boolean(value) && typeof value === 'object' && !Array.isArray(value);

/** 校验单个文件夹条目：id / name 为字符串、sessionIds 为字符串数组、createdAt 为有限数字、parentId 可选（缺省 / null / 字符串）。 */
const hasValidFolderShape = (folder) => (
  isObjectRecord(folder)
  && typeof folder.id === 'string'
  && typeof folder.name === 'string'
  && Array.isArray(folder.sessionIds)
  && folder.sessionIds.every((sessionId) => typeof sessionId === 'string')
  && typeof folder.createdAt === 'number'
  && Number.isFinite(folder.createdAt)
  && (folder.parentId === undefined || folder.parentId === null || typeof folder.parentId === 'string')
);

/** 校验 foldersMap：对象本身合法且每个键的值都是合法文件夹条目数组。 */
const hasValidFoldersMapShape = (foldersMap) => (
  isObjectRecord(foldersMap)
  && Object.values(foldersMap).every((folders) => (
    Array.isArray(folders) && folders.every(hasValidFolderShape)
  ))
);

/** 校验整份快照：version 必须为 1、foldersMap 合法、collapsedFolderIds 为字符串数组。 */
const hasValidFolderSnapshotShape = (snapshot) => (
  isObjectRecord(snapshot)
  && snapshot.version === 1
  && hasValidFoldersMapShape(snapshot.foldersMap)
  && Array.isArray(snapshot.collapsedFolderIds)
  && snapshot.collapsedFolderIds.every((folderId) => typeof folderId === 'string')
);

/**
 * 在 express app 上注册会话文件夹快照路由。
 *
 * @param {object} app express 应用
 * @param {object} dependencies 注入依赖：fsPromises（可替换的 fs/promises）、
 *   path、ompchamberDataDir（快照文件所在数据目录）
 */
export const registerSessionFoldersRoutes = (app, dependencies) => {
  const {
    fsPromises,
    path,
    ompchamberDataDir,
  } = dependencies;

  // 快照文件绝对路径：<数据目录>/sessions-directories.json。
  const filePath = path.join(ompchamberDataDir, 'sessions-directories.json');
  // 串行化保存的 promise 链：并发 POST 逐个落盘，避免临时文件与 rename 交错。
  let saveQueue = Promise.resolve();

  /** 确保快照文件所在目录存在（递归 mkdir）。 */
  const ensureDir = async () => {
    await fsPromises.mkdir(path.dirname(filePath), { recursive: true });
  };

  // 读取快照：文件缺失返回 { version: 1, exists: false }（不把缺失当作权威
  // 空快照）；磁盘内容 JSON 畸形或结构非法返回 500，让浏览器端保留自己的
  // 有效状态；其余读失败同样 500。
  app.get('/api/session-folders', async (_req, res) => {
    try {
      const raw = await fsPromises.readFile(filePath, 'utf8').catch((error) => {
        if (error && error.code === 'ENOENT') return null;
        throw error;
      });
      if (!raw) {
        return res.json({ version: 1, exists: false });
      }
      try {
        const parsed = JSON.parse(raw);
        if (
          !hasValidFolderSnapshotShape(parsed)
          || typeof parsed.updatedAt !== 'number'
          || !Number.isFinite(parsed.updatedAt)
          || parsed.updatedAt <= 0
        ) {
          return res.status(500).json({ error: 'Stored session folders have an invalid shape' });
        }
        return res.json({ ...parsed, exists: true });
      } catch {
        return res.status(500).json({ error: 'Stored session folders are malformed' });
      }
    } catch (error) {
      const message = error instanceof Error ? error.message : 'Failed to read session folders';
      return res.status(500).json({ error: message });
    }
  });

  // 保存快照：body 结构校验（400）、序列化大小上限（413）、updatedAt 必须
  // 为正有限数（400），全部通过后入队串行保存。
  app.post('/api/session-folders', async (req, res) => {
    const body = req.body;
    if (!isObjectRecord(body)) {
      return res.status(400).json({ error: 'Body must be an object' });
    }
    if (!hasValidFolderSnapshotShape(body)) {
      return res.status(400).json({ error: 'Invalid session folders payload' });
    }
    const serialized = JSON.stringify(body, null, 2);
    if (Buffer.byteLength(serialized, 'utf8') > MAX_BODY_BYTES) {
      return res.status(413).json({ error: 'Payload too large' });
    }
    if (typeof body.updatedAt !== 'number' || !Number.isFinite(body.updatedAt) || body.updatedAt <= 0) {
      return res.status(400).json({ error: 'updatedAt must be a positive finite number' });
    }

      /**
       * 实际落盘：先读现有文件，磁盘快照更新（updatedAt 更大或相等）则忽略
       * 本次写入并返回 ignored:true；磁盘现状畸形时允许新快照修复覆盖。
       * 写入走“唯一临时文件 + rename”原子替换；任何失败清理未落地的临时
       * 文件并返回 500。
       */
    const save = async () => {
      let tmp;
      let saved = false;
      try {
        const currentRaw = await fsPromises.readFile(filePath, 'utf8').catch((error) => {
          if (error && error.code === 'ENOENT') return null;
          throw error;
        });
        if (currentRaw) {
          try {
            const current = JSON.parse(currentRaw);
            const currentUpdatedAt = hasValidFolderSnapshotShape(current)
              && typeof current.updatedAt === 'number'
              && Number.isFinite(current.updatedAt)
              ? current.updatedAt
              : 0;
            if (currentUpdatedAt >= body.updatedAt) {
              return res.json({ success: true, ignored: true });
            }
          } catch { /* A valid new snapshot repairs malformed prior state. */ }
        }

        await ensureDir();
        tmp = `${filePath}.tmp-${process.pid}-${Date.now()}-${Math.random().toString(16).slice(2)}`;
        await fsPromises.writeFile(tmp, serialized, 'utf8');
        await fsPromises.rename(tmp, filePath);
        saved = true;
        return res.json({ success: true });
      } catch (error) {
        if (tmp && !saved) {
          await fsPromises.unlink(tmp).catch(() => {});
        }
        const message = error instanceof Error ? error.message : 'Failed to write session folders';
        return res.status(500).json({ error: message });
      }
    };

    // 接入串行队列执行本次保存，并吞掉结果让队列得以继续。
    const pendingSave = saveQueue.then(save, save);
    saveQueue = pendingSave.then(() => undefined, () => undefined);
    return pendingSave;
  });
};
