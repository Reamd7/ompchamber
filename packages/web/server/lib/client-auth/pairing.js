/**
 * 远程客户端配对（pairing）运行时。
 *
 * 流程：操作者在服务端创建一个短期配对会话（id + 一次性 secret，存储
 * 里只保存 secret 的哈希），把两者交给待接入的设备；设备调用
 * redeemPairingSession 用它们换取正式的客户端凭据（经
 * remoteClientAuthRuntime.createClient 签发）。
 *
 * 会话持久化为 storePath 处的 JSON 文件（0600 权限），全部写操作经
 * withStoreMutation 串行化以避免读改写竞争；过期、已用、已取消的会话
 * 在每次写入时顺带清扫。redeem 的所有失败路径共用同一个模糊错误文
 * 案，不给探测者区分"不存在/已用过/已过期"的机会。
 */

/** 配对存储文件的 schema 版本。 */
const STORE_VERSION = 1;
/** 配对会话 id 的前缀（pair_）。 */
const PAIRING_ID_PREFIX = 'pair_';
/** 一次性 secret 的熵（字节数），以 base64url 交给设备。 */
const SECRET_BYTES = 32;
/** 展示指纹的随机字节数；格式化为 8 位大写十六进制（XXXX-XXXX）。 */
const FINGERPRINT_BYTES = 4;
/** 配对会话的默认有效期：10 分钟。 */
const DEFAULT_TTL_MS = 10 * 60 * 1000;
/** 操作者可输入的设备标签的最大长度（字符数）。 */
const MAX_LABEL_LENGTH = 80;
/** 允许的客户端种类：mobile 与 desktop。 */
const VALID_CLIENT_KINDS = new Set(['mobile', 'desktop']);
/** redeem 失败的统一文案——不泄露会话处于哪种不可兑换状态。 */
const GENERIC_REDEEM_ERROR = 'Invalid or expired pairing session';

/** 规整可选字符串：trim 后非空返回原串，否则返回 null。 */
const normalizeOptionalString = (value) => {
  if (typeof value !== 'string') return null;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
};

// Placeholder shown in the pending-devices list when the operator did not type a
// name. It is a DISPLAY default only — the stored label stays null so redeem can
// fall back to the device's own reported name instead of this placeholder.
// 中文补充：仅用于待配对列表的展示占位；存储中 label 保持 null，让
// redeem 回退到设备自报的名称而不是这个占位符。
const PAIRING_LABEL_PLACEHOLDER = 'Pair new device';

// The operator's typed device label, capped. Returns null when unset so callers
// can distinguish "no name given" from a real name.
// 中文补充：操作者输入的设备名，超长截断到 MAX_LABEL_LENGTH；未输入
// 返回 null，让调用方区分"没起名"与真实名称。
const normalizeStoredLabel = (value) => {
  const normalized = normalizeOptionalString(value);
  if (!normalized) return null;
  return normalized.length > MAX_LABEL_LENGTH ? normalized.slice(0, MAX_LABEL_LENGTH) : normalized;
};

/** 规整时间戳为 ISO 字符串；无法解析返回 null。 */
const normalizeTimestamp = (value) => {
  const normalized = normalizeOptionalString(value);
  if (!normalized) return null;
  const time = Date.parse(normalized);
  return Number.isFinite(time) ? new Date(time).toISOString() : null;
};

/** 规整客户端种类：不在 VALID_CLIENT_KINDS 内返回 null。 */
const normalizeClientKind = (value) => {
  const normalized = normalizeOptionalString(value);
  return normalized && VALID_CLIENT_KINDS.has(normalized) ? normalized : null;
};

/** 规整允许的客户端种类列表：过滤非法值并去重；结果为空时回退为两种皆可。 */
const normalizeAllowedClientKinds = (value) => {
  if (!Array.isArray(value)) return ['mobile', 'desktop'];
  const kinds = value.map(normalizeClientKind).filter(Boolean);
  return kinds.length > 0 ? Array.from(new Set(kinds)) : ['mobile', 'desktop'];
};

/** 安全的 JSON.parse：解析失败返回 null 而不抛错（用于读取存储文件）。 */
const safeJsonParse = (raw) => {
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
};

/**
 * 借助 crypto.timingSafeEqual 常数时间比较两个 hex 字符串，用于
 * secret 哈希比对以避免时序侧信道；非字符串或长度不等直接 false。
 */
const constantTimeEqual = (left, right, crypto) => {
  if (typeof left !== 'string' || typeof right !== 'string') return false;
  const leftBuffer = Buffer.from(left, 'hex');
  const rightBuffer = Buffer.from(right, 'hex');
  if (leftBuffer.length !== rightBuffer.length) return false;
  return crypto.timingSafeEqual(leftBuffer, rightBuffer);
};

/**
 * 把存储中的会话投影为对外形状：剥离 secretHash，只保留路由与面板需要
 * 的字段；label 为空时回退到展示占位符。
 */
const publicSession = (session) => ({
  id: session.id,
  createdAt: session.createdAt,
  expiresAt: session.expiresAt,
  usedAt: session.usedAt,
  cancelledAt: session.cancelledAt,
  clientId: session.clientId,
  label: session.label || PAIRING_LABEL_PLACEHOLDER,
  fingerprint: session.fingerprint,
  allowedClientKinds: session.allowedClientKinds,
  createdByClientId: session.createdByClientId,
  usesRelay: session.usesRelay === true,
});

// A pending session is one that can still be redeemed: not used, not cancelled,
// not expired.
// 中文补充：待定（pending）= 仍可被兑换：未使用、未取消、且未过期。
const isPendingSession = (session) => !session.usedAt
  && !session.cancelledAt
  && Number.isFinite(Date.parse(session.expiresAt))
  && Date.parse(session.expiresAt) > Date.now();

/** 构造 redeem 的统一拒绝错误（400 + 模糊文案）。 */
const redeemError = () => {
  const error = new Error(GENERIC_REDEEM_ERROR);
  error.statusCode = 400;
  return error;
};

/**
 * 创建配对运行时。依赖：fsPromises、path、crypto、storePath（存储文件
 * 路径）、remoteClientAuthRuntime（正式客户端凭据的签发方）、ttlMs
 * （有效期，默认 10 分钟）；缺任一必填项直接抛错。返回七个配对操作。
 */
export const createClientPairingRuntime = ({
  fsPromises,
  path,
  crypto,
  storePath,
  remoteClientAuthRuntime,
  ttlMs = DEFAULT_TTL_MS,
} = {}) => {
  if (!fsPromises || !path || !crypto || !storePath || !remoteClientAuthRuntime) {
    throw new Error('createClientPairingRuntime requires fsPromises, path, crypto, storePath, and remoteClientAuthRuntime');
  }

  /** 当前时间的 ISO 字符串。 */
  const nowIso = () => new Date().toISOString();
  /** 计算 secret 的 SHA-256 hex 哈希；存储里只保存这个哈希。 */
  const hashSecret = (secret) => crypto.createHash('sha256').update(secret).digest('hex');
  /** 生成配对会话 id：pair_ 前缀 + 12 字节随机 hex。 */
  const generateId = () => `${PAIRING_ID_PREFIX}${crypto.randomBytes(12).toString('hex')}`;
  /** 生成一次性 secret：32 字节随机数的 base64url。 */
  const generateSecret = () => crypto.randomBytes(SECRET_BYTES).toString('base64url');
  /** 生成展示指纹：4 字节随机 hex 大写，格式 XXXX-XXXX。 */
  const generateFingerprint = () => crypto.randomBytes(FINGERPRINT_BYTES).toString('hex').toUpperCase().replace(/^(.{4})(.{4})$/, '$1-$2');
  // 存储互斥队列的队尾 Promise：所有写操作依次排队，避免读改写竞争。
  let storeMutationQueue = Promise.resolve();

  /**
   * 串行执行一次存储变更：先取走当前队尾，用新 Promise 占位延长队列，
   * 等待前序完成后再运行 fn，结束时释放占位。保证任意时刻至多一个变
   * 更在读改写存储文件。
   */
  const withStoreMutation = async (fn) => {
    const previous = storeMutationQueue;
    let release;
    storeMutationQueue = new Promise((resolve) => {
      release = resolve;
    });
    await previous;
    try {
      return await fn();
    } finally {
      release();
    }
  };

  /**
   * 把从磁盘读到的任意 payload 规整为合法的存储形状：逐字段做类型检查
   * 与回填（缺失的 id、时间、指纹现场生成），丢弃没有 secretHash 的会
   * 话；sessions 非数组时归为空表。坏数据不会让整个存储不可用。
   */
  const normalizeStore = (payload) => ({
    version: STORE_VERSION,
    sessions: Array.isArray(payload?.sessions)
      ? payload.sessions
        .filter((session) => session && typeof session === 'object')
        .map((session) => ({
          id: typeof session.id === 'string' ? session.id : generateId(),
          secretHash: typeof session.secretHash === 'string' ? session.secretHash : '',
          createdAt: typeof session.createdAt === 'string' ? session.createdAt : nowIso(),
          expiresAt: normalizeTimestamp(session.expiresAt) || new Date(Date.now() + ttlMs).toISOString(),
          usedAt: normalizeTimestamp(session.usedAt),
          cancelledAt: normalizeTimestamp(session.cancelledAt),
          clientId: normalizeOptionalString(session.clientId),
          label: normalizeStoredLabel(session.label),
          fingerprint: normalizeOptionalString(session.fingerprint) || generateFingerprint(),
          allowedClientKinds: normalizeAllowedClientKinds(session.allowedClientKinds),
          createdByClientId: normalizeOptionalString(session.createdByClientId),
          usesRelay: session.usesRelay === true,
        }))
        .filter((session) => session.secretHash.length > 0)
      : [],
  });

  /** 读取并规整存储文件；文件不存在（ENOENT）视为空存储，其它错误上抛。 */
  const readStore = async () => {
    try {
      const raw = await fsPromises.readFile(storePath, 'utf8');
      return normalizeStore(safeJsonParse(raw));
    } catch (error) {
      if (error?.code === 'ENOENT') return normalizeStore(null);
      throw error;
    }
  };

  /**
   * 写入存储：先确保目录存在（0700）再写文件（0600），并尽力补一次
   * chmod 压住既有文件的宽松权限；chmod 失败不阻塞写入。
   */
  const writeStore = async (store) => {
    await fsPromises.mkdir(path.dirname(storePath), { recursive: true, mode: 0o700 });
    await fsPromises.writeFile(storePath, JSON.stringify(normalizeStore(store), null, 2), { mode: 0o600 });
    if (typeof fsPromises.chmod === 'function') {
      await fsPromises.chmod(storePath, 0o600).catch(() => {});
    }
  };

  /**
   * 就地清扫过期会话：已使用或已取消的保留一段 ttlMs 后删除；从未激活
   * 的在自身过期后删除——已不可兑换，留着只会永远躺在存储里。
   */
  const sweepExpiredSessionsFromStore = (store) => {
    const now = Date.now();
    const cutoff = now - ttlMs;
    store.sessions = store.sessions.filter((session) => {
      const usedAt = Date.parse(session.usedAt || '');
      const cancelledAt = Date.parse(session.cancelledAt || '');
      const inactiveAt = Number.isFinite(usedAt) ? usedAt : cancelledAt;
      if (Number.isFinite(inactiveAt)) return inactiveAt >= cutoff;
      // Never used or cancelled: drop once the session itself has expired —
      // it can no longer be redeemed and would otherwise sit in the store forever.
      const expiresAt = Date.parse(session.expiresAt || '');
      return !Number.isFinite(expiresAt) || expiresAt > now;
    });
  };

  /**
   * 创建配对会话：生成 id、一次性 secret（只存哈希）与指纹，先清扫过期
   * 会话再登记入库。返回 { pairing }——secret 明文只在这一次返回里出现。
   */
  const createPairingSession = async ({ label, allowedClientKinds, createdByClientId, usesRelay } = {}) => {
    return withStoreMutation(async () => {
      const store = await readStore();
      sweepExpiredSessionsFromStore(store);
      const secret = generateSecret();
      const session = {
        id: generateId(),
        secretHash: hashSecret(secret),
        createdAt: nowIso(),
        expiresAt: new Date(Date.now() + ttlMs).toISOString(),
        usedAt: null,
        cancelledAt: null,
        clientId: null,
        label: normalizeStoredLabel(label),
        fingerprint: generateFingerprint(),
        allowedClientKinds: normalizeAllowedClientKinds(allowedClientKinds),
        createdByClientId: normalizeOptionalString(createdByClientId),
        usesRelay: usesRelay === true,
      };
      store.sessions.push(session);
      await writeStore(store);
      return { pairing: { ...publicSession(session), secret } };
    });
  };

  // Sessions that can still be redeemed (link created, device not yet connected).
  // 中文补充：仍可被兑换的会话（链接已创建、设备尚未接入）。
  const listPendingSessions = async () => withStoreMutation(async () => {
    const store = await readStore();
    return store.sessions.filter(isPendingSession).map(publicSession);
  });

  // Relay-transport demand from pairing: any still-redeemable relay session.
  // 中文补充：relay 传输的配对需求——存在任一仍可兑换的 relay 会话。
  const hasActiveRelaySession = async () => withStoreMutation(async () => {
    const store = await readStore();
    return store.sessions.some((session) => session.usesRelay === true && isPendingSession(session));
  });

  /** 按 id 查询单个会话的对外形状；id 无效或未找到返回 null。 */
  const getPairingSession = async (id) => {
    const normalizedId = normalizeOptionalString(id);
    if (!normalizedId) return null;
    return withStoreMutation(async () => {
      const store = await readStore();
      const session = store.sessions.find((entry) => entry.id === normalizedId);
      return session ? publicSession(session) : null;
    });
  };

  /**
   * 取消配对会话：幂等（已取消的再取消仍返回 cancelled: true）；id 无
   * 效或未找到返回 { cancelled: false }，不改动存储。
   */
  const cancelPairingSession = async (id) => {
    const normalizedId = normalizeOptionalString(id);
    if (!normalizedId) return { cancelled: false };
    return withStoreMutation(async () => {
      const store = await readStore();
      const session = store.sessions.find((entry) => entry.id === normalizedId);
      if (!session) return { cancelled: false };
      if (!session.cancelledAt) session.cancelledAt = nowIso();
      await writeStore(store);
      return { cancelled: true, pairing: publicSession(session) };
    });
  };

  /**
   * 兑换配对会话：校验 id 存在、未取消、未使用、未过期、客户端种类被
   * 允许、secret 哈希常数时间匹配——任一失败都抛统一的模糊错误。通过
   * 后经 remoteClientAuthRuntime.createClient 签发正式客户端凭据（操作
   * 者起的 label 优先，设备自报名仅作回退；dedupeKey 缺省退回
   * pairing:会话id），标记 usedAt 并返回 { pairing, client, token }。
   */
  const redeemPairingSession = async ({
    pairingId,
    secret,
    clientLabel,
    clientKind,
    deviceName,
    devicePlatform,
    deviceModel,
    appVersion,
    dedupeKey,
  } = {}) => {
    const normalizedId = normalizeOptionalString(pairingId);
    const normalizedSecret = normalizeOptionalString(secret);
    const normalizedKind = normalizeClientKind(clientKind) || 'mobile';
    if (!normalizedId || !normalizedSecret) throw redeemError();

    return withStoreMutation(async () => {
      const store = await readStore();
      const session = store.sessions.find((entry) => entry.id === normalizedId);
      if (!session) throw redeemError();
      if (session.cancelledAt || session.usedAt) throw redeemError();
      if (Date.parse(session.expiresAt) <= Date.now()) throw redeemError();
      if (!session.allowedClientKinds.includes(normalizedKind)) throw redeemError();
      if (!constantTimeEqual(session.secretHash, hashSecret(normalizedSecret), crypto)) throw redeemError();

      // The operator's typed pairing label is THIS server's name for the device
      // (shown in the device list) and wins outright. The device's self-reported
      // label is only a fallback: on a re-pair with the same dedupeKey,
      // createClient keeps the replaced record's label over it, so a rescan
      // without a typed name does not reset the device to the app default.
      const result = await remoteClientAuthRuntime.createClient({
        label: normalizeOptionalString(session.label),
        fallbackLabel: normalizeOptionalString(clientLabel)
          || normalizeOptionalString(deviceName)
          || 'Remote client',
        clientKind: normalizedKind,
        dedupeKey: normalizeOptionalString(dedupeKey) || `pairing:${session.id}`,
        authMethod: 'pairing',
        pairingId: session.id,
        deviceName,
        devicePlatform,
        deviceModel,
        appVersion,
        usesRelay: session.usesRelay === true,
      });
      session.usedAt = nowIso();
      session.clientId = result.client?.id || null;
      await writeStore(store);
      return { pairing: publicSession(session), client: result.client, token: result.token };
    });
  };

  /** 手动清扫入口：删除过期会话并返回 { purged }；无变化时不写盘。 */
  const sweepExpiredSessions = async () => withStoreMutation(async () => {
    const store = await readStore();
    const before = store.sessions.length;
    sweepExpiredSessionsFromStore(store);
    const purged = before - store.sessions.length;
    if (purged > 0) await writeStore(store);
    return { purged };
  });

  return {
    createPairingSession,
    getPairingSession,
    listPendingSessions,
    hasActiveRelaySession,
    cancelPairingSession,
    redeemPairingSession,
    sweepExpiredSessions,
  };
};
