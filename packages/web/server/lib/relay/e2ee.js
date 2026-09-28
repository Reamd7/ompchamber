// E2EE primitives + responder handshake for the private relay (Layer 2).
// JS mirror of the normative TS implementation in
// packages/ui/src/lib/relay/{protocol,crypto,handshake}.ts — the web server is
// plain JS and cannot import from packages/ui, so the logic is copied verbatim
// (converted to JSDoc'd JS) and MUST stay byte-compatible with those modules.
// WebCrypto only: `globalThis.crypto.subtle` (Node >= 22).
// Spec: .opencode/plans/private-relay/01-protocol-spec.md (Layer 2).
/**
 * 私有中继的端到端加密（E2EE）原语与响应方（host 侧）握手状态机
 * （协议 Layer 2）。是 packages/ui/src/lib/relay/{protocol,crypto,handshake}.ts
 * 规范 TS 实现的 JS 镜像——web server 是纯 JS、无法直接 import UI 包，
 * 因此逐字拷贝并转为带 JSDoc 的 JS，必须与 TS 版保持字节级兼容。
 * 只使用 WebCrypto（`globalThis.crypto.subtle`，Node >= 22）。密钥体系：
 * ECDH P-256 协商共享秘密 -> HKDF-SHA256 派生双向 AES-256-GCM 会话密钥，
 * 每方向独立计数器 IV 防重放。所有密码学失败抛 RelayCryptoError。
 */

const subtle = globalThis.crypto.subtle;

/** 中继协议版本号；握手双方的 `v` 字段必须一致。 */
export const RELAY_PROTOCOL_VERSION = 1;
/** HKDF 派生会话密钥时的 info 参数（域分隔字符串）。 */
export const RELAY_HKDF_INFO = 'ompchamber-relay-v1';

// 加密帧布局：[1 字节 version][12 字节 IV][密文 + 16 字节 GCM tag]。
// Encrypted frame layout: [1 byte version][12 byte IV][ciphertext + 16 byte GCM tag].
export const ENCRYPTED_FRAME_VERSION = 1;
// 加密帧中 IV 的长度（12 字节 = 4 前缀 + 8 计数器）。
export const ENCRYPTED_FRAME_IV_BYTES = 12;
// 加密帧固定头部长度（version 1 字节 + IV 12 字节）。
export const ENCRYPTED_FRAME_HEADER_BYTES = 1 + ENCRYPTED_FRAME_IV_BYTES;
// 单帧明文上限：决定隧道帧 payload 预算的根约束（64 KiB）。
export const MAX_PLAINTEXT_FRAME_BYTES = 64 * 1024;

// 中继侧分配的 WebSocket 关闭码（host 只需其中两个）。
// Relay-assigned WebSocket close codes (subset the host needs).
export const RelayCloseCode = {
// 握手中途换钥（rekey）攻击：关闭码 1008。
  RekeyMismatch: 1008,
// 信道级失败（解密失败、就绪后收到明文等）：关闭码 1011。
  ChannelFailure: 1011,
};

/** ECDH 曲线参数：P-256，密钥可导出（deriveBits）。 */
const ECDH_PARAMS = { name: 'ECDH', namedCurve: 'P-256' };
/** 握手 nonce 的字节数（同时作为 HKDF 的 salt）。 */
const HANDSHAKE_NONCE_BYTES = 16;
/** 单方向会话密钥长度（字节）：AES-256。 */
const SESSION_KEY_BYTES = 32;
/** AES-GCM 认证标签长度（字节）。 */
const GCM_TAG_BYTES = 16;
// IV = 每方向 4 字节随机前缀 || 8 字节大端帧计数器。
// IV = 4-byte random per-direction prefix || 8-byte big-endian frame counter.
const IV_PREFIX_BYTES = 4;
// IV 中计数器部分的长度（大端 8 字节，配合 BigInt 可撑满 64 位）。
const IV_COUNTER_BYTES = 8;

/** 密码学操作失败（JWK 非法、帧过短、计数器回退、解密失败等）统一错误类型。 */
export class RelayCryptoError extends Error {
  constructor(message) {
    super(message);
    this.name = 'RelayCryptoError';
  }
}

// 生成一对 ECDH P-256 密钥（用于握手协商）。
/** @returns {Promise<CryptoKeyPair>} */
export const generateEcdhKeyPair = () => subtle.generateKey(ECDH_PARAMS, true, ['deriveBits']);

/**
 * 导出公钥 JWK，只保留定义椭圆曲线点的字段（kty/crv/x/y），
 * 使序列化形式稳定、可作为指纹输入。
 */
/**
 * @param {CryptoKey} key
 * @returns {Promise<JsonWebKey>} public JWK reduced to the fields that define the point
 */
export const exportPublicKeyJwk = async (key) => {
  const jwk = await subtle.exportKey('jwk', key);
  return { kty: jwk.kty, crv: jwk.crv, x: jwk.x, y: jwk.y };
};

// 导入对端公钥 JWK：校验 kty=EC、crv=P-256 及坐标字段，非法时抛
// RelayCryptoError（不透出底层 importKey 的错误细节）。
/** @param {JsonWebKey} jwk */
export const importEcdhPublicKey = async (jwk) => {
  if (jwk.kty !== 'EC' || jwk.crv !== 'P-256' || typeof jwk.x !== 'string' || typeof jwk.y !== 'string') {
    throw new RelayCryptoError('invalid ECDH public key JWK');
  }
  try {
    return await subtle.importKey(
      'jwk',
      { kty: jwk.kty, crv: jwk.crv, x: jwk.x, y: jwk.y, ext: true },
      ECDH_PARAMS,
      true,
      [],
    );
  } catch {
    throw new RelayCryptoError('invalid ECDH public key JWK');
  }
};

// 导入本方私钥 JWK（d + 点坐标）；不可导出（ext=false），仅用于 deriveBits。
/** @param {JsonWebKey} jwk private ECDH JWK (d + point) */
export const importEcdhPrivateKey = async (jwk) => {
  try {
    return await subtle.importKey('jwk', jwk, ECDH_PARAMS, false, ['deriveBits']);
  } catch {
    throw new RelayCryptoError('invalid ECDH private key JWK');
  }
};

// 公钥的稳定指纹（点字段的规范 JSON 串），用于在重复 hello 时检测换钥企图。
// 与上方英文注释指同一用途。
// Stable fingerprint of a public key, used to detect rekey attempts on re-hello.
/** @param {JsonWebKey} jwk */
export const publicKeyJwkFingerprint = (jwk) =>
  JSON.stringify({ crv: jwk.crv, kty: jwk.kty, x: jwk.x, y: jwk.y });

/** 生成 16 字节加密随机握手 nonce。 */
export const generateHandshakeNonce = () => {
  const nonce = new Uint8Array(HANDSHAKE_NONCE_BYTES);
  globalThis.crypto.getRandomValues(nonce);
  return nonce;
};

/**
 * 双向会话密钥派生：双方各持本方私钥与对端公钥调用，ECDH 得到相同
 * 共享秘密，再以握手 nonce 为 salt、RELAY_HKDF_INFO 为 info 做 HKDF，
 * 切成 clientToHost / hostToClient 两把 AES-256-GCM 密钥。nonce 长度
 * 非法时抛 RelayCryptoError。
 */
/**
 * Both sides call this with their own private key and the peer's public key;
 * ECDH yields the same shared secret, so the derived key pair matches.
 * @param {CryptoKey} ownPrivateKey
 * @param {CryptoKey} peerPublicKey
 * @param {Uint8Array} handshakeNonce
 * @returns {Promise<{ clientToHost: CryptoKey, hostToClient: CryptoKey }>}
 */
export const deriveSessionKeys = async (ownPrivateKey, peerPublicKey, handshakeNonce) => {
  if (handshakeNonce.length !== HANDSHAKE_NONCE_BYTES) {
    throw new RelayCryptoError('invalid handshake nonce length');
  }
  const sharedSecret = await subtle.deriveBits({ name: 'ECDH', public: peerPublicKey }, ownPrivateKey, 256);
  const hkdfKey = await subtle.importKey('raw', sharedSecret, 'HKDF', false, ['deriveBits']);
  const keyMaterial = new Uint8Array(
    await subtle.deriveBits(
      {
        name: 'HKDF',
        hash: 'SHA-256',
        salt: handshakeNonce,
        info: new TextEncoder().encode(RELAY_HKDF_INFO),
      },
      hkdfKey,
      SESSION_KEY_BYTES * 2 * 8,
    ),
  );
    // 把派生材料的一段字节导入为 AES-GCM 密钥（双向密钥各取 32 字节）。
  const importAesKey = (bytes, usage) => subtle.importKey('raw', bytes, { name: 'AES-GCM' }, false, usage);
  return {
    clientToHost: await importAesKey(keyMaterial.slice(0, SESSION_KEY_BYTES), ['encrypt', 'decrypt']),
    hostToClient: await importAesKey(keyMaterial.slice(SESSION_KEY_BYTES), ['encrypt', 'decrypt']),
  };
};

// 把 8 字节计数器以大端序写入 target 的 offset 起（作为 IV 的后 8 字节）。
const writeCounter = (target, offset, counter) => {
  for (let i = IV_COUNTER_BYTES - 1; i >= 0; i -= 1) {
    target[offset + i] = Number(counter & 0xffn);
    counter >>= 8n;
  }
};

// 从 source 的 offset 起按大端序读出 8 字节计数器（BigInt，防溢出）。
const readCounter = (source, offset) => {
  let value = 0n;
  for (let i = 0; i < IV_COUNTER_BYTES; i += 1) {
    value = (value << 8n) | BigInt(source[offset + i]);
  }
  return value;
};

/**
 * 创建单方向帧加密器：初始化时生成 4 字节随机 IV 前缀，之后每帧计数器
 * 自增并组出 `[version|IV|密文+tag]`。明文超过 64 KiB 上限时抛错。
 * 注意：计数器从 1 开始，1 也被占用，避免与全零 IV 撞车。
 */
/** @param {CryptoKey} key AES-256-GCM key for this direction */
export const createFrameEncryptor = (key) => {
  const ivPrefix = new Uint8Array(IV_PREFIX_BYTES);
  globalThis.crypto.getRandomValues(ivPrefix);
  let counter = 0n;
  return {
    /** @param {Uint8Array} plaintext */
    async encrypt(plaintext) {
      if (plaintext.length > MAX_PLAINTEXT_FRAME_BYTES) {
        throw new RelayCryptoError('plaintext frame exceeds maximum size');
      }
      counter += 1n;
      const iv = new Uint8Array(ENCRYPTED_FRAME_IV_BYTES);
      iv.set(ivPrefix, 0);
      writeCounter(iv, IV_PREFIX_BYTES, counter);
      const ciphertext = new Uint8Array(await subtle.encrypt({ name: 'AES-GCM', iv }, key, plaintext));
      const frame = new Uint8Array(ENCRYPTED_FRAME_HEADER_BYTES + ciphertext.length);
      frame[0] = ENCRYPTED_FRAME_VERSION;
      frame.set(iv, 1);
      frame.set(ciphertext, ENCRYPTED_FRAME_HEADER_BYTES);
      return frame;
    },
  };
};

// 强制本方向计数器严格递增：中继 WS 保序，任何回退或重放都意味着
// 篡改，必须立即失败关闭（fail closed）。
// Enforces strictly increasing per-direction counters: the relay WS preserves
// ordering, so any regression or replay means tampering and must fail closed.
/** @param {CryptoKey} key AES-256-GCM key for this direction */
export const createFrameDecryptor = (key) => {
  let lastCounter = 0n;
  return {
    /** @param {Uint8Array} frame */
    async decrypt(frame) {
      if (frame.length < ENCRYPTED_FRAME_HEADER_BYTES + GCM_TAG_BYTES) {
        throw new RelayCryptoError('encrypted frame too short');
      }
      if (frame[0] !== ENCRYPTED_FRAME_VERSION) {
        throw new RelayCryptoError('unsupported encrypted frame version');
      }
      const iv = frame.slice(1, ENCRYPTED_FRAME_HEADER_BYTES);
      const counter = readCounter(iv, IV_PREFIX_BYTES);
      if (counter <= lastCounter) {
        throw new RelayCryptoError('frame counter regression');
      }
      let plaintext;
      try {
        plaintext = await subtle.decrypt({ name: 'AES-GCM', iv }, key, frame.slice(ENCRYPTED_FRAME_HEADER_BYTES));
      } catch {
        throw new RelayCryptoError('frame decryption failed');
      }
      lastCounter = counter;
      return new Uint8Array(plaintext);
    },
  };
};

/** base64url 字母表（标准 base64 但以 `-`/`_` 替换 `+`/`/`，无填充）。 */
const BASE64URL_ALPHABET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_';

// 手写 base64url 编码（不依赖 Buffer，与 TS 镜像逐字节一致）。
/** @param {Uint8Array} bytes */
export const bytesToBase64Url = (bytes) => {
  let out = '';
  for (let i = 0; i < bytes.length; i += 3) {
    const b0 = bytes[i];
    const b1 = i + 1 < bytes.length ? bytes[i + 1] : undefined;
    const b2 = i + 2 < bytes.length ? bytes[i + 2] : undefined;
    out += BASE64URL_ALPHABET[b0 >> 2];
    out += BASE64URL_ALPHABET[((b0 & 0x03) << 4) | ((b1 ?? 0) >> 4)];
    if (b1 !== undefined) out += BASE64URL_ALPHABET[((b1 & 0x0f) << 2) | ((b2 ?? 0) >> 6)];
    if (b2 !== undefined) out += BASE64URL_ALPHABET[b2 & 0x3f];
  }
  return out;
};

// 手写 base64url 解码：字符集非法或长度 mod 4 == 1 时抛 RelayCryptoError。
/** @param {string} value */
export const base64UrlToBytes = (value) => {
  if (!/^[A-Za-z0-9_-]*$/.test(value) || value.length % 4 === 1) {
    throw new RelayCryptoError('invalid base64url input');
  }
  const out = new Uint8Array(Math.floor((value.length * 3) / 4));
  let outIndex = 0;
  let buffer = 0;
  let bits = 0;
  for (const char of value) {
    buffer = (buffer << 6) | BASE64URL_ALPHABET.indexOf(char);
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      out[outIndex] = (buffer >> bits) & 0xff;
      outIndex += 1;
    }
  }
  return out;
};

// ---------------------------------------------------------------------------
// Responder handshake state machine (host side). Mirror of createHostHandshake
// in packages/ui/src/lib/relay/handshake.ts.
//
// Fail-closed rules (from the spec):
// - a repeated identical `hello` re-sends `ready` (client retry race);
// - a `hello` with a DIFFERENT key on an established channel is a rekey
//   attack -> close 1008, never rekey in place;
// - plaintext after `ready`, or any decrypt failure -> close 1011.
// ---------------------------------------------------------------------------

// 解析并校验握手文本帧：仅接受版本匹配的 hello / ready；batch 能力位
// 缺失或未知一律按 false（legacy 行为）处理；不合法返回 null。
const parseHandshakeMessage = (raw) => {
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  if (typeof parsed !== 'object' || parsed === null) return null;
  if (parsed.v !== RELAY_PROTOCOL_VERSION) return null;
  // Unknown/missing capability flag = false = legacy behavior.
  const batch = parsed.batch === true;
  if (parsed.t === 'ready') {
    return { t: 'ready', v: RELAY_PROTOCOL_VERSION, batch };
  }
  if (parsed.t === 'hello' && typeof parsed.nonce === 'string' && typeof parsed.clientPubJwk === 'object' && parsed.clientPubJwk !== null) {
    return { t: 'hello', v: RELAY_PROTOCOL_VERSION, clientPubJwk: parsed.clientPubJwk, nonce: parsed.nonce, batch };
  }
  return null;
};

// 构造“失败关闭”动作：携带 1011 关闭码与原因，让调用方直接断开信道。
const failClosed = (reason) => ({
  type: 'fail',
  closeCode: RelayCloseCode.ChannelFailure,
  reason,
});

/**
 * Host（响应方）握手状态机。把每条入站文本帧交给 `handleText`，按返回
 * 动作驱动 socket：send-text（回发明文）/ established（先发 replyText，
 * 再切换到加密帧并使用返回的 channel）/ ignore（丢弃）/ fail（以
 * closeCode 关闭）。同一客户端重发相同 hello 会重发 ready（重试竞态），
 * 但换钥（不同公钥的 hello）触发 1008，就绪后收到明文触发 1011。
 */
/**
 * Host (responder) handshake. Feed every inbound text frame to `handleText`;
 * it returns one of:
 *   { type: 'send-text', text }                       — send this plaintext frame
 *   { type: 'established', channel, replyText }       — send replyText first, then switch to encrypted frames
 *   { type: 'ignore' }                                — drop the frame
 *   { type: 'fail', closeCode, reason }               — close the socket with closeCode
 * @param {CryptoKey} hostEncPrivateKey long-lived ECDH private key
 * @param {{ batch?: boolean }} [options] `batch` defaults true; set false to force legacy behavior
 */
export const createHostHandshake = (hostEncPrivateKey, options = {}) => {
  // 握手状态：是否已建立信道、已接受的客户端公钥指纹、缓存的 ready
  // 文本与是否协商出批量能力。
  const localBatch = options.batch !== false;
  let established = false;
  let acceptedClientKeyFingerprint = null;
  let readyText = null;
  let negotiatedBatch = false;
  // 对外句柄：established 只读标记 + handleText 状态机入口。
  return {
    get established() {
      return established;
    },
    /** @param {string} raw */
    async handleText(raw) {
      const message = parseHandshakeMessage(raw);
      if (message?.t !== 'hello') {
        if (established) {
          return failClosed('plaintext frame on established channel');
        }
        return { type: 'ignore' };
      }
      const fingerprint = publicKeyJwkFingerprint(message.clientPubJwk);
      if (acceptedClientKeyFingerprint !== null) {
        if (fingerprint === acceptedClientKeyFingerprint && readyText !== null) {
          // Client retried `hello` before our `ready` arrived — answer again.
          return { type: 'send-text', text: readyText };
        }
        return { type: 'fail', closeCode: RelayCloseCode.RekeyMismatch, reason: 'rekey mismatch' };
      }
      let clientPublicKey;
      let nonce;
      try {
        clientPublicKey = await importEcdhPublicKey(message.clientPubJwk);
        nonce = base64UrlToBytes(message.nonce);
      } catch {
        return failClosed('malformed hello');
      }
      let keys;
      try {
        keys = await deriveSessionKeys(hostEncPrivateKey, clientPublicKey, nonce);
      } catch {
        return failClosed('key derivation failed');
      }
      acceptedClientKeyFingerprint = fingerprint;
      // Batching runs only if both peers advertised it.
      negotiatedBatch = localBatch && message.batch === true;
      readyText = JSON.stringify(
        negotiatedBatch
          ? { t: 'ready', v: RELAY_PROTOCOL_VERSION, batch: true }
          : { t: 'ready', v: RELAY_PROTOCOL_VERSION },
      );
      established = true;
      return {
        type: 'established',
        batch: negotiatedBatch,
        replyText: readyText,
        channel: {
          encryptor: createFrameEncryptor(keys.hostToClient),
          decryptor: createFrameDecryptor(keys.clientToHost),
        },
      };
    },
  };
};
