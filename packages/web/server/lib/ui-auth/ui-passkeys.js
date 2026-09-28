/**
 * UI passkey（WebAuthn/FIDO2）认证支持：为启用密码保护的 OMPChamber Web UI
 * 提供 passkey 的注册、认证、列出与撤销能力。凭据持久化在本地 JSON 存储文件
 * 中，challenge 只保留在内存并在 TTL 后过期。基于 @simplewebauthn/server
 * 实现 WebAuthn 协议的服务端部分，Relying Party ID 按请求 Host 动态确定。
 */
import crypto from 'crypto';
import fs from 'fs';
import os from 'os';
import path from 'path';
import {
  generateAuthenticationOptions,
  generateRegistrationOptions,
  verifyAuthenticationResponse,
  verifyRegistrationResponse,
} from '@simplewebauthn/server';

/** passkey 存储文件的 schema 版本；当前只有 v1，预留给未来格式迁移。 */
const DEFAULT_STORE_VERSION = 1;
/** challenge 的默认有效期（5 分钟），超时后未完成的注册/认证请求作废。 */
const DEFAULT_CHALLENGE_TTL_MS = 5 * 60 * 1000;
/** 默认的 Relying Party 显示名称，展示在浏览器的 passkey 选择界面。 */
const DEFAULT_RP_NAME = 'OMPChamber';

/**
 * OMPChamber 的数据目录：优先读 OMPCHAMBER_DATA_DIR 环境变量（相对路径会被
 * 解析为绝对路径），否则回退到用户主目录下的 ~/.config/ompchamber。
 */
const OMPCHAMBER_DATA_DIR = process.env.OMPCHAMBER_DATA_DIR
  ? path.resolve(process.env.OMPCHAMBER_DATA_DIR)
  : path.join(os.homedir(), '.config', 'ompchamber');

/** passkey 存储文件的默认路径（数据目录下的 ui-passkeys.json）。 */
const PASSKEY_STORE_FILE = path.join(OMPCHAMBER_DATA_DIR, 'ui-passkeys.json');

/** 生成新的 WebAuthn userID：32 字节随机数的 base64url 编码，作为存储里的用户标识。 */
const createUserId = () => crypto.randomBytes(32).toString('base64url');

/**
 * 把存储中的 base64url userID 解码回 Uint8Array（@simplewebauthn 要求的格式）。
 * 输入不是非空字符串或解码失败时返回 null，由调用方决定如何报错。
 */
const decodeUserId = (value) => {
  if (typeof value !== 'string' || !value) {
    return null;
  }

  try {
    return Uint8Array.from(Buffer.from(value, 'base64url'));
  } catch {
    return null;
  }
};

/**
 * 归一化 passkey 的显示标签：去首尾空白、压缩连续空白为单个空格、截断到 120
 * 字符。结果为空或输入不是字符串时返回 fallback。
 */
const normalizeLabel = (value, fallback) => {
  if (typeof value !== 'string') {
    return fallback;
  }
  const normalized = value.trim().replace(/\s+/g, ' ');
  return normalized ? normalized.slice(0, 120) : fallback;
};

/**
 * 从 Host 头（或类似值）提取可作 WebAuthn rpID 的主机名：剥掉端口，处理
 * [IPv6]:port 的方括号写法，统一转小写。空输入返回空字符串。
 */
const normalizeHost = (value) => {
  if (typeof value !== 'string') {
    return '';
  }

  const trimmed = value.trim();
  if (!trimmed) {
    return '';
  }

  if (trimmed.startsWith('[')) {
    const end = trimmed.indexOf(']');
    return end >= 0 ? trimmed.slice(1, end).toLowerCase() : trimmed.toLowerCase();
  }

  const colonIndex = trimmed.indexOf(':');
  return (colonIndex >= 0 ? trimmed.slice(0, colonIndex) : trimmed).toLowerCase();
};

/** 判断 rpID 是否为本机回环地址（localhost / 127.0.0.1 / ::1），用于识别本地开发场景。 */
const isLocalRpId = (rpID) => rpID === 'localhost' || rpID === '127.0.0.1' || rpID === '::1';

/**
 * 推导当前请求的完整 origin（scheme://host）。优先信任反向代理注入的
 * x-forwarded-proto / x-forwarded-host（各取第一段），否则退回 socket 是否
 * 加密与 Host 头。无法确定 host 时返回空字符串。
 */
const getCurrentRequestOrigin = (req) => {
  const forwardedProto = typeof req.headers['x-forwarded-proto'] === 'string'
    ? req.headers['x-forwarded-proto'].split(',')[0].trim().toLowerCase()
    : '';
  const protocol = forwardedProto || (req.socket?.encrypted ? 'https' : 'http');
  const forwardedHost = typeof req.headers['x-forwarded-host'] === 'string'
    ? req.headers['x-forwarded-host'].split(',')[0].trim()
    : '';
  const host = forwardedHost || (typeof req.headers.host === 'string' ? req.headers.host.trim() : '');

  if (!host) {
    return '';
  }

  return `${protocol}://${host}`;
};

/**
 * 推导当前请求对应的 WebAuthn rpID：x-forwarded-host 优先于 Host 头，再退回
 * Express 的 req.hostname，最终经 normalizeHost 去端口、转小写。
 */
const getCurrentRpId = (req) => {
  const forwardedHost = typeof req.headers['x-forwarded-host'] === 'string'
    ? req.headers['x-forwarded-host'].split(',')[0].trim()
    : '';
  const host = forwardedHost || (typeof req.headers.host === 'string' ? req.headers.host.trim() : '');
  return normalizeHost(host || req.hostname || '');
};

/**
 * 校验并归一化存储文件里的一条 passkey 记录。字段缺失或类型不符时返回 null
 * （该条会被 loadStore 丢弃）；counter/transports 等可选字段给安全默认值，
 * label 缺省为 "Unnamed device"。防止手改的存储文件把非法数据带进验证流程。
 */
const parseStoredPasskey = (record) => {
  if (!record || typeof record !== 'object') {
    return null;
  }

  if (typeof record.id !== 'string' || typeof record.publicKey !== 'string' || typeof record.rpID !== 'string') {
    return null;
  }

  return {
    id: record.id,
    publicKey: record.publicKey,
    counter: typeof record.counter === 'number' && Number.isFinite(record.counter) ? record.counter : 0,
    transports: Array.isArray(record.transports)
      ? record.transports.filter((value) => typeof value === 'string')
      : [],
    deviceType: typeof record.deviceType === 'string' ? record.deviceType : 'singleDevice',
    backedUp: record.backedUp === true,
    createdAt: typeof record.createdAt === 'number' ? record.createdAt : Date.now(),
    lastUsedAt: typeof record.lastUsedAt === 'number' ? record.lastUsedAt : null,
    label: normalizeLabel(record.label, 'Unnamed device'),
    rpID: record.rpID,
  };
};

/**
 * 创建 UI passkey 服务实例（本模块的工厂入口），持有 challenge 的内存状态，
 * 并通过返回对象暴露全部操作。
 *
 * @param {object} [options] - 实例配置。
 * @param {string} options.passwordBinding - 当前 UI 密码保护的绑定标识；为空
 *   表示未启用密码保护，此时 passkey 功能整体禁用，已有凭据会被清空。
 * @param {Function} [options.readSettingsFromDiskMigrated] - 读取已迁移设置的
 *   异步函数，用于取 settings.publicOrigin 补充合法 origin 列表。
 * @param {string} [options.storeFile] - passkey 存储文件路径。
 * @param {string} [options.rpName] - Relying Party 显示名称。
 * @param {number} [options.challengeTtlMs] - challenge 有效期（毫秒）。
 * @returns 包含状态查询、注册、认证、撤销方法与 enabled 标志的服务对象。
 */
export const createUiPasskeys = ({
  passwordBinding,
  readSettingsFromDiskMigrated,
  storeFile = PASSKEY_STORE_FILE,
  rpName = DEFAULT_RP_NAME,
  challengeTtlMs = DEFAULT_CHALLENGE_TTL_MS,
} = {}) => {
  /** 进行中的注册 challenge：requestId -> 记录（challenge、预期 origin/rpID 等），仅存内存。 */
  const registrationChallenges = new Map();
  /** 进行中的认证 challenge：requestId -> 记录，仅存内存，进程重启后自然作废。 */
  const authenticationChallenges = new Map();

  /** 确保存储文件所在目录存在（递归创建），写入前调用。 */
  const ensureStoreDirectory = () => {
    fs.mkdirSync(path.dirname(storeFile), { recursive: true });
  };

  /** 把 store 以两空格缩进的 JSON 同步写入磁盘（先确保目录存在）。 */
  const persistStore = (store) => {
    ensureStoreDirectory();
    fs.writeFileSync(storeFile, JSON.stringify(store, null, 2));
  };

  /** 构造空的存储结构：全新 userID、无 passkey，绑定到当前 passwordBinding。 */
  const createEmptyStore = () => ({
    version: DEFAULT_STORE_VERSION,
    userID: createUserId(),
    passwordBinding,
    passkeys: [],
  });

  /**
   * 读取并校正 passkey 存储，永远返回一个可用 store：
   * - 文件不存在或解析失败时返回空 store（解析失败仅告警，不抛错）；
   * - 未启用密码保护（passwordBinding 为空）时清空并持久化已有 passkey；
   * - 存储绑定的 passwordBinding 与当前不一致（密码已改）时重置为空 store，
   *   防止旧密码时代的凭据继续有效；
   * - 文件尚不存在时把 store 落盘。
   */
  const loadStore = () => {
    let store = createEmptyStore();

    try {
      if (fs.existsSync(storeFile)) {
        const raw = fs.readFileSync(storeFile, 'utf8');
        const parsed = JSON.parse(raw);
        store = {
          version: DEFAULT_STORE_VERSION,
          userID: decodeUserId(parsed?.userID) ? parsed.userID : store.userID,
          passwordBinding: typeof parsed?.passwordBinding === 'string' ? parsed.passwordBinding : '',
          passkeys: Array.isArray(parsed?.passkeys) ? parsed.passkeys.map(parseStoredPasskey).filter(Boolean) : [],
        };
      }
    } catch (error) {
      console.warn('[UI Passkeys] Failed to read passkey store:', error?.message || error);
    }

    if (!passwordBinding) {
      if (store.passkeys.length > 0 || store.passwordBinding) {
        store = { ...store, passkeys: [], passwordBinding: '' };
        persistStore(store);
      }
      return store;
    }

    if (store.passwordBinding !== passwordBinding) {
        store = {
          version: DEFAULT_STORE_VERSION,
          userID: store.userID || createUserId(),
          passwordBinding,
          passkeys: [],
        };
      persistStore(store);
      return store;
    }

    if (!fs.existsSync(storeFile)) {
      persistStore(store);
    }

    return store;
  };

  /** 清理 challenge Map 中已过期（或记录损坏）的条目，在每次发起注册/认证前调用。 */
  const cleanupChallengeMap = (map) => {
    const now = Date.now();
    for (const [requestId, record] of map.entries()) {
      if (!record || now >= record.expiresAt) {
        map.delete(requestId);
      }
    }
  };

  /**
   * 汇总本次验证可接受的 origin 列表：当前请求推导出的 origin，加上设置里的
   * publicOrigin（通过公网域名访问时验证依然通过）。读取设置失败时静默跳过。
   * 返回去重后的数组，供 @simplewebauthn 的 expectedOrigin 使用。
   */
  const buildOriginCandidates = async (req) => {
    const origins = new Set();
    const currentOrigin = getCurrentRequestOrigin(req);
    if (currentOrigin) {
      origins.add(currentOrigin);
    }

    try {
      const settings = await readSettingsFromDiskMigrated?.();
      if (typeof settings?.publicOrigin === 'string' && settings.publicOrigin.trim().length > 0) {
        origins.add(new URL(settings.publicOrigin.trim()).origin);
      }
    } catch {
    }

    return Array.from(origins);
  };

  /** 入口守卫：断言密码保护已启用，否则抛 statusCode 400 的错误。 */
  const assertEnabled = () => {
    if (!passwordBinding) {
      const error = new Error('Passkeys require UI password protection to be enabled');
      error.statusCode = 400;
      throw error;
    }
  };

  /** 取出存储中属于指定 rpID（当前访问主机）的全部 passkey。 */
  const getPasskeysForRpId = (store, rpID) => store.passkeys.filter((passkey) => passkey.rpID === rpID);

  /**
   * 返回 passkey 功能状态：是否启用、当前主机是否已有 passkey、数量以及解析出
   * 的 rpID。未启用时不抛错，只报 enabled: false。
   */
  const getStatus = (req) => {
    const store = loadStore();
    const rpID = getCurrentRpId(req);
    return {
      enabled: Boolean(passwordBinding),
      hasPasskeys: Boolean(rpID) && getPasskeysForRpId(store, rpID).length > 0,
      passkeyCount: Boolean(rpID) ? getPasskeysForRpId(store, rpID).length : 0,
      rpID,
    };
  };

  /**
   * 列出当前主机的 passkey 元信息（id、label、创建/最近使用时间、设备类型、
   * 是否已备份），不含公钥等敏感字段。未启用时抛 400，主机无法解析时返回空数组。
   */
  const listPasskeys = (req) => {
    assertEnabled();

    const store = loadStore();
    const rpID = getCurrentRpId(req);
    if (!rpID) {
      return [];
    }

    return getPasskeysForRpId(store, rpID).map((passkey) => ({
      id: passkey.id,
      label: passkey.label,
      createdAt: passkey.createdAt,
      lastUsedAt: passkey.lastUsedAt,
      deviceType: passkey.deviceType,
      backedUp: passkey.backedUp,
    }));
  };

  /**
   * 按 ID 撤销当前主机上的一个 passkey：ID 为空抛 400；不存在或不属于当前主机
   * 抛 404。成功后持久化并返回剩余数量；其他主机上的同名凭据不受影响。
   */
  const revokePasskey = (req, passkeyId) => {
    assertEnabled();

    const normalizedPasskeyId = typeof passkeyId === 'string' ? passkeyId.trim() : '';
    if (!normalizedPasskeyId) {
      const error = new Error('Passkey ID is required');
      error.statusCode = 400;
      throw error;
    }

    const store = loadStore();
    const rpID = getCurrentRpId(req);
    const existingPasskey = store.passkeys.find((passkey) => passkey.id === normalizedPasskeyId && passkey.rpID === rpID);

    if (!existingPasskey) {
      const error = new Error('Passkey not found for this host');
      error.statusCode = 404;
      throw error;
    }

    const nextPasskeys = store.passkeys.filter((passkey) => !(passkey.id === normalizedPasskeyId && passkey.rpID === rpID));
    persistStore({
      ...store,
      passwordBinding,
      passkeys: nextPasskeys,
    });

    return {
      revoked: true,
      passkeyCount: nextPasskeys.filter((passkey) => passkey.rpID === rpID).length,
    };
  };

  /**
   * 清空全部 passkey 并轮换 userID（防止旧的 discoverable credential 残留绑定）。
   * 返回清理前的数量；未启用时抛 400。
   */
  const clearAllPasskeys = () => {
    assertEnabled();

    const store = loadStore();
    const clearedCount = store.passkeys.length;
    persistStore({
      ...store,
      userID: crypto.randomBytes(32).toString('base64url'),
      passwordBinding,
      passkeys: [],
    });

    return {
      cleared: true,
      clearedCount,
    };
  };

  /**
   * WebAuthn 注册第一步：为当前主机生成注册 options（要求 resident key 与用户
   * 验证，excludeCredentials 防止重复注册同一认证器），把 challenge 连同预期
   * origin/rpID/标签存入内存，返回 requestId + optionsJSON 给浏览器调
   * navigator.credentials.create。rpID 或 origin 无法解析抛 400，存储中的
   * userID 损坏抛 500。
   */
  const beginRegistration = async (req, { label } = {}) => {
    assertEnabled();
    cleanupChallengeMap(registrationChallenges);

    const rpID = getCurrentRpId(req);
    if (!rpID) {
      const error = new Error('Unable to resolve a valid passkey host for this request');
      error.statusCode = 400;
      throw error;
    }

    const currentOrigin = getCurrentRequestOrigin(req);
    if (!currentOrigin) {
      const error = new Error('Unable to resolve a valid passkey origin for this request');
      error.statusCode = 400;
      throw error;
    }

    const store = loadStore();
    const userID = decodeUserId(store.userID);
    if (!userID) {
      const error = new Error('Passkey storage is invalid. Please try again.');
      error.statusCode = 500;
      throw error;
    }

    const options = await generateRegistrationOptions({
      rpName,
      rpID,
      userID,
      userName: 'ompchamber-ui',
      userDisplayName: 'OMPChamber UI',
      attestationType: 'none',
      excludeCredentials: getPasskeysForRpId(store, rpID).map((passkey) => ({
        id: passkey.id,
        transports: passkey.transports,
      })),
      authenticatorSelection: {
        residentKey: 'required',
        userVerification: 'required',
      },
    });

    const requestId = crypto.randomBytes(16).toString('base64url');
    registrationChallenges.set(requestId, {
      challenge: options.challenge,
      expectedOrigins: await buildOriginCandidates(req),
      expectedRPIDs: [rpID],
      rpID,
      label: normalizeLabel(label, 'This device'),
      createdAt: Date.now(),
      expiresAt: Date.now() + challengeTtlMs,
    });

    return {
      requestId,
      optionsJSON: options,
    };
  };

  /**
   * WebAuthn 注册第二步：校验浏览器回传的注册响应（challenge、origin、rpID
   * 必须与第一步记录匹配，且要求用户验证）。requestId 无效或已过期抛 400，
   * 校验失败抛 400。通过后把公钥凭据以 base64url 写入存储；同 ID 的旧凭据被
   * 替换（相当于重新注册）。
   */
  const finishRegistration = async (payload) => {
    assertEnabled();
    cleanupChallengeMap(registrationChallenges);

    const store = loadStore();
    const requestId = typeof payload?.requestId === 'string' ? payload.requestId : '';
    const response = payload?.response;

    const matchingRecord = requestId ? registrationChallenges.get(requestId) : null;
    if (!matchingRecord) {
      const error = new Error('Passkey setup has expired. Please try again.');
      error.statusCode = 400;
      throw error;
    }

    registrationChallenges.delete(requestId);

    const verification = await verifyRegistrationResponse({
      response,
      expectedChallenge: matchingRecord.challenge,
      expectedOrigin: matchingRecord.expectedOrigins,
      expectedRPID: matchingRecord.expectedRPIDs,
      requireUserVerification: true,
    });

    if (!verification.verified || !verification.registrationInfo) {
      const error = new Error('Passkey registration could not be verified');
      error.statusCode = 400;
      throw error;
    }

    const {
      credential,
      credentialDeviceType,
      credentialBackedUp,
    } = verification.registrationInfo;

    const nextPasskeys = store.passkeys.filter((passkey) => passkey.id !== credential.id);
    nextPasskeys.push({
      id: credential.id,
      publicKey: Buffer.from(credential.publicKey).toString('base64url'),
      counter: credential.counter,
      transports: Array.isArray(credential.transports) ? credential.transports.filter((value) => typeof value === 'string') : [],
      deviceType: credentialDeviceType,
      backedUp: credentialBackedUp,
      createdAt: Date.now(),
      lastUsedAt: null,
      label: matchingRecord.label,
      rpID: matchingRecord.rpID,
    });

    persistStore({
      ...store,
      passwordBinding,
      passkeys: nextPasskeys,
    });

    return {
      verified: true,
      passkeyCount: nextPasskeys.filter((passkey) => passkey.rpID === matchingRecord.rpID).length,
    };
  };

  /**
   * WebAuthn 认证第一步：为当前主机生成认证 options，allowCredentials 只含已
   * 注册凭据。当前主机没有任何 passkey 时抛 404。challenge 按 TTL 存入内存。
   */
  const beginAuthentication = async (req) => {
    assertEnabled();
    cleanupChallengeMap(authenticationChallenges);

    const store = loadStore();
    const rpID = getCurrentRpId(req);
    const passkeys = getPasskeysForRpId(store, rpID);

    if (!rpID || passkeys.length === 0) {
      const error = new Error('No passkeys are registered for this host yet');
      error.statusCode = 404;
      throw error;
    }

    const options = await generateAuthenticationOptions({
      rpID,
      userVerification: 'required',
      allowCredentials: passkeys.map((passkey) => ({
        id: passkey.id,
        transports: passkey.transports,
      })),
    });

    const requestId = crypto.randomBytes(16).toString('base64url');
    authenticationChallenges.set(requestId, {
      challenge: options.challenge,
      expectedOrigins: await buildOriginCandidates(req),
      expectedRPIDs: [rpID],
      createdAt: Date.now(),
      expiresAt: Date.now() + challengeTtlMs,
    });

    return {
      requestId,
      optionsJSON: options,
    };
  };

  /**
   * WebAuthn 认证第二步：校验浏览器回传的断言（签名、challenge、origin、rpID、
   * 计数器回退检测、用户验证）。凭据不属于本实例抛 404，requestId 过期抛 400，
   * 校验失败抛 400。通过后更新计数器与 lastUsedAt 并持久化，返回 verified。
   */
  const finishAuthentication = async (payload) => {
    assertEnabled();
    cleanupChallengeMap(authenticationChallenges);

    const requestId = typeof payload?.requestId === 'string' ? payload.requestId : '';
    const response = payload?.response;
    const store = loadStore();
    const passkey = store.passkeys.find((item) => item.id === response?.id);

    if (!passkey) {
      const error = new Error('That passkey is not registered for this OMPChamber instance');
      error.statusCode = 404;
      throw error;
    }

    const matchingRecord = requestId ? authenticationChallenges.get(requestId) : null;
    if (!matchingRecord) {
      const error = new Error('Passkey sign-in has expired. Please try again.');
      error.statusCode = 400;
      throw error;
    }

    authenticationChallenges.delete(requestId);

    const verification = await verifyAuthenticationResponse({
      response,
      expectedChallenge: matchingRecord.challenge,
      expectedOrigin: matchingRecord.expectedOrigins,
      expectedRPID: matchingRecord.expectedRPIDs,
      credential: {
        id: passkey.id,
        publicKey: Buffer.from(passkey.publicKey, 'base64url'),
        counter: passkey.counter,
        transports: passkey.transports,
      },
      requireUserVerification: true,
    });

    if (!verification.verified || !verification.authenticationInfo) {
      const error = new Error('Passkey sign-in could not be verified');
      error.statusCode = 400;
      throw error;
    }

    const nextPasskeys = store.passkeys.map((item) => (
      item.id === passkey.id
        ? {
            ...item,
            counter: verification.authenticationInfo.newCounter,
            lastUsedAt: Date.now(),
          }
        : item
    ));

    persistStore({
      ...store,
      passwordBinding,
      passkeys: nextPasskeys,
    });

    return { verified: true };
  };

  /** 释放实例：清空内存中的全部 pending challenge（测试与关停时使用）。 */
  const dispose = () => {
    registrationChallenges.clear();
    authenticationChallenges.clear();
  };

  return {
    enabled: Boolean(passwordBinding),
    getStatus,
    listPasskeys,
    revokePasskey,
    clearAllPasskeys,
    beginRegistration,
    finishRegistration,
    beginAuthentication,
    finishAuthentication,
    dispose,
    isLocalRpId,
  };
};
