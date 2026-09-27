// Private relay service: config persistence, lifecycle of the relay host
// client, and the /api/ompchamber/relay/* management routes.
//
// Config lives in the server settings file as `settings.privateRelay =
// { enabled, relayUrl }` (same storage precedent as tunnels/notifications).
// Routes are registered with the other OMPChamber feature routes, before the
// generic OpenCode proxy, and are covered by the same global UI auth gate.
//
// Cross-runtime parity note: relay host mode intentionally targets the web
// server runtime only in v1 (Electron shares this server in-process). The VS
// Code runtime does not host a relay; shared UI must treat these routes as
// web-runtime capabilities.
/**
 * 私有中继服务：配置持久化（settings.privateRelay = { enabled, relayUrl }）、
 * 中继 host 客户端的生命周期管理（含跨进程 host 锁的认领/接管/让位），
 * 以及 /api/ompchamber/relay/* 管理路由的注册。路由与其它 OMPChamber
 * 功能路由一起注册在通用 OpenCode proxy 之前，受同一道全局 UI 鉴权门
 * 保护。v1 仅面向 web server 运行时（Electron 与之共享进程；VS Code
 * 运行时不承载中继）。
 */

import express from 'express';

import { createRelayIdentityRuntime } from './identity.js';
import { startRelayHost } from './host-client.js';

/** 默认中继端点（官方 Cloudflare 中继的 WebSocket 地址）。 */
export const DEFAULT_RELAY_URL = 'wss://relay.ompchamber.dev/ws';

/** 校验值是否为合法的 ws:/wss: URL（仅协议判断，不限制主机）。 */
const isValidRelayUrl = (value) => {
  if (typeof value !== 'string') return false;
  try {
    const url = new URL(value.trim());
    return url.protocol === 'ws:' || url.protocol === 'wss:';
  } catch {
    return false;
  }
};

/**
 * 归一化中继 URL：非字符串、空白或协议非法时回退到 DEFAULT_RELAY_URL。
 * @param {unknown} value 存储或请求体中的 relayUrl
 * @returns {string}
 */
const normalizeRelayUrl = (value) => {
  if (typeof value !== 'string') return DEFAULT_RELAY_URL;
  const trimmed = value.trim();
  if (!trimmed || !isValidRelayUrl(trimmed)) return DEFAULT_RELAY_URL;
  return trimmed;
};

// 部署可以通过环境变量钉死中继端点（例如自建在自家 Cloudflare 账号/
// 域名上的中继）。设置且合法时它完全覆盖存储的设置项——host 连接、
// 配对 offer 与状态全部指向它，客户端再从 offer 自动继承。
// A deployment can pin the relay endpoint via env (e.g. a self-hosted relay on
// your own Cloudflare account/domain). When set and valid it overrides the
// stored setting entirely, so the host connection, the pairing offer, and the
// status all point at it — clients then inherit it from the offer automatically.
const envRelayUrlOverride = () => {
  const raw = process.env.OMPCHAMBER_RELAY_URL;
  if (typeof raw !== 'string' || !raw.trim() || !isValidRelayUrl(raw)) return null;
  return raw.trim();
};

/**
 * 创建中继服务实例。依赖注入 crypto、设置读写、回环端口与 host 锁；
 * 可选 hasRelayDemand 驱动“按需开关”生命周期。返回路由注册、启动、
 * 对账、状态与配对候选等句柄（见文件底部 return）。
 */
/**
 * @param {{
 *   crypto: typeof import('node:crypto'),
 *   readSettingsFromDiskMigrated: () => Promise<object>,
 *   writeSettingsToDisk: (settings: object) => Promise<void>,
 *   getLocalPort: () => number,
 *   logger?: Pick<Console, 'warn'>,
 * }} deps
 */
export const createRelayService = ({
  crypto,
  readSettingsFromDiskMigrated,
  writeSettingsToDisk,
  // Strict settings reader (throws on corrupt/unreadable) gating identity
  // regeneration — see identity.js/signing-key.js.
  readSettingsStrict,
  getLocalPort,
  // Returns true when any paired device or pending pairing session uses the
  // relay transport. The relay lifecycle is driven purely by this demand.
  hasRelayDemand = async () => false,
  // Per-machine claim (host-lock.js): all local instances share the same
  // serverId, so only ONE process may run the relay host at a time or they
  // evict each other at the relay worker ("Control replaced") and devices land
  // on a random instance. Optional: without it, behavior is pre-lock.
  hostLock = null,
  // When false, this instance never starts the relay host on its own (boot,
  // demand reconcile, or claim-watch takeover) — only an explicit user action
  // (enable, pairing) force-claims. Dev/debug instances set this so they do not
  // capture paired devices from the production instance sharing the data dir.
  allowPassiveHost = true,
  logger = console,
}) => {
  // 身份运行时：读取/生成 serverId 与签名、加密密钥（见 identity.js）。
  const identityRuntime = createRelayIdentityRuntime({ crypto, readSettingsFromDiskMigrated, writeSettingsToDisk, readSettingsStrict });

  // 当前 host 客户端实例；null 表示未运行（可能禁用或 standby）。
  let hostClient = null;
  // 无 host 客户端时的兜底状态（disabled 或 standby）。
  let status = { state: 'disabled', lastError: null, connectedClients: 0 };
  // 启用期间周期复核 host 锁认领：认领者死亡则 standby 实例接管；
  // 运行中的 host 遇到他人抢锁则让位停止。
  // Re-checks the claim while enabled: a standby instance takes over when the
  // claimant dies; a running host stands down when another process claims.
  let claimWatchTimer = null;
  // host 锁复核周期。
  const CLAIM_WATCH_INTERVAL_MS = 30_000;
  // standby 实例不立即抢夺被释放的锁：原 host 干净重启（应用更新、重新
  // 启动）会短暂释放锁，此时抢过来会把设备困在这个可能更旧的实例上。
  // 重启中的 host 启动时无需等待即可重新认领，永远赢得这个窗口。
  // A standby instance does not grab a freed claim immediately: a clean restart
  // of the previous host (app update, relaunch) releases the claim for a short
  // while, and taking it during that window strands the devices on this —
  // possibly older — instance. The restarting host reclaims at boot without any
  // wait, so it always wins the window.
  const CLAIM_TAKEOVER_GRACE_MS = 120_000;
  // 锁开始空闲的时刻（用于接管宽限计时）；null 表示未在计时。
  let claimFreeSinceMs = null;

  // 读取生效配置：enabled 标记 + 归一化后的 relayUrl；环境变量覆盖时
  // relayUrlLocked 为 true（存储项被忽略）。
  const readConfig = async () => {
    const settings = await readSettingsFromDiskMigrated();
    const stored = settings?.privateRelay;
    const override = envRelayUrlOverride();
    return {
      enabled: stored?.enabled === true,
      relayUrl: override ?? normalizeRelayUrl(stored?.relayUrl),
      // True when the endpoint is pinned by OMPCHAMBER_RELAY_URL (a self-hosted
      // relay); the stored setting is ignored while it is set.
      relayUrlLocked: override !== null,
    };
  };

  // 写回配置：合并进现有 settings，只动 privateRelay 字段。
  const writeConfig = async (config) => {
    const settings = await readSettingsFromDiskMigrated();
    await writeSettingsToDisk({
      ...settings,
      privateRelay: { enabled: config.enabled === true, relayUrl: normalizeRelayUrl(config.relayUrl) },
    });
  };

  // 停止并丢弃 host 客户端实例（不动配置与锁）。
  const stopHostClient = () => {
    if (!hostClient) return;
    hostClient.stop();
    hostClient = null;
  };

  // standby 状态工厂：说明锁被哪个本地进程持有。
  const standbyStatus = (holderPid) => ({
    state: 'standby',
    lastError: `relay host is owned by another local OMPChamber process (pid ${holderPid})`,
    connectedClients: 0,
  });

  // 认领监视器（启用期间运行）：standby 且锁空闲超过宽限期 -> 接管成为
  // host；运行中且锁被别的活进程持有 -> 让位转 standby。正是这个退让
  // 结束了互相驱逐的拉锯：输家必须停止重连，否则两边永远互相顶替。
  // Claim watcher, active while the relay is enabled:
  //   - standby → claimant died → take over (start our host);
  //   - running → another live process claimed → stand down (stop, standby).
  // This back-off is what actually ends the mutual-eviction fight: the loser
  // must STOP reconnecting, otherwise both keep replacing each other forever.
  const ensureClaimWatch = (relayUrl) => {
    if (!hostLock || claimWatchTimer) return;
    claimWatchTimer = setInterval(() => {
      void (async () => {
        try {
          if (hostClient) {
            if (!hostLock.holdsClaim() && hostLock.liveClaimantPid() !== null) {
              logger.warn('[Relay] host claim taken by another local instance — standing down');
              const holder = hostLock.liveClaimantPid();
              stopHostClient();
              status = standbyStatus(holder);
            }
            return;
          }
          if (status.state !== 'standby' || !allowPassiveHost) return;
          if (hostLock.liveClaimantPid() !== null) {
            claimFreeSinceMs = null;
            return;
          }
          if (claimFreeSinceMs === null) {
            claimFreeSinceMs = Date.now();
            return;
          }
          if (Date.now() - claimFreeSinceMs < CLAIM_TAKEOVER_GRACE_MS) return;
          if (hostLock.tryClaim()) {
            claimFreeSinceMs = null;
            logger.warn('[Relay] host claim stayed free — taking over the relay host');
            await start(relayUrl);
          }
        } catch (error) {
          logger.warn(`[Relay] claim watch failed: ${error?.message ?? error}`);
        }
      })();
    }, CLAIM_WATCH_INTERVAL_MS);
    if (typeof claimWatchTimer.unref === 'function') claimWatchTimer.unref();
  };

  // 停止认领监视器并清空接管计时。
  const stopClaimWatch = () => {
    if (!claimWatchTimer) return;
    clearInterval(claimWatchTimer);
    claimWatchTimer = null;
    claimFreeSinceMs = null;
  };

  // 启动 host 客户端。claim 取 'try'（常规尝试）或 'force'（显式用户
  // 动作强制抢锁）；allowPassiveHost 为 false 时被动启动直接转 standby。
  // 抢锁失败同样转 standby 并开启监视器等待接管。
  const start = async (relayUrl, { claim = 'try' } = {}) => {
    if (hostClient) return;
    if (claim !== 'force' && !allowPassiveHost) {
      status = {
        state: 'standby',
        lastError: 'passive relay hosting is disabled on this instance — enable the relay or create a pairing link to host here',
        connectedClients: 0,
      };
      return;
    }
    if (hostLock) {
      const claimed = claim === 'force' ? hostLock.forceClaim() : hostLock.tryClaim();
      if (!claimed) {
        status = standbyStatus(hostLock.liveClaimantPid());
        ensureClaimWatch(relayUrl);
        return;
      }
    }
    const identity = await identityRuntime.getRelayIdentity();
    hostClient = startRelayHost({
      relayUrl,
      identity,
      getLocalPort,
      logger,
      onStatus: (next) => {
        status = next;
      },
    });
    status = hostClient.getStatus();
    ensureClaimWatch(relayUrl);
  };

  // 完全停止：停监视器与 host 客户端、释放 host 锁、状态置 disabled。
  const stop = () => {
    stopClaimWatch();
    stopHostClient();
    if (hostLock) hostLock.release();
    status = { state: 'disabled', lastError: null, connectedClients: 0 };
  };

  // 启动入口（服务装配时调用）：已启用则拉起 host；失败只记日志。
  const startIfEnabled = async () => {
    try {
      const config = await readConfig();
      if (config.enabled) {
        await start(config.relayUrl);
      }
    } catch (error) {
      logger.warn(`[Relay] startup failed: ${error?.message ?? error}`);
    }
  };

  // 按需对账：有设备或待配对会话使用中继就开启并运行，否则落盘关闭并
  // 停止。启动与配对/设备变更后调用，运维无需手动开关。
  // Drive the relay lifecycle from demand: run it when a device or pending
  // session uses the relay, stop it when none remain. Called on startup and after
  // pairing/device changes, so the operator never toggles it manually.
  const reconcile = async () => {
    try {
      const demand = await hasRelayDemand();
      const config = await readConfig();
      if (demand) {
        if (!config.enabled) await writeConfig({ enabled: true, relayUrl: config.relayUrl });
        if (!hostClient) {
          const next = await readConfig();
          await start(next.relayUrl);
        }
      } else {
        if (config.enabled) await writeConfig({ enabled: false, relayUrl: config.relayUrl });
        stop();
      }
    } catch (error) {
      logger.warn(`[Relay] reconcile failed: ${error?.message ?? error}`);
    }
  };

  // 稳定的 server 身份（签名公钥规范 JWK 的 base64url SHA-256）。由公钥
  // 派生、并非机密；客户端在信任某个探测到的地址前用它核验归属。与
  // 中继是否启用无关。
  // Stable server identity (base64url SHA-256 of the canonical public signing
  // JWK). Derived from a public key, so it is not a secret; clients use it to
  // verify that a learned/probed address belongs to this server before trusting
  // it. Independent of whether the relay host is currently enabled.
  const getServerId = async () => {
    const identity = await identityRuntime.getRelayIdentity();
    return identity.serverId;
  };

  // 汇总状态：配置、身份、host 客户端实时状态（无客户端时回退 standby/
  // disabled），以及 relayUrl 是否被环境变量钉死。
  const getStatus = async () => {
    const config = await readConfig();
    const identity = await identityRuntime.getRelayIdentity();
    const live = hostClient ? hostClient.getStatus() : status;
    return {
      enabled: config.enabled,
      // Without a host client the service is either off or standing by while
      // another local process owns the machine's relay host claim.
      state: hostClient ? live.state : (status.state === 'standby' ? 'standby' : 'disabled'),
      serverId: identity.serverId,
      connectedClients: live.connectedClients,
      relayUrl: config.relayUrl,
      relayUrlLocked: config.relayUrlLocked,
      ...(live.lastError ? { lastError: live.lastError } : {}),
    };
  };

  // 统一连接负载（配对 v2）的中继候选。中继只是又一种传输：只携带中继
  // 路由与 E2EE 信任锚，不内嵌 token——客户端像其它候选一样在隧道内
  // 兑换一次性配对密钥。host 关闭时返回 null，保证只在真正可达时广播
  // 中继。优先级为高（LAN/隧道之后），作为最后兜底传输。
  // Pairing candidate for the unified connection payload (pairing v2). Relay is
  // just another transport: it carries the relay route + E2EE trust anchor, no
  // embedded token — the client redeems the one-time pairing secret over the
  // tunnel like any other candidate. Returns null when the host relay is off, so
  // callers only advertise relay when it is actually reachable. Priority is high
  // (tried after LAN/tunnel) since the relay path is the last-resort transport.
  const buildPairingCandidate = async () => {
    const config = await readConfig();
    const identity = await identityRuntime.getRelayIdentity();
    return {
      type: 'relay',
      relayUrl: config.relayUrl,
      serverId: identity.serverId,
      hostEncPubJwk: identity.hostEncPubJwk,
      priority: 30,
    };
  };

  // 对外配对候选入口：未启用时返回 null。
  const getPairingCandidate = async () => {
    const config = await readConfig();
    if (!config.enabled) return null;
    return buildPairingCandidate();
  };

  // 按需启用并返回配对候选。创建中继配对链接本身就是需求信号，因此
  // 这里直接开启中继而非要求另行手动开关。幂等：已启用且运行中则是
  // no-op。
  // Enable the relay host on demand and return its pairing candidate. Creating a
  // relay pairing link IS the demand signal, so the relay turns itself on here
  // rather than requiring a separate manual toggle. Idempotent: a no-op when the
  // relay is already enabled and running.
  const ensureEnabledForPairing = async () => {
    const config = await readConfig();
    if (!config.enabled) {
      await writeConfig({ enabled: true, relayUrl: config.relayUrl });
    }
    if (!hostClient) {
      const next = await readConfig();
      // Force-claim: creating a pairing link is explicit user intent — the
      // instance the user is pairing against MUST be the one devices reach,
      // even if another local process currently holds the machine's claim
      // (its claim watcher sees the takeover and stands down).
      await start(next.relayUrl, { claim: 'force' });
    }
    return buildPairingCandidate();
  };

  // 注册管理路由（挂在其它 OMPChamber 功能路由之后、通用 proxy 之前，
  // 受全局 UI 鉴权门保护）。
  const registerRoutes = (app) => {
    // GET 状态：配置 + 身份 + 实时连接情况。
    app.get('/api/ompchamber/relay/status', async (_req, res) => {
      try {
        res.json(await getStatus());
      } catch (error) {
        res.status(500).json({ error: error?.message ?? 'Failed to read relay status' });
      }
    });

    // POST 启用：可选携带 relayUrl；显式用户动作，强制抢锁后启动。
    app.post('/api/ompchamber/relay/enable', express.json({ limit: '16kb' }), async (req, res) => {
      try {
        const current = await readConfig();
        const relayUrl = typeof req.body?.relayUrl === 'string' ? normalizeRelayUrl(req.body.relayUrl) : current.relayUrl;
        await writeConfig({ enabled: true, relayUrl });
        if (hostClient) stop();
        // Explicit user action: take the machine's host claim like pairing does.
        await start(relayUrl, { claim: 'force' });
        res.json(await getStatus());
      } catch (error) {
        res.status(500).json({ error: error?.message ?? 'Failed to enable relay' });
      }
    });

    // POST 禁用：落盘关闭并停止 host（保留 relayUrl 设置）。
    app.post('/api/ompchamber/relay/disable', async (_req, res) => {
      try {
        const current = await readConfig();
        await writeConfig({ enabled: false, relayUrl: current.relayUrl });
        stop();
        res.json(await getStatus());
      } catch (error) {
        res.status(500).json({ error: error?.message ?? 'Failed to disable relay' });
      }
    });

  };

  // 对外句柄集合。
  return {
    registerRoutes,
    startIfEnabled,
    reconcile,
    stop,
    getStatus,
    getServerId,
    getPairingCandidate,
    ensureEnabledForPairing,
  };
};
