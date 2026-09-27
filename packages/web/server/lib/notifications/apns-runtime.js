// APNs (Apple Push Notification service) runtime for the native iOS mobile app.
//
// Device tokens are persisted per UI session (mirrors push-runtime.js). Delivery has two
// modes, chosen at send time:
//   - Relay (default): POST tokens + generic text to the central Cloudflare relay, which
//     holds the single project APNs key and signs+sends — so users configure nothing.
//   - Direct (fallback): sign an ES256 JWT with Node crypto and send over HTTP/2 ourselves,
//     for self-hosters who set OMPCHAMBER_APNS_* and OMPCHAMBER_PUSH_RELAY_DISABLED=true.
// Wired into the same trigger fanout as web push (see runtime.js); the relay carries only
// generic, model-based text (no session content) — see APNS.md.
/**
 * APNs（Apple Push Notification service）运行时，服务原生 iOS 移动端。
 *
 * 设备 token 按 UI 会话持久化（与 push-runtime.js 相同的文件形态与写锁模式）。
 * 投递在发送时二选一：Relay（默认）——把 token 与通用文案 POST 到中央
 * Cloudflare relay，由其持有项目唯一的 APNs 密钥并签名发送，用户零配置；
 * Direct（兜底）——自托管者设置 OMPCHAMBER_APNS_* 且
 * OMPCHAMBER_PUSH_RELAY_DISABLED=true 时，用 Node crypto 签 ES256 JWT、
 * 经 HTTP/2 直发。与 web push 共用同一触发扇出（见 runtime.js）；
 * relay 只携带通用的、基于模型的文案（不含会话内容），详见 APNS.md。
 */

import {
  getOrCreateRelaySigningKeypair,
  signRelayMessage as signRelayMessageShared,
} from '../relay/signing-key.js';

/** APNs token 存储文件的格式版本；version 不符的文件按空数据处理。 */
const APNS_TOKENS_VERSION = 1;
/** APNs 生产环境 API 地址（直连模式）。 */
const APNS_HOST_PRODUCTION = 'https://api.push.apple.com';
/** APNs 沙盒环境 API 地址（直连模式）。 */
const APNS_HOST_SANDBOX = 'https://api.sandbox.push.apple.com';
// APNs rejects auth tokens older than 1h; refresh well inside that window.
/** 缓存的 APNs JWT 有效期（50 分钟）；APNs 拒绝超过 1 小时的 token，须在窗口内刷新。 */
const JWT_TTL_MS = 50 * 60 * 1000;
/** 未配置 bundleId 时使用的默认 iOS bundle 标识。 */
const DEFAULT_BUNDLE_ID = 'com.openchamber.app';
/** 默认中央 push relay 的发送端点。 */
const DEFAULT_RELAY_URL = 'https://api.openchamber.dev/v1/push/send';
/** 每个 UI 会话最多保存的设备 token 数。 */
const MAX_TOKENS_PER_SESSION = 10;
// APNs reasons that mean the token is permanently invalid → drop it.
/** 表示 token 永久失效、应从存储中删除的 APNs 错误原因集合。 */
const DEAD_TOKEN_REASONS = new Set(['BadDeviceToken', 'Unregistered', 'DeviceTokenNotForTopic']);

/** 读取环境变量并 trim；不存在或为空白时返回 null。 */
const trimmedEnv = (name) => {
  const value = process.env[name];
  return typeof value === 'string' && value.trim().length > 0 ? value.trim() : null;
};

// Env vars commonly store the .p8 with literal "\n" sequences; restore real newlines.
/** 规整 PEM 私钥：环境变量常以字面量反斜杠 n 序列存储换行，这里还原真实换行并去首尾空白。 */
const normalizePem = (value) => (typeof value === 'string' ? value.replace(/\\n/g, '\n').trim() : '');

/**
 * 创建 APNs 运行时。
 * deps 注入 fsPromises/path/crypto/http2、token 存储路径 APNS_TOKENS_FILE_PATH、
 * 设置读写 readSettingsFromDiskMigrated / writeSettingsToDisk，以及
 * readSettingsStrict（签名密钥再生成时的严格读取，见 signing-key.js）。
 */
export const createApnsRuntime = (deps) => {
  const {
    fsPromises,
    path,
    crypto,
    http2,
    APNS_TOKENS_FILE_PATH,
    readSettingsFromDiskMigrated,
    writeSettingsToDisk,
    // Strict settings reader gating identity regeneration (see signing-key.js).
    readSettingsStrict,
  } = deps;

  /** 串行化 token 文件读改写的 Promise 链锁（与 push-runtime.js 同模式）。 */
  let persistLock = Promise.resolve();
  /** 缓存的 APNs JWT 及其签发时间与 keyId，避免每次发送都重新签名。 */
  let cachedJwt = null; // { token, issuedAtMs, keyId }
  /** 缓存的 relay 签名密钥对 { privateKey, publicJwk }。 */
  let cachedRelayKey = null; // { privateKey, publicJwk }
  /** 「直连模式未配置」告警只提示一次的标记。 */
  let warnedUnconfigured = false;

  // ---------------------------------------------------------------------------
  // Per-server relay signing identity (ECDSA P-256). Auto-generated + persisted in settings
  // (mirrors getOrCreateVapidKeys). The relay derives serverId = SHA-256(publicKey), verifies
  // each request's signature, and only delivers to tokens this server registered — so a leaked
  // device token alone can't be used to push. Zero-config: the keypair generates on first use.
  // ---------------------------------------------------------------------------

  // Key access lives in lib/relay/signing-key.js now (shared with the private
  // relay identity — same keypair, same storage, same serverId derivation).
  /** 获取（或首次生成并持久化到设置中的）relay 签名密钥对；结果做进程内缓存。 */
  const getOrCreateRelayKeypair = async () => {
    if (cachedRelayKey) return cachedRelayKey;
    cachedRelayKey = await getOrCreateRelaySigningKeypair({ crypto, readSettingsFromDiskMigrated, writeSettingsToDisk, readSettingsStrict });
    return cachedRelayKey;
  };

  /** 用共享实现（signing-key.js）对 message 做 ECDSA 签名，返回 base64url 签名。 */
  const signRelayMessage = (privateKey, message) => signRelayMessageShared({ crypto }, privateKey, message);

  // Trim to the 4 fields the relay's schema accepts (and that feed the serverId hash).
  /** 把公钥 JWK 裁剪为 relay schema 接受（并参与 serverId 哈希）的 4 个字段。 */
  const relayPublicJwk = (publicJwk) => ({
    kty: publicJwk.kty,
    crv: publicJwk.crv,
    x: publicJwk.x,
    y: publicJwk.y,
  });

  /**
   * 向 relay 注册（绑定）设备 token：以 `ts.token.platform` 作为被签消息，
   * 使 platform 无法被中途篡改。direct 模式（relay 为 null）无需绑定，直接跳过；
   * 请求失败仅告警，不影响本地持久化。
   */
  const registerTokenWithRelay = async (token, platform = 'ios') => {
    const relay = resolveRelayConfig();
    if (!relay) return; // direct mode — no relay binding needed
    try {
      const { privateKey, publicJwk } = await getOrCreateRelayKeypair();
      const ts = Date.now();
      // platform is part of the signed message so it can't be tampered en route.
      const sig = signRelayMessage(privateKey, `${ts}.${token}.${platform}`);
      const res = await fetch(relay.registerUrl, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ token, platform, publicKeyJwk: relayPublicJwk(publicJwk), ts, sig }),
      });
      if (!res.ok) console.warn(`[Push relay] register-token failed status=${res.status}`);
    } catch (error) {
      console.warn('[Push relay] register-token request failed:', error?.message ?? error);
    }
  };

  // ---------------------------------------------------------------------------
  // Token persistence (same shape + write-lock pattern as push-runtime.js)
  // ---------------------------------------------------------------------------

  /** 构造空的 token 存储结构。 */
  const emptyStore = () => ({ version: APNS_TOKENS_VERSION, tokensBySession: {} });

  /**
   * 读取并校验磁盘上的 token 文件：文件缺失（ENOENT）、解析失败或
   * version 不符时返回空结构，绝不抛错（仅告警）。
   */
  const readTokensFromDisk = async () => {
    try {
      const raw = await fsPromises.readFile(APNS_TOKENS_FILE_PATH, 'utf8');
      const parsed = JSON.parse(raw);
      if (!parsed || typeof parsed !== 'object' || parsed.version !== APNS_TOKENS_VERSION) {
        return emptyStore();
      }
      const tokensBySession =
        parsed.tokensBySession && typeof parsed.tokensBySession === 'object' ? parsed.tokensBySession : {};
      return { version: APNS_TOKENS_VERSION, tokensBySession };
    } catch (error) {
      if (error && typeof error === 'object' && error.code === 'ENOENT') {
        return emptyStore();
      }
      console.warn('Failed to read APNs tokens file:', error);
      return emptyStore();
    }
  };

  /** 把 token 数据以两空格缩进的 JSON 写回磁盘，先确保父目录存在。 */
  const writeTokensToDisk = async (data) => {
    await fsPromises.mkdir(path.dirname(APNS_TOKENS_FILE_PATH), { recursive: true });
    await fsPromises.writeFile(APNS_TOKENS_FILE_PATH, JSON.stringify(data, null, 2), 'utf8');
  };

  /** 串行执行一次 token 文件的读改写（经 persistLock 排队），返回更新后的数据。 */
  const persistTokenUpdate = async (mutate) => {
    persistLock = persistLock.then(async () => {
      const current = await readTokensFromDisk();
      const next = mutate({ version: APNS_TOKENS_VERSION, tokensBySession: current.tokensBySession || {} });
      await writeTokensToDisk(next);
      return next;
    });
    return persistLock;
  };

  /**
   * 规整某会话的 token 记录数组：剔除 deviceToken 缺失的非法项，
   * 并为 platform（缺省 ios）与 environment（缺省 production）补默认值。
   */
  const normalizeTokens = (record) => {
    if (!Array.isArray(record)) return [];
    return record
      .map((entry) => {
        if (!entry || typeof entry !== 'object') return null;
        const deviceToken = entry.deviceToken;
        if (typeof deviceToken !== 'string' || deviceToken.trim().length === 0) return null;
        return {
          deviceToken: deviceToken.trim(),
          createdAt: typeof entry.createdAt === 'number' ? entry.createdAt : null,
          lastSeenAt: typeof entry.lastSeenAt === 'number' ? entry.lastSeenAt : null,
          userAgent: typeof entry.userAgent === 'string' ? entry.userAgent : undefined,
          // 'ios' (APNs) or 'android' (FCM). Older entries without one are APNs by default.
          platform: entry.platform === 'android' ? 'android' : 'ios',
          // APNs delivery environment for this token. Xcode/dev-signed installs produce
          // sandbox tokens, TestFlight/App Store produce production ones; the client reports
          // which at registration. Older entries without one default to production (matches
          // released builds).
          environment: entry.environment === 'sandbox' ? 'sandbox' : 'production',
        };
      })
      .filter(Boolean);
  };

  // Normalize an incoming platform hint to the two we support; default to APNs/iOS since that
  // was the only registrant before Android/FCM existed.
  /** 把平台提示归一为受支持的两种；历史上只有 iOS 注册，缺省 ios。 */
  const normalizePlatform = (platform) => (platform === 'android' ? 'android' : 'ios');

  /** 把环境提示归一为 sandbox / production；缺省 production。 */
  const normalizeEnvironment = (environment) => (environment === 'sandbox' ? 'sandbox' : 'production');

  /**
   * 为指定 UI 会话新增或更新设备 token：按 token 去重、新条目置顶，
   * 记录 createdAt/lastSeenAt/userAgent/platform/environment，
   * 每会话最多保留 MAX_TOKENS_PER_SESSION 条。
   * 之后总是向 relay 幂等重新绑定该 token：设备每次启动都会重发 token，
   * 每次绑定可让 relay 升级后存量 token 不至于静默失绑；
   * platform 一并绑定，供 relay 选择 APNs 或 FCM 通道。
   */
  const addOrUpdateApnsToken = async (uiSessionToken, deviceToken, userAgent, platform, environment) => {
    if (!uiSessionToken || typeof deviceToken !== 'string' || deviceToken.trim().length === 0) return;
    const token = deviceToken.trim();
    const tokenPlatform = normalizePlatform(platform);
    const tokenEnvironment = normalizeEnvironment(environment);
    const now = Date.now();

    await persistTokenUpdate((current) => {
      const tokensBySession = { ...(current.tokensBySession || {}) };
      const existing = normalizeTokens(tokensBySession[uiSessionToken]);
      const filtered = existing.filter((entry) => entry.deviceToken !== token);
      filtered.unshift({
        deviceToken: token,
        createdAt: now,
        lastSeenAt: now,
        userAgent: typeof userAgent === 'string' && userAgent.length > 0 ? userAgent : undefined,
        platform: tokenPlatform,
        environment: tokenEnvironment,
      });
      tokensBySession[uiSessionToken] = filtered.slice(0, MAX_TOKENS_PER_SESSION);
      return { version: APNS_TOKENS_VERSION, tokensBySession };
    });

    // (Re)bind this token to our server on the relay so only we can push to it. The device
    // re-sends its token on each launch; this is an idempotent upsert relay-side, and binding
    // every time (not just for new tokens) keeps existing tokens bound after a relay/server
    // upgrade rather than silently going unbound. Platform is bound too so the relay routes
    // it to APNs vs FCM.
    await registerTokenWithRelay(token, tokenPlatform);
  };

  /** 删除指定会话中的某设备 token；会话 token 清空时连同该会话键一并移除。 */
  const removeApnsToken = async (uiSessionToken, deviceToken) => {
    if (!uiSessionToken || !deviceToken) return;
    await persistTokenUpdate((current) => {
      const tokensBySession = { ...(current.tokensBySession || {}) };
      const filtered = normalizeTokens(tokensBySession[uiSessionToken]).filter(
        (entry) => entry.deviceToken !== deviceToken,
      );
      if (filtered.length === 0) delete tokensBySession[uiSessionToken];
      else tokensBySession[uiSessionToken] = filtered;
      return { version: APNS_TOKENS_VERSION, tokensBySession };
    });
  };

  /** 从所有会话中删除某设备 token（APNs / relay 报告其失效时调用）。 */
  const removeApnsTokenFromAllSessions = async (deviceToken) => {
    if (!deviceToken) return;
    await persistTokenUpdate((current) => {
      const tokensBySession = { ...(current.tokensBySession || {}) };
      for (const [session, entries] of Object.entries(tokensBySession)) {
        const filtered = normalizeTokens(entries).filter((entry) => entry.deviceToken !== deviceToken);
        if (filtered.length === 0) delete tokensBySession[session];
        else tokensBySession[session] = filtered;
      }
      return { version: APNS_TOKENS_VERSION, tokensBySession };
    });
  };

  // ---------------------------------------------------------------------------
  // Config (env first, then settings.apnsConfig) — mirrors resolveVapidSubject
  // ---------------------------------------------------------------------------

  /**
   * 解析直连模式配置：环境变量优先（OMPCHAMBER_APNS_KEY_ID / TEAM_ID /
   * BUNDLE_ID / ENVIRONMENT / P8 / P8_PATH，P8_PATH 可从文件读取 .p8 私钥），
   * 缺失项回退到设置中的 apnsConfig。keyId/teamId/p8 任一缺失视为未配置，
   * 返回 null。environment 未显式指定时为 null——按每个 token 注册时的环境投递。
   */
  const resolveApnsConfig = async () => {
    let keyId = trimmedEnv('OMPCHAMBER_APNS_KEY_ID');
    let teamId = trimmedEnv('OMPCHAMBER_APNS_TEAM_ID');
    let bundleId = trimmedEnv('OMPCHAMBER_APNS_BUNDLE_ID');
    let environment = (trimmedEnv('OMPCHAMBER_APNS_ENVIRONMENT') || '').toLowerCase();
    let p8 = normalizePem(process.env.OMPCHAMBER_APNS_P8 || '');

    const p8Path = trimmedEnv('OMPCHAMBER_APNS_P8_PATH');
    if (!p8 && p8Path) {
      try {
        p8 = (await fsPromises.readFile(p8Path, 'utf8')).trim();
      } catch (error) {
        console.warn('[APNs] Failed to read OMPCHAMBER_APNS_P8_PATH:', error?.message ?? error);
      }
    }

    if (!keyId || !teamId || !p8) {
      try {
        const settings = await readSettingsFromDiskMigrated();
        const stored = settings?.apnsConfig;
        if (stored && typeof stored === 'object') {
          keyId = keyId || (typeof stored.keyId === 'string' ? stored.keyId.trim() : null);
          teamId = teamId || (typeof stored.teamId === 'string' ? stored.teamId.trim() : null);
          bundleId = bundleId || (typeof stored.bundleId === 'string' ? stored.bundleId.trim() : null);
          environment = environment || (typeof stored.environment === 'string' ? stored.environment.toLowerCase() : '');
          if (!p8 && typeof stored.p8 === 'string') p8 = normalizePem(stored.p8);
        }
      } catch {
        // settings unavailable — fall through to the unconfigured result
      }
    }

    if (!keyId || !teamId || !p8) return null;

    return {
      keyId,
      teamId,
      p8,
      bundleId: bundleId || DEFAULT_BUNDLE_ID,
      // Explicit env/settings value forces every send to that environment; when unset (null),
      // each token is delivered to the environment it registered with.
      environment: environment === 'sandbox' ? 'sandbox' : environment === 'production' ? 'production' : null,
    };
  };

  // ---------------------------------------------------------------------------
  // JWT (ES256, JOSE/raw signature) + HTTP/2 send
  // ---------------------------------------------------------------------------

  /**
   * 手工构造并签名 APNs 要求的 ES256 JWT（JOSE header + claims + P-1363 签名）：
   * header 带 kid（keyId），claims 带 iss（teamId）与 iat。
   */
  const signApnsJwt = (config) => {
    const header = Buffer.from(JSON.stringify({ alg: 'ES256', kid: config.keyId })).toString('base64url');
    const claims = Buffer.from(
      JSON.stringify({ iss: config.teamId, iat: Math.floor(Date.now() / 1000) }),
    ).toString('base64url');
    const signingInput = `${header}.${claims}`;
    const signature = crypto
      .sign('sha256', Buffer.from(signingInput), { key: config.p8, dsaEncoding: 'ieee-p1363' })
      .toString('base64url');
    return `${signingInput}.${signature}`;
  };

  /**
   * 获取带缓存的 APNs JWT：keyId 一致且签发未超过 JWT_TTL_MS 时复用缓存，
   * 否则重新签名并更新缓存。
   */
  const getJwt = (config) => {
    const now = Date.now();
    if (cachedJwt && cachedJwt.keyId === config.keyId && now - cachedJwt.issuedAtMs < JWT_TTL_MS) {
      return cachedJwt.token;
    }
    const token = signApnsJwt(config);
    cachedJwt = { token, issuedAtMs: now, keyId: config.keyId };
    return token;
  };

  /**
   * 构造 APNs 请求体 JSON：aps 内含标题/正文/角标/默认声音/thread-id（由 tag
   * 映射）与 mutable-content（唤醒 Notification Service Extension 以刷新桌面小组件），
   * payload.data 在顶层展开为自定义键。
   */
  const buildBody = (payload) => {
    const data = payload && typeof payload.data === 'object' && payload.data ? payload.data : {};
    return JSON.stringify({
      aps: {
        alert: {
          title: typeof payload?.title === 'string' ? payload.title : undefined,
          body: typeof payload?.body === 'string' ? payload.body : undefined,
        },
        badge: Number.isFinite(payload?.badge) && payload.badge >= 0 ? Math.trunc(payload.badge) : undefined,
        sound: 'default',
        'thread-id': typeof payload?.tag === 'string' ? payload.tag : undefined,
        // Wakes the Notification Service Extension so it can refresh the home/lock-screen
        // widgets (attention count + unread dot) from the push, even when the app is closed.
        // No extra network call — just an extra key on the push we already send.
        'mutable-content': 1,
      },
      ...data,
    });
  };

  /**
   * 经已有 HTTP/2 会话向单个设备 token 发送一条通知（Promise 永不 reject）。
   * 200 视为成功；410 或失败原因命中 DEAD_TOKEN_REASONS 时从所有会话删除该 token，
   * 其余失败仅告警。apns-collapse-id（截断到 64 字节）实现类似 web-push tag 的折叠去重。
   */
  const sendOne = (client, deviceToken, body, jwt, config) =>
    new Promise((resolve) => {
      const headers = {
        ':method': 'POST',
        ':path': `/3/device/${deviceToken}`,
        authorization: `bearer ${jwt}`,
        'apns-topic': config.bundleId,
        'apns-push-type': 'alert',
        'apns-priority': '10',
      };
      // collapse-id dedups like web-push tags; APNs caps it at 64 bytes.
      const collapseId = typeof config.tag === 'string' ? config.tag.slice(0, 64) : undefined;
      if (collapseId) headers['apns-collapse-id'] = collapseId;

      let req;
      try {
        req = client.request(headers);
      } catch (error) {
        console.warn('[APNs] request open failed:', error?.message ?? error);
        resolve();
        return;
      }

      let status = 0;
      let responseBody = '';
      req.on('response', (resHeaders) => {
        status = Number(resHeaders[':status']) || 0;
      });
      req.setEncoding('utf8');
      req.on('data', (chunk) => {
        responseBody += chunk;
      });
      req.on('end', async () => {
        if (status === 200) {
          resolve();
          return;
        }
        let reason = '';
        try {
          reason = JSON.parse(responseBody)?.reason || '';
        } catch {
          // non-JSON error body
        }
        if (status === 410 || DEAD_TOKEN_REASONS.has(reason)) {
          await removeApnsTokenFromAllSessions(deviceToken);
        } else {
          console.warn(`[APNs] push failed status=${status} reason=${reason || 'unknown'}`);
        }
        resolve();
      });
      req.on('error', (error) => {
        console.warn('[APNs] request error:', error?.message ?? error);
        resolve();
      });
      req.end(body);
    });

  // Relay mode (default): the single APNs key lives in the central Cloudflare relay, not on
  // each user's server — so users configure nothing. The server just POSTs device tokens +
  // generic text; the relay signs + sends and reports which tokens to drop. Direct mode (below)
  // is the fallback for self-hosters who set OMPCHAMBER_APNS_* and disable the relay.
  /**
   * 解析 relay 配置：OMPCHAMBER_PUSH_RELAY_DISABLED=true 时返回 null（转直连模式）；
   * 否则取 OMPCHAMBER_PUSH_RELAY_URL（默认官方 relay），并由发送 URL 推导
   * register-token 端点；environment 显式设置时强制覆盖每次发送的环境。
   */
  const resolveRelayConfig = () => {
    if (trimmedEnv('OMPCHAMBER_PUSH_RELAY_DISABLED') === 'true') return null;
    const url = trimmedEnv('OMPCHAMBER_PUSH_RELAY_URL') || DEFAULT_RELAY_URL;
    const override = (trimmedEnv('OMPCHAMBER_APNS_ENVIRONMENT') || '').toLowerCase();
    return {
      url,
      registerUrl: url.replace(/\/send$/, '/register-token'),
      // Explicit OMPCHAMBER_APNS_ENVIRONMENT forces every send to that environment; when
      // unset (null), each token is delivered to the environment it registered with.
      environment: override === 'sandbox' ? 'sandbox' : override === 'production' ? 'production' : null,
    };
  };

  /**
   * Relay 模式发送：最多取前 100 个 token，用 relay 密钥对
   * `ts.排序后的tokens.title` 规范形式签名后 POST；relay 返回 results 中
   * drop 为 true 的 token 会被从所有会话移除。任何失败仅告警，不向外抛出。
   */
  const sendViaRelay = async (deviceTokens, payload, relay, environment) => {
    const tokens = deviceTokens.slice(0, 100);
    const title = typeof payload?.title === 'string' && payload.title.length > 0 ? payload.title : 'OMPChamber';
    const { privateKey, publicJwk } = await getOrCreateRelayKeypair();
    const ts = Date.now();
    // Sign over the same canonical form the relay verifies: ts.sortedTokens.title.
    const sig = signRelayMessage(privateKey, `${ts}.${[...tokens].sort().join(',')}.${title}`);
    const requestBody = JSON.stringify({
      tokens,
      title,
      body: typeof payload?.body === 'string' ? payload.body : '',
      badge: Number.isFinite(payload?.badge) && payload.badge >= 0 ? Math.trunc(payload.badge) : undefined,
      collapseId: typeof payload?.tag === 'string' ? payload.tag.slice(0, 64) : undefined,
      env: environment,
      data: payload?.data && typeof payload.data === 'object' ? payload.data : undefined,
      publicKeyJwk: relayPublicJwk(publicJwk),
      ts,
      sig,
    });
    try {
      const res = await fetch(relay.url, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: requestBody,
      });
      if (!res.ok) {
        console.warn(`[APNs relay] send failed status=${res.status}`);
        return;
      }
      const data = await res.json().catch(() => null);
      const results = Array.isArray(data?.results) ? data.results : [];
      for (const result of results) {
        if (result && result.drop === true && typeof result.token === 'string') {
          await removeApnsTokenFromAllSessions(result.token);
        }
      }
    } catch (error) {
      console.warn('[APNs relay] request failed:', error?.message ?? error);
    }
  };

  /**
   * 直连模式发送：无配置时告警一次后放弃；否则复用缓存 JWT，
   * 按 APNs 环境分组、各建一条 HTTP/2 会话并发送（sandbox/production 混发会得到
   * BadDeviceToken 并被误判为死 token）。config.environment 显式设置时覆盖每组环境；
   * 会话出错即收尾，单条发送失败互不影响。
   */
  const sendViaDirectApns = async (tokenGroups, payload) => {
    const config = await resolveApnsConfig();
    if (!config) {
      if (!warnedUnconfigured) {
        warnedUnconfigured = true;
        console.warn(
          '[APNs] Relay disabled and no direct config; set OMPCHAMBER_APNS_KEY_ID / OMPCHAMBER_APNS_TEAM_ID / OMPCHAMBER_APNS_P8 for direct send.',
        );
      }
      return;
    }

    const jwt = getJwt(config);
    const body = buildBody(payload);
    const sendConfig = { ...config, tag: typeof payload?.tag === 'string' ? payload.tag : undefined };

    // One HTTP/2 session per APNs environment; a sandbox token sent to the production host
    // (or vice versa) gets BadDeviceToken and would be wrongly dropped as dead.
    for (const [environment, deviceTokens] of tokenGroups) {
      const effectiveEnvironment = config.environment ?? environment;
      const host = effectiveEnvironment === 'sandbox' ? APNS_HOST_SANDBOX : APNS_HOST_PRODUCTION;

      let client;
      try {
        client = http2.connect(host);
      } catch (error) {
        console.warn('[APNs] connect failed:', error?.message ?? error);
        continue;
      }

      await new Promise((resolve) => {
        let settled = false;
        const finish = () => {
          if (settled) return;
          settled = true;
          try {
            client.close();
          } catch {
            // ignore close errors
          }
          resolve();
        };
        client.on('error', (error) => {
          console.warn('[APNs] session error:', error?.message ?? error);
          finish();
        });
        Promise.all(
          deviceTokens.map((token) => sendOne(client, token, body, jwt, sendConfig)),
        ).finally(finish);
      });
    }
  };

  // NOT gated on UI visibility (unlike web push). A backgrounded WKWebView can't reliably
  // report "hidden" before iOS suspends it, so a visibility gate wrongly suppressed
  // background push for short responses. Instead we always send, and rely on iOS to NOT
  // display the alert while the app is foreground (presentationOptions: [] in
  // capacitor.config) — so there is no notification when the app is active, with no race.
  /**
   * 向所有会话的全部设备 token 发送 APNs 通知（跨会话按 token 去重）。
   * 与 web push 不同，不做 UI 可见性门控：后台 WKWebView 在 iOS 挂起前来不及
   * 上报隐藏状态，门控会错误抑制短回复的推送；改由 iOS 前台不展示 alert
   * 保证应用活跃时无通知且无竞态。token 按注册环境分组：relay 可用时逐组
   * 经 relay 发送，否则直连发送。
   */
  const sendApnsToAllUiSessions = async (payload, _options = {}) => {
    const store = await readTokensFromDisk();
    // Tokens are grouped by their registered APNs environment so each batch goes to the
    // endpoint that actually knows the token (Xcode builds → sandbox, TestFlight/App Store
    // → production). Mixing them gets BadDeviceToken and the token wrongly dropped as dead.
    const tokensByEnvironment = new Map();
    const seen = new Set();
    for (const record of Object.values(store.tokensBySession || {})) {
      for (const entry of normalizeTokens(record)) {
        if (seen.has(entry.deviceToken)) continue;
        seen.add(entry.deviceToken);
        const group = tokensByEnvironment.get(entry.environment) || [];
        group.push(entry.deviceToken);
        tokensByEnvironment.set(entry.environment, group);
      }
    }
    if (seen.size === 0) return;

    const relay = resolveRelayConfig();
    if (relay) {
      for (const [environment, deviceTokens] of tokensByEnvironment) {
        await sendViaRelay(deviceTokens, payload, relay, relay.environment ?? environment);
      }
      return;
    }
    await sendViaDirectApns(tokensByEnvironment, payload);
  };

  return {
    addOrUpdateApnsToken,
    removeApnsToken,
    removeApnsTokenFromAllSessions,
    sendApnsToAllUiSessions,
    resolveApnsConfig,
    // exposed for tests
    signApnsJwt,
  };
};
