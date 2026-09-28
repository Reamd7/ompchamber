/**
 * 远程客户端认证运行时（remote client auth）。
 *
 * 管理"远程客户端"（手机 App、桌面端等）与 OMPChamber 服务器之间的
 * bearer token 生命周期：签发（磁盘上只保存 SHA-256 哈希，明文仅签发时
 * 返回一次）、认证（常数时间比较）、列举、吊销与清理。状态存放在一个
 * 0600 权限的 JSON 文件里；所有读改写都经同一个串行队列执行，避免并发
 * 认证流量覆盖掉并发执行的吊销。
 */
/** 令牌存储文件的结构版本号，保留用于将来做格式迁移。 */
const STORE_VERSION = 1;
/** 客户端 bearer token 的固定前缀；认证入口先做廉价前缀过滤。 */
const TOKEN_PREFIX = 'oc_client_';
/** 生成 token 使用的随机字节数（256 位熵）。 */
const TOKEN_BYTES = 32;
/** 客户端显示名称的最大长度，超长直接截断。 */
const MAX_LABEL_LENGTH = 80;
/** lastUsedAt 落盘的最小间隔：认证高频时限制写放大（传输方式变化时会立即写）。 */
const LAST_USED_WRITE_INTERVAL_MS = 60_000;

/** 归一化客户端显示名：非字符串或空白回退为 'Remote client'，超长按 MAX_LABEL_LENGTH 截断。 */
const normalizeLabel = (value) => {
  if (typeof value !== 'string') return 'Remote client';
  const trimmed = value.trim();
  if (!trimmed) return 'Remote client';
  return trimmed.length > MAX_LABEL_LENGTH ? trimmed.slice(0, MAX_LABEL_LENGTH) : trimmed;
};

/** 把任意输入规整为合法的 ISO 时间字符串；为空或无法解析时返回 null。 */
const normalizeTimestamp = (value) => {
  if (typeof value !== 'string') return null;
  const trimmed = value.trim();
  if (!trimmed) return null;
  const time = Date.parse(trimmed);
  return Number.isFinite(time) ? new Date(time).toISOString() : null;
};

/** 去除首尾空白后返回非空字符串，否则返回 null（用于可选元数据字段的统一收口）。 */
const normalizeOptionalString = (value) => {
  if (typeof value !== 'string') return null;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
};

/** 提取并归一化客户端元数据（认证方式、配对来源、设备信息、App 版本），缺失字段一律为 null。 */
const normalizeMetadata = (client) => ({
  authMethod: normalizeOptionalString(client.authMethod),
  pairingId: normalizeOptionalString(client.pairingId),
  deviceName: normalizeOptionalString(client.deviceName),
  devicePlatform: normalizeOptionalString(client.devicePlatform),
  deviceModel: normalizeOptionalString(client.deviceModel),
  appVersion: normalizeOptionalString(client.appVersion),
});

/** 解析 JSON 字符串；解析失败（存储文件损坏）时返回 null 而不是抛错。 */
const safeJsonParse = (raw) => {
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
};

/**
 * 常数时间比较两个十六进制字符串，防止时序侧信道逐字节泄露哈希。
 * 任一侧不是字符串、或 hex 解码后长度不同（timingSafeEqual 的前置要求）时直接返回 false。
 */
const constantTimeEqual = (left, right, crypto) => {
  if (typeof left !== 'string' || typeof right !== 'string') return false;
  const leftBuffer = Buffer.from(left, 'hex');
  const rightBuffer = Buffer.from(right, 'hex');
  if (leftBuffer.length !== rightBuffer.length) return false;
  return crypto.timingSafeEqual(leftBuffer, rightBuffer);
};

/**
 * 创建远程客户端认证运行时。
 *
 * @param {object} deps 依赖注入，便于测试替换
 * @param {object} deps.fsPromises 兼容 node:fs/promises 的文件系统接口
 * @param {object} deps.path 兼容 node:path 的路径工具
 * @param {object} deps.crypto 兼容 node:crypto 的实现（哈希、随机数、timingSafeEqual）
 * @param {string} deps.storePath 令牌存储 JSON 文件路径
 * @returns 认证运行时：authenticateBearerToken / createClient / listClients /
 *   hasActiveRelayClients / purgeRevokedClients / revokeClient
 */
export const createRemoteClientAuthRuntime = ({ fsPromises, path, crypto, storePath }) => {
  /** 计算 bearer token 的 SHA-256 十六进制哈希；明文 token 永不落盘。 */
  const hashToken = (token) => crypto.createHash('sha256').update(token).digest('hex');
  /** 当前时间的 ISO 字符串（所有时间戳字段的统一格式）。 */
  const nowIso = () => new Date().toISOString();
  /** 生成 24 位十六进制随机客户端 id。 */
  const generateId = () => crypto.randomBytes(12).toString('hex');
  /** 生成带 TOKEN_PREFIX 前缀的 base64url 随机 bearer token（仅创建时返回一次）。 */
  const generateToken = () => `${TOKEN_PREFIX}${crypto.randomBytes(TOKEN_BYTES).toString('base64url')}`;
  // 串行化所有存储写操作的 promise 链尾。
  let storeMutationQueue = Promise.resolve();

  /**
   * 把一次存储变更放入全局串行队列：等此前所有变更完成后执行 fn，
   * 无论成败都在 finally 里释放队列。保证 read-modify-write 不交错
   * （例如并发认证期间发起的吊销不会被覆盖回来）。
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

  /** 把磁盘上的任意 JSON 规整为当前版本的存储结构，逐字段校验并补默认值；没有有效 tokenHash 的记录被丢弃。 */
  const normalizeStore = (payload) => ({
    version: STORE_VERSION,
    clients: Array.isArray(payload?.clients)
      ? payload.clients
        .filter((client) => client && typeof client === 'object')
        .map((client) => ({
          id: typeof client.id === 'string' ? client.id : generateId(),
          label: normalizeLabel(client.label),
          tokenHash: typeof client.tokenHash === 'string' ? client.tokenHash : '',
          createdAt: typeof client.createdAt === 'string' ? client.createdAt : nowIso(),
          lastUsedAt: typeof client.lastUsedAt === 'string' ? client.lastUsedAt : null,
          revokedAt: typeof client.revokedAt === 'string' ? client.revokedAt : null,
          expiresAt: normalizeTimestamp(client.expiresAt),
          clientKind: normalizeOptionalString(client.clientKind),
          dedupeKey: normalizeOptionalString(client.dedupeKey),
          usesRelay: client.usesRelay === true,
          lastTransport: client.lastTransport === 'relay' || client.lastTransport === 'direct' ? client.lastTransport : null,
          ...normalizeMetadata(client),
        }))
        .filter((client) => client.tokenHash.length > 0)
      : [],
  });

  /** 读取并归一化存储文件；文件不存在（ENOENT）视作空存储，其它错误原样上抛。 */
  const readStore = async () => {
    try {
      const raw = await fsPromises.readFile(storePath, 'utf8');
      return normalizeStore(safeJsonParse(raw));
    } catch (error) {
      if (error?.code === 'ENOENT') return normalizeStore(null);
      throw error;
    }
  };

  /** 写回存储：先递归创建 0700 目录，再以 0600 权限写入；最后再补一次 chmod（容忍不支持它的实现，失败忽略）。 */
  const writeStore = async (store) => {
    await fsPromises.mkdir(path.dirname(storePath), { recursive: true, mode: 0o700 });
    await fsPromises.writeFile(storePath, JSON.stringify(normalizeStore(store), null, 2), { mode: 0o600 });
    if (typeof fsPromises.chmod === 'function') {
      await fsPromises.chmod(storePath, 0o600).catch(() => {});
    }
  };

  /** 投影出对外安全的客户端字段；刻意不含 tokenHash 等敏感信息。 */
  const publicClient = (client) => ({
    id: client.id,
    label: client.label,
    createdAt: client.createdAt,
    lastUsedAt: client.lastUsedAt,
    revokedAt: client.revokedAt,
    expiresAt: client.expiresAt,
    clientKind: client.clientKind,
    authMethod: client.authMethod,
    pairingId: client.pairingId,
    deviceName: client.deviceName,
    devicePlatform: client.devicePlatform,
    deviceModel: client.deviceModel,
    appVersion: client.appVersion,
    usesRelay: client.usesRelay === true,
    lastTransport: client.lastTransport ?? null,
  });

  /** 列出全部客户端的公开视图（含已吊销的）。 */
  const listClients = async () => {
    return withStoreMutation(async () => {
      const store = await readStore();
      return store.clients.map(publicClient);
    });
  };

  // Relay-transport demand from paired devices: any non-revoked, non-expired
  // client that was paired over the relay OR was actually observed connecting
  // through the relay tunnel (lastTransport). The observed transport is the
  // authoritative signal — it covers records written before usesRelay existed
  // and devices re-paired via a QR that carried no relay candidate.
  /** 是否存在活跃的 relay 客户端：未被吊销、未过期，且配对时走 relay 或最近实际经 relay 隧道连过。 */
  const hasActiveRelayClients = async () => {
    return withStoreMutation(async () => {
      const store = await readStore();
      const now = Date.now();
      return store.clients.some((client) => {
        if (client.usesRelay !== true && client.lastTransport !== 'relay') return false;
        if (client.revokedAt) return false;
        const expires = Date.parse(client.expiresAt || '');
        return !Number.isFinite(expires) || expires > now;
      });
    });
  };

  /**
   * 签发一个新客户端 token：生成随机 token，只落盘其哈希。
   * 带 dedupeKey 时替换同设备的旧记录（一台设备一条）；label 取值优先级为
   * 显式传入 > 被替换记录的原 label > fallbackLabel。
   * @returns {{ client: object, token: string }} 公开客户端记录与明文 token（仅此一次返回）
   */
  const createClient = async ({
    label,
    fallbackLabel,
    expiresAt,
    clientKind,
    dedupeKey,
    authMethod,
    pairingId,
    deviceName,
    devicePlatform,
    deviceModel,
    appVersion,
    usesRelay,
  } = {}) => {
    return withStoreMutation(async () => {
      const store = await readStore();
      const normalizedDedupeKey = normalizeOptionalString(dedupeKey);
      const token = generateToken();
      // A dedupe-keyed mint REPLACES the previous record for the same device,
      // so an operator-visible name must survive the replacement: an explicit
      // label wins, otherwise the replaced record's label is kept, and only a
      // first-ever mint falls back to the client-reported default.
      const existing = normalizedDedupeKey
        ? store.clients.find((entry) => entry.dedupeKey === normalizedDedupeKey)
        : null;
      const client = {
        id: generateId(),
        label: normalizeLabel(normalizeOptionalString(label) || existing?.label || fallbackLabel),
        tokenHash: hashToken(token),
        createdAt: nowIso(),
        lastUsedAt: null,
        revokedAt: null,
        expiresAt: normalizeTimestamp(expiresAt),
        clientKind: normalizeOptionalString(clientKind),
        dedupeKey: normalizedDedupeKey,
        authMethod: normalizeOptionalString(authMethod),
        pairingId: normalizeOptionalString(pairingId),
        deviceName: normalizeOptionalString(deviceName),
        devicePlatform: normalizeOptionalString(devicePlatform),
        deviceModel: normalizeOptionalString(deviceModel),
        appVersion: normalizeOptionalString(appVersion),
        usesRelay: usesRelay === true,
      };
      if (normalizedDedupeKey) {
        store.clients = store.clients.filter((entry) => entry.dedupeKey !== normalizedDedupeKey);
        // Migrate pre-clientKind desktop tokens: a deduped, kind-tagged mint
        // supersedes legacy records with the same label that carry neither a
        // kind nor a dedupe key — those tokens can no longer pass the
        // desktop-local client-create gate and would otherwise linger forever.
        if (client.clientKind) {
          store.clients = store.clients.filter((entry) =>
            !(entry.label === client.label && !entry.clientKind && !entry.dedupeKey));
        }
      }
      store.clients.push(client);
      await writeStore(store);
      return { client: publicClient(client), token };
    });
  };

  /** 按 id 吊销客户端（幂等，只补写 revokedAt 时间戳）；id 无效或记录不存在时返回 { revoked: false }。 */
  const revokeClient = async (id) => {
    if (typeof id !== 'string' || id.trim().length === 0) {
      return { revoked: false };
    }
    return withStoreMutation(async () => {
      const store = await readStore();
      const client = store.clients.find((entry) => entry.id === id);
      if (!client) return { revoked: false };
      if (!client.revokedAt) client.revokedAt = nowIso();
      await writeStore(store);
      return { revoked: true, client: publicClient(client) };
    });
  };

  /** 物理删除所有已吊销记录；返回删除条数（为 0 时不写盘）。 */
  const purgeRevokedClients = async () => {
    return withStoreMutation(async () => {
      const store = await readStore();
      const before = store.clients.length;
      store.clients = store.clients.filter((entry) => !entry.revokedAt);
      const purged = before - store.clients.length;
      if (purged > 0) {
        await writeStore(store);
      }
      return { purged };
    });
  };

  /**
   * 校验 bearer token 并返回认证上下文；前缀不符、哈希不匹配、已吊销或已过期时返回 null。
   * 请求头带 x-ompchamber-relay-connection 即判定为 relay 传输，并据此自愈 usesRelay
   * （粘性：之后的 direct 请求不会翻转回来）；lastUsedAt/lastTransport 按节流间隔落盘，
   * 传输方式变化时立即写。
   * @param {string} token 客户端 bearer token
   * @param {object} [req] 请求对象，用于识别传输方式（relay vs direct）
   * @returns {Promise<{ok:true,clientId:string,sessionToken:string,client:object}|null>}
   */
  const authenticateBearerToken = async (token, req) => {
    if (typeof token !== 'string' || !token.startsWith(TOKEN_PREFIX)) {
      return null;
    }
    // Which transport carried this request: the relay tunnel proxy stamps every
    // forwarded request with x-ompchamber-relay-connection; anything else is a
    // direct (local/LAN/tunnel-URL) request. Feeds device display AND relay
    // demand (hasActiveRelayClients), so a relay request must never be
    // misclassified as direct.
    const transport = req?.headers?.['x-ompchamber-relay-connection'] ? 'relay' : 'direct';
    return withStoreMutation(async () => {
      const tokenHash = hashToken(token);
      const store = await readStore();
      const client = store.clients.find((entry) => !entry.revokedAt && constantTimeEqual(entry.tokenHash, tokenHash, crypto));
      if (!client) return null;
      if (client.expiresAt && Date.parse(client.expiresAt) <= Date.now()) return null;
      const now = Date.now();
      const lastUsedAt = Date.parse(client.lastUsedAt || '');
      // Self-heal the paired-over-relay flag from the authoritative signal: a
      // request that arrived through the tunnel proves this device uses the
      // relay, regardless of what the pairing-time snapshot recorded. Sticky on
      // purpose — a later LAN request must not turn the relay host off again.
      const healUsesRelay = transport === 'relay' && client.usesRelay !== true;
      if (healUsesRelay) client.usesRelay = true;
      // Write on the throttle interval — or immediately when the transport
      // changed, so a LAN⇄relay switch is visible right away, not a minute late.
      if (healUsesRelay || !Number.isFinite(lastUsedAt) || now - lastUsedAt >= LAST_USED_WRITE_INTERVAL_MS || client.lastTransport !== transport) {
        client.lastUsedAt = new Date(now).toISOString();
        client.lastTransport = transport;
        await writeStore(store);
      }
      return { ok: true, clientId: client.id, sessionToken: client.id, client: publicClient(client) };
    });
  };

  return {
    authenticateBearerToken,
    createClient,
    listClients,
    hasActiveRelayClients,
    purgeRevokedClients,
    revokeClient,
  };
};
