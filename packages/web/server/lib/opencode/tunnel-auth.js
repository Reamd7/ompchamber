/**
 * OpenCode 隧道（公网 URL）访问的认证与限流模块。
 *
 * createTunnelAuth 创建一个有状态的认证器：隧道建立后可签发一次性
 * bootstrap token，远端浏览器用它换取 HttpOnly 会话 cookie，此后所有经
 * 隧道进来的请求都必须持有有效会话。同时提供：按客户端 IP 的 connect
 * 限流（防 token 爆破，拿不到 IP 的请求用更严的聚合阈值）、请求来源分类
 * （tunnel / local / unknown-public，本地回环不受隧道锁影响）、以及隧道
 * 停止时对会话与 token 的一并吊销。全部状态在内存中，随服务重启清空。
 */
import crypto from 'crypto';

/** bootstrap token 的随机字节数：32 字节，base64url 编码后可安全放进 cookie。 */
const BOOTSTRAP_TOKEN_COOKIE_SAFE_BYTES = 32;
/** 隧道会话 cookie 的名称。 */
const TUNNEL_SESSION_COOKIE_NAME = 'oc_tunnel_session';

/** connect 尝试的计数窗口：5 分钟。 */
const CONNECT_RATE_LIMIT_WINDOW_MS = 5 * 60 * 1000;
/** 触发限流后的锁定时长：10 分钟。 */
const CONNECT_RATE_LIMIT_LOCK_MS = 10 * 60 * 1000;
/** 能取到 IP 的客户端在窗口内允许的最大失败次数。 */
const CONNECT_RATE_LIMIT_MAX_ATTEMPTS = 20;
/** 取不到 IP 的客户端允许的最大失败次数（更严格，共用一个聚合键）。 */
const CONNECT_RATE_LIMIT_NO_IP_MAX_ATTEMPTS = 5;

/** 解析 Cookie 请求头为对象：按 “;” 拆分、值做 decodeURIComponent；非法输入返回空对象。 */
const parseCookies = (cookieHeader) => {
  if (!cookieHeader || typeof cookieHeader !== 'string') {
    return {};
  }

  return cookieHeader.split(';').reduce((acc, segment) => {
    const [name, ...rest] = segment.split('=');
    if (!name) {
      return acc;
    }
    const key = name.trim();
    if (!key) {
      return acc;
    }
    const value = rest.join('=').trim();
    acc[key] = decodeURIComponent(value || '');
    return acc;
  }, {});
};

/** 判断请求是否为 HTTPS：req.secure 为真，或 x-forwarded-proto 首项为 https（反向代理场景）。 */
const isSecureRequest = (req) => {
  if (req.secure) {
    return true;
  }
  const forwardedProto = req.headers['x-forwarded-proto'];
  if (typeof forwardedProto === 'string') {
    const firstProto = forwardedProto.split(',')[0]?.trim().toLowerCase();
    return firstProto === 'https';
  }
  return false;
};

/**
 * 拼接 Set-Cookie 值：固定 Path=/、HttpOnly、SameSite=Lax；maxAge 为数字时
 * 附 Max-Age 与 Expires（0 用 epoch 时间立即过期），secure 为真时追加 Secure。
 */
const buildCookie = ({ name, value, maxAge, secure }) => {
  const attributes = [
    `${name}=${value}`,
    'Path=/',
    'HttpOnly',
    'SameSite=Lax',
  ];

  if (typeof maxAge === 'number') {
    attributes.push(`Max-Age=${Math.max(0, Math.floor(maxAge))}`);
  }

  const expires = maxAge === 0
    ? 'Thu, 01 Jan 1970 00:00:00 GMT'
    : new Date(Date.now() + maxAge * 1000).toUTCString();

  attributes.push(`Expires=${expires}`);

  if (secure) {
    attributes.push('Secure');
  }

  return attributes.join('; ');
};

/** 当前毫秒时间戳（统一的时间取用入口，便于测试替身）。 */
const nowTs = () => Date.now();

/** 计算 token 的 SHA-256 十六进制摘要；存储侧只落哈希、绝不落 token 原文。 */
const hashToken = (token) => crypto.createHash('sha256').update(token).digest('hex');

/** 规整 host：trim、转小写、去掉末尾端口；空串或非字符串返回 null。 */
const normalizeHost = (candidate) => {
  if (typeof candidate !== 'string') {
    return null;
  }
  const trimmed = candidate.trim().toLowerCase();
  if (!trimmed) {
    return null;
  }
  return trimmed.replace(/:\d+$/, '');
};

/**
 * 规整 IP 字符串：去掉 IPv6 方括号与 zone 后缀（% 后部分），把
 * ::ffff: 映射的 IPv6 还原为点分 IPv4；规整后为空或输入非法返回 null。
 */
const normalizeIpCandidate = (candidate) => {
  if (typeof candidate !== 'string') {
    return null;
  }

  const trimmed = candidate.trim().toLowerCase();
  if (!trimmed) {
    return null;
  }

  const withoutBrackets = trimmed.startsWith('[') && trimmed.endsWith(']')
    ? trimmed.slice(1, -1)
    : trimmed;

  const withoutZone = withoutBrackets.split('%')[0];
  if (!withoutZone) {
    return null;
  }

  if (withoutZone.startsWith('::ffff:')) {
    const mappedIpv4 = withoutZone.slice('::ffff:'.length);
    if (/^\d+\.\d+\.\d+\.\d+$/.test(mappedIpv4)) {
      return mappedIpv4;
    }
  }

  return withoutZone;
};

/** 取 socket 层的远端 IP（remoteAddress，兼容旧 connection 字段）并规整。 */
const getSocketRemoteIp = (req) => {
  const remoteAddress = req?.socket?.remoteAddress || req?.connection?.remoteAddress;
  return normalizeIpCandidate(remoteAddress);
};

/**
 * 判断 IPv4 是否为回环（127/8）或私有地址：10/8、172.16-31、192.168/16、
 * 169.254/16（链路本地）。格式不合法直接返回 false。
 */
const isPrivateOrLoopbackIpv4 = (candidate) => {
  const octets = candidate.split('.').map((part) => Number(part));
  if (octets.length !== 4 || octets.some((part) => !Number.isInteger(part) || part < 0 || part > 255)) {
    return false;
  }

  const [first, second] = octets;
  if (first === 127) {
    return true;
  }
  if (first === 10) {
    return true;
  }
  if (first === 172 && second >= 16 && second <= 31) {
    return true;
  }
  if (first === 192 && second === 168) {
    return true;
  }
  if (first === 169 && second === 254) {
    return true;
  }
  return false;
};

/** 判断 IPv6 是否为回环（::1）、ULA 私有地址（fc/fd 前缀）或链路本地（fe8~feb 前缀）。 */
const isPrivateOrLoopbackIpv6 = (candidate) => {
  if (candidate === '::1') {
    return true;
  }

  if (candidate.startsWith('fc') || candidate.startsWith('fd')) {
    return true;
  }

  return candidate.startsWith('fe8')
    || candidate.startsWith('fe9')
    || candidate.startsWith('fea')
    || candidate.startsWith('feb');
};

/** 判断（先规整的）IP 是否私有/回环：含冒号走 IPv6 判定，否则走 IPv4 判定。 */
const isPrivateOrLoopbackIp = (candidate) => {
  const normalized = normalizeIpCandidate(candidate);
  if (!normalized) {
    return false;
  }

  if (normalized.includes(':')) {
    return isPrivateOrLoopbackIpv6(normalized);
  }

  return isPrivateOrLoopbackIpv4(normalized);
};

/**
 * 判断请求是否来自本机：host 为 localhost / host.docker.internal / 私有 IP，
 * 且 socket 远端地址也是私有或回环 IP——两者都满足才算本地，防止借私网域名绕过。
 */
const isLocalHost = (host, req) => {
  if (!host) {
    return false;
  }

  const isLocalHostname = host === 'localhost'
    || host === 'host.docker.internal'
    || isPrivateOrLoopbackIp(host);
  return isLocalHostname && isPrivateOrLoopbackIp(getSocketRemoteIp(req));
};

/**
 * 取客户端 IP：优先 x-forwarded-for 的首项，其次 req.ip / socket 远端地址；
 * ::ffff: 映射还原为 IPv4 点分形式；都取不到返回 null。
 */
const getClientIp = (req) => {
  const forwarded = req.headers['x-forwarded-for'];
  if (typeof forwarded === 'string') {
    const ip = forwarded.split(',')[0].trim();
    if (ip.startsWith('::ffff:')) {
      return ip.substring(7);
    }
    return ip;
  }

  const ip = req.ip || req.connection?.remoteAddress;
  if (ip) {
    if (ip.startsWith('::ffff:')) {
      return ip.substring(7);
    }
    return ip;
  }
  return null;
};

/** connect 限流的键：客户端 IP；取不到 IP 时退化为共享的 no-ip 聚合键。 */
const getRateLimitKey = (req) => {
  const ip = getClientIp(req);
  if (ip) {
    return ip;
  }
  return 'connect-rate-limit:no-ip';
};

/** 该限流键允许的最大失败次数：no-ip 聚合键用更小阈值，其余用常规阈值。 */
const rateLimitMaxForKey = (key) => {
  if (key === 'connect-rate-limit:no-ip') {
    return CONNECT_RATE_LIMIT_NO_IP_MAX_ATTEMPTS;
  }
  return CONNECT_RATE_LIMIT_MAX_ATTEMPTS;
};

/**
 * 创建隧道认证器（有状态，通常每个服务实例创建一个）。
 *
 * 返回的方法面涵盖：请求来源分类（classifyRequestScope）、隧道生命周期
 * （setActiveTunnel / clearActiveTunnel / revokeTunnelArtifacts）、bootstrap
 * token 的签发与状态（issueBootstrapToken / getBootstrapStatus）、会话管理
 * （requireTunnelSession 中间件 / getTunnelSessionFromRequest /
 * exchangeBootstrapToken / listTunnelSessions / clearTunnelSessionCookie），
 * 以及活跃隧道 id / host / mode 三个 getter。
 */
export const createTunnelAuth = () => {
  // 认证器内部状态：活跃隧道的 id/host/模式/公网 URL、一次性 bootstrap
  // token 记录、隧道会话表（sessionId → 记录）与 connect 限流表（键 → 计数）。
  let activeTunnelId = null;
  let activeTunnelHost = null;
  let activeTunnelMode = null;
  let activeTunnelPublicUrl = null;
  let bootstrapRecord = null;

  const tunnelSessions = new Map();
  const connectRateLimiter = new Map();

  /** 用 Max-Age=0 立即清掉隧道会话 cookie（是否加 Secure 跟随请求的 HTTPS 状态）。 */
  const clearTunnelSessionCookie = (req, res) => {
    const secure = isSecureRequest(req);
    const header = buildCookie({
      name: TUNNEL_SESSION_COOKIE_NAME,
      value: '',
      maxAge: 0,
      secure,
    });
    res.setHeader('Set-Cookie', header);
  };

  /** 把 sessionId 写入 HttpOnly 会话 cookie，有效期 ttlMs 毫秒；值经 URL 编码。 */
  const setTunnelSessionCookie = (req, res, sessionId, ttlMs) => {
    const secure = isSecureRequest(req);
    const maxAge = Math.max(0, Math.floor(ttlMs / 1000));
    const header = buildCookie({
      name: TUNNEL_SESSION_COOKIE_NAME,
      value: encodeURIComponent(sessionId),
      maxAge,
      secure,
    });
    res.setHeader('Set-Cookie', header);
  };

  /**
   * 判定请求来源：host 等于活跃隧道 host → 'tunnel'；本机来源 → 'local'
   * （没有活跃隧道时也一律视为 local）；其余公网来源 → 'unknown-public'。
   */
  const classifyRequestScope = (req) => {
    const hostHeader = normalizeHost(typeof req.headers.host === 'string' ? req.headers.host : '');
    const reqHost = normalizeHost(typeof req.hostname === 'string' ? req.hostname : '') || hostHeader;

    if (activeTunnelHost && reqHost === activeTunnelHost) {
      return 'tunnel';
    }

    if (isLocalHost(reqHost, req)) {
      return 'local';
    }

    if (!activeTunnelId) {
      return 'local';
    }

    return 'unknown-public';
  };

  /** 吊销当前 bootstrap token（幂等）：未持有或已吊销返回 0，实际完成吊销返回 1。 */
  const revokeBootstrapToken = () => {
    if (!bootstrapRecord) {
      return 0;
    }
    if (bootstrapRecord.revokedAt) {
      return 0;
    }
    if (!bootstrapRecord.revokedAt) {
      bootstrapRecord.revokedAt = nowTs();
    }
    return 1;
  };

  /** 吊销属于指定隧道的全部未吊销会话（写入 revokedAt/revokedReason），返回吊销数量。 */
  const invalidateTunnelSessions = (tunnelId, reason = 'tunnel-stopped') => {
    const revokedAt = nowTs();
    let count = 0;
    for (const record of tunnelSessions.values()) {
      if (record.tunnelId === tunnelId && !record.revokedAt) {
        record.revokedAt = revokedAt;
        record.revokedReason = reason;
        count += 1;
      }
    }
    return count;
  };

  /** 隧道停止或被吊销时的收尾：吊销对应 bootstrap token 并失效其全部会话，返回两项计数。 */
  const revokeTunnelArtifacts = (tunnelId) => {
    const revokedBootstrapCount = bootstrapRecord && bootstrapRecord.tunnelId === tunnelId
      ? revokeBootstrapToken()
      : 0;
    const invalidatedSessionCount = invalidateTunnelSessions(tunnelId, 'tunnel-revoked');
    return { revokedBootstrapCount, invalidatedSessionCount };
  };

  /** 登记活跃隧道：记录 id、模式与公网 URL，并尝试从 publicUrl 解析 host（失败置 null）。 */
  const setActiveTunnel = ({ tunnelId, publicUrl, mode = null }) => {
    activeTunnelId = tunnelId;
    activeTunnelMode = mode;
    activeTunnelPublicUrl = publicUrl || null;
    try {
      activeTunnelHost = normalizeHost(new URL(publicUrl).host);
    } catch {
      activeTunnelHost = null;
    }
  };

  /** 清除活跃隧道：先吊销该隧道的 token 与会话，再把全部状态复位为空。 */
  const clearActiveTunnel = () => {
    if (activeTunnelId) {
      revokeTunnelArtifacts(activeTunnelId);
    }
    activeTunnelId = null;
    activeTunnelHost = null;
    activeTunnelMode = null;
    activeTunnelPublicUrl = null;
    bootstrapRecord = null;
  };

  /** bootstrap 记录是否仍可用：存在、未吊销、未使用、且未过 expiresAt。 */
  const isBootstrapRecordUsable = (record) => {
    if (!record || record.revokedAt || record.usedAt) {
      return false;
    }
    if (typeof record.expiresAt === 'number' && nowTs() >= record.expiresAt) {
      return false;
    }
    return true;
  };

  /**
   * 签发一次性 bootstrap token：先吊销旧记录（同时只允许一个），再生成随机
   * token——内存里只保存其 SHA-256 哈希。返回 { token, expiresAt }；
   * ttlMs 非正数则永不过期（expiresAt 为 null）。隧道未激活时抛错。
   */
  const issueBootstrapToken = ({ ttlMs }) => {
    if (!activeTunnelId) {
      throw new Error('Tunnel is not active');
    }

    revokeBootstrapToken();

    const token = crypto.randomBytes(BOOTSTRAP_TOKEN_COOKIE_SAFE_BYTES).toString('base64url');
    const issuedAt = nowTs();
    const expiresAt = Number.isFinite(ttlMs) && ttlMs > 0 ? issuedAt + ttlMs : null;

    bootstrapRecord = {
      id: crypto.randomUUID(),
      tunnelId: activeTunnelId,
      tokenHash: hashToken(token),
      issuedAt,
      expiresAt,
      usedAt: null,
      revokedAt: null,
    };

    return {
      token,
      expiresAt,
    };
  };

  /** 查询 bootstrap token 状态：是否存在可用 token 及其过期时间。 */
  const getBootstrapStatus = () => {
    if (!isBootstrapRecordUsable(bootstrapRecord)) {
      return {
        hasBootstrapToken: false,
        bootstrapExpiresAt: null,
      };
    }

    return {
      hasBootstrapToken: true,
      bootstrapExpiresAt: bootstrapRecord.expiresAt,
    };
  };

  /**
   * 检查 connect 是否被限流：锁定期内拒绝并返回剩余 retryAfter 秒；距上次
   * 尝试超过窗口则视为重新开始；窗口内失败次数达到上限则锁定
   * CONNECT_RATE_LIMIT_LOCK_MS 并拒绝。放行时不改计数。
   */
  const checkConnectRateLimit = (req) => {
    const key = getRateLimitKey(req);
    const now = nowTs();
    const maxAttempts = rateLimitMaxForKey(key);
    const record = connectRateLimiter.get(key);

    if (record?.lockedUntil && now < record.lockedUntil) {
      return {
        allowed: false,
        retryAfter: Math.ceil((record.lockedUntil - now) / 1000),
      };
    }

    if (!record || now - record.lastAttempt > CONNECT_RATE_LIMIT_WINDOW_MS) {
      return { allowed: true, retryAfter: 0 };
    }

    if (record.count >= maxAttempts) {
      const lockedUntil = now + CONNECT_RATE_LIMIT_LOCK_MS;
      connectRateLimiter.set(key, {
        count: record.count + 1,
        lastAttempt: now,
        lockedUntil,
      });
      return {
        allowed: false,
        retryAfter: Math.ceil(CONNECT_RATE_LIMIT_LOCK_MS / 1000),
      };
    }

    return { allowed: true, retryAfter: 0 };
  };

  /** 记录一次失败的 connect 尝试：窗口已过期则从 1 重新计数，否则在原记录上累加。 */
  const recordConnectFailedAttempt = (req) => {
    const key = getRateLimitKey(req);
    const now = nowTs();
    const record = connectRateLimiter.get(key);

    if (!record || now - record.lastAttempt > CONNECT_RATE_LIMIT_WINDOW_MS) {
      connectRateLimiter.set(key, { count: 1, lastAttempt: now, lockedUntil: null });
      return;
    }

    connectRateLimiter.set(key, {
      count: record.count + 1,
      lastAttempt: now,
      lockedUntil: record.lockedUntil || null,
    });
  };

  /** 认证成功后清除该客户端的限流记录。 */
  const clearConnectRateLimit = (req) => {
    const key = getRateLimitKey(req);
    connectRateLimiter.delete(key);
  };

  /**
   * 从请求 cookie 中取有效隧道会话：无 cookie、会话未知、已吊销、已过期
   * （顺带补记 expiredAt）或属于其它隧道时都返回 null；有效则刷新
   * lastSeenAt 并返回会话记录。
   */
  const getTunnelSessionFromRequest = (req) => {
    const cookies = parseCookies(req.headers.cookie);
    const token = cookies[TUNNEL_SESSION_COOKIE_NAME];
    if (!token) {
      return null;
    }
    const session = tunnelSessions.get(token);
    if (!session) {
      return null;
    }
    if (session.revokedAt) {
      return null;
    }
    if (session.expiresAt <= nowTs()) {
      if (!session.expiredAt) {
        session.expiredAt = nowTs();
      }
      return null;
    }
    if (session.tunnelId !== activeTunnelId) {
      return null;
    }
    session.lastSeenAt = nowTs();
    return session;
  };

  /** express 中间件：无有效隧道会话时清 cookie 并回 401（tunnelLocked 标记）；有会话则放行。 */
  const requireTunnelSession = (req, res, next) => {
    const session = getTunnelSessionFromRequest(req);
    if (session) {
      return next();
    }

    clearTunnelSessionCookie(req, res);
    res.status(401).json({
      error: 'Tunnel authentication required',
      locked: true,
      tunnelLocked: true,
    });
  };

  /**
   * 用 bootstrap token 换取隧道会话。校验链：限流 → 隧道与记录存在 →
   * token 非空 → 记录可用（未用未吊销未过期）→ 隧道匹配 → 哈希一致
   * （定长比较 + crypto.timingSafeEqual，防时序侧信道）；任一失败都记一次
   * 失败尝试并返回对应 reason。成功则标记 token 已用、清限流记录、生成
   * 新会话（有效期 sessionTtlMs）并种下 HttpOnly cookie。
   */
  const exchangeBootstrapToken = ({ req, res, token, sessionTtlMs }) => {
    const rateLimit = checkConnectRateLimit(req);
    if (!rateLimit.allowed) {
      return {
        ok: false,
        reason: 'rate-limited',
        retryAfter: rateLimit.retryAfter,
      };
    }

    if (!activeTunnelId || !bootstrapRecord) {
      recordConnectFailedAttempt(req);
      return { ok: false, reason: 'inactive' };
    }

    if (!token || typeof token !== 'string') {
      recordConnectFailedAttempt(req);
      return { ok: false, reason: 'missing-token' };
    }

    if (!isBootstrapRecordUsable(bootstrapRecord)) {
      recordConnectFailedAttempt(req);
      return { ok: false, reason: 'expired' };
    }

    if (bootstrapRecord.tunnelId !== activeTunnelId) {
      recordConnectFailedAttempt(req);
      return { ok: false, reason: 'tunnel-mismatch' };
    }

    const incomingHash = hashToken(token);
    const expected = bootstrapRecord.tokenHash;
    const validHash = incomingHash.length === expected.length
      && crypto.timingSafeEqual(Buffer.from(incomingHash), Buffer.from(expected));

    if (!validHash) {
      recordConnectFailedAttempt(req);
      return { ok: false, reason: 'invalid-token' };
    }

    bootstrapRecord.usedAt = nowTs();
    clearConnectRateLimit(req);

    const sessionId = crypto.randomBytes(32).toString('base64url');
    const createdAt = nowTs();
    const expiresAt = createdAt + sessionTtlMs;

    tunnelSessions.set(sessionId, {
      sessionId,
      tunnelId: activeTunnelId,
      mode: activeTunnelMode,
      publicUrl: activeTunnelPublicUrl,
      createdAt,
      lastSeenAt: createdAt,
      expiresAt,
      revokedAt: null,
      revokedReason: null,
      expiredAt: null,
    });

    setTunnelSessionCookie(req, res, sessionId, sessionTtlMs);

    return {
      ok: true,
      sessionExpiresAt: expiresAt,
    };
  };

  /**
   * 列出全部隧道会话（含已失效的）：逐条计算 active/inactive 状态与失效原因
   * （revoked / expired / inactive），按创建时间倒序返回；顺带给过期会话补记 expiredAt。
   */
  const listTunnelSessions = () => {
    const now = nowTs();

    const sessions = [];
    for (const record of tunnelSessions.values()) {
      const isExpired = record.expiresAt <= now;
      if (isExpired && !record.expiredAt) {
        record.expiredAt = now;
      }

      const active = !record.revokedAt && !isExpired && record.tunnelId === activeTunnelId;
      const status = active ? 'active' : 'inactive';
      const inactiveReason = record.revokedAt ? (record.revokedReason || 'revoked') : (isExpired ? 'expired' : 'inactive');

      sessions.push({
        sessionId: record.sessionId,
        tunnelId: record.tunnelId,
        mode: record.mode,
        publicUrl: record.publicUrl,
        createdAt: record.createdAt,
        lastSeenAt: record.lastSeenAt,
        expiresAt: record.expiresAt,
        revokedAt: record.revokedAt,
        status,
        inactiveReason: status === 'inactive' ? inactiveReason : null,
      });
    }

    sessions.sort((a, b) => b.createdAt - a.createdAt);
    return sessions;
  };

  // 暴露的认证器 API：来源分类、隧道生命周期、token 签发/校验、
  // 会话管理与查询，以及活跃隧道 id / host / mode 三个 getter。
  return {
    classifyRequestScope,
    setActiveTunnel,
    clearActiveTunnel,
    revokeTunnelArtifacts,
    issueBootstrapToken,
    getBootstrapStatus,
    requireTunnelSession,
    getTunnelSessionFromRequest,
    exchangeBootstrapToken,
    listTunnelSessions,
    clearTunnelSessionCookie,
    getActiveTunnelId: () => activeTunnelId,
    getActiveTunnelHost: () => activeTunnelHost,
    getActiveTunnelMode: () => activeTunnelMode,
  };
};
