// Host relay identity: the EXISTING ECDSA P-256 signing keypair (shared with
// the push relay via signing-key.js — same storage, same serverId) plus a NEW
// long-lived ECDH P-256 encryption keypair for the E2EE channel (WebCrypto
// keys are single-purpose, so signing and encryption keys must differ).
// The encryption keypair is persisted as `settings.relayEncryptionKey =
// { privateJwk, publicJwk }`, mirroring the relaySigningKey precedent.

/**
 * 【模块】host 侧 relay 身份：复用现有 ECDSA P-256 签名密钥对（与 push relay 经
 * signing-key.js 共享——同一存储、同一 serverId），并新增一把长期 ECDH P-256
 * 加密密钥对用于 E2EE 通道（WebCrypto 密钥单一用途，签名与加密密钥必须分开）。
 * 加密密钥对持久化为 `settings.relayEncryptionKey = { privateJwk, publicJwk }`，
 * 与 relaySigningKey 的先例保持一致。
 */
import {
  canonicalPublicJwkString,
  deriveServerId,
  getOrCreateRelaySigningKeypair,
  signRelayMessage,
} from './signing-key.js';
import { exportPublicKeyJwk, generateEcdhKeyPair, importEcdhPrivateKey } from './e2ee.js';

/** 判断值是否为 { privateJwk, publicJwk } 形态的 JWK 密钥对（宽松校验，仅检查两字段存在）。 */
const isJwkPair = (value) => Boolean(value && typeof value === 'object' && value.privateJwk && value.publicJwk);

/**
 * 创建 relay 身份运行时。返回 { getRelayIdentity }；身份在首次获取后进程内缓存，
 * 之后调用返回同一对象（含已导入的 CryptoKey，避免重复 I/O 与密钥导入）。
 */
/**
 * @param {{
 *   crypto: typeof import('node:crypto'),
 *   readSettingsFromDiskMigrated: () => Promise<object>,
 *   writeSettingsToDisk: (settings: object) => Promise<void>,
 *   readSettingsStrict?: () => Promise<object>,
 * }} deps
 */
export const createRelayIdentityRuntime = (deps) => {
  const { crypto, readSettingsFromDiskMigrated, writeSettingsToDisk, readSettingsStrict } = deps;

  // 进程内身份缓存：getRelayIdentity 首次成功后驻留，后续调用直接复用。
  let cachedIdentity = null;

  /**
   * 读取（或首次生成并持久化）relay E2EE 加密密钥对。存在即直接返回；
   * "不存在"的判定要过严格读取器（readSettingsStrict，损坏/不可读时抛错）复核，
   * 绝不因吞掉的读取失败而误生成新密钥——新加密密钥会使所有已配对设备
   * 固定的 E2EE 信任锚失效。预期仅在中继首次使用时生成一次。
   */
  const getOrCreateEncryptionKeypair = async () => {
    const settings = await readSettingsFromDiskMigrated();
    const existing = settings?.relayEncryptionKey;
    if (isJwkPair(existing)) {
      return existing;
    }
    // Same regeneration gate as the signing key: never mint a replacement
    // identity key off a swallowed read failure — a new encryption key breaks
    // the E2EE trust anchor pinned by every paired device. Verify "missing" via
    // the strict reader (throws on corrupt/unreadable) before generating.
    let verifiedSettings = settings;
    if (readSettingsStrict) {
      verifiedSettings = await readSettingsStrict();
      const verified = verifiedSettings?.relayEncryptionKey;
      if (isJwkPair(verified)) {
        return verified;
      }
    }
    // Loud on purpose: a new encryption key invalidates the E2EE trust anchor of
    // every paired device. Expected exactly once, on first relay use.
    console.warn('[relay-identity] Generating NEW relay encryption keypair (E2EE trust anchor changes; previously paired devices must re-pair)');
    const keyPair = await generateEcdhKeyPair();
    const privateJwk = await globalThis.crypto.subtle.exportKey('jwk', keyPair.privateKey);
    const publicJwk = await exportPublicKeyJwk(keyPair.publicKey);
    await writeSettingsToDisk({ ...settings, ...(verifiedSettings || {}), relayEncryptionKey: { privateJwk, publicJwk } });
    return { privateJwk, publicJwk };
  };

  /**
   * 获取（并缓存）完整 relay 身份：serverId、E2EE 加密公钥 JWK、已导入的
   * 加密私钥 CryptoKey，以及 signRelayAuth 签名函数。
   */
  /**
   * @returns {Promise<{
   *   serverId: string,
   *   hostEncPubJwk: JsonWebKey,
   *   hostEncPrivateKey: CryptoKey,
   *   signRelayAuth: (role: string, connectionId?: string | null) => { ts: number, sig: string, pk: string },
   * }>}
   */
  const getRelayIdentity = async () => {
    if (cachedIdentity) return cachedIdentity;
    const signing = await getOrCreateRelaySigningKeypair({ crypto, readSettingsFromDiskMigrated, writeSettingsToDisk, readSettingsStrict });
    const serverId = deriveServerId({ crypto }, signing.publicJwk);
    const encryption = await getOrCreateEncryptionKeypair();
    const hostEncPrivateKey = await importEcdhPrivateKey(encryption.privateJwk);
    const pk = Buffer.from(canonicalPublicJwkString(signing.publicJwk), 'utf8').toString('base64url');

  /**
   * relay 层鉴权签名（host-control / host-data 升级用）。签名载荷字符串为
   * `${ts}.${serverId}.${role}.${connectionId ?? ""}`（规范 Layer 1）。
   */
    // Relay-layer auth for host-control / host-data upgrades. Signature payload
    // string is `${ts}.${serverId}.${role}.${connectionId ?? ""}` (spec Layer 1).
    const signRelayAuth = (role, connectionId) => {
      const ts = Date.now();
      const sig = signRelayMessage({ crypto }, signing.privateKey, `${ts}.${serverId}.${role}.${connectionId ?? ''}`);
      return { ts, sig, pk };
    };

    cachedIdentity = {
      serverId,
      hostEncPubJwk: encryption.publicJwk,
      hostEncPrivateKey,
      signRelayAuth,
    };
    return cachedIdentity;
  };

  return { getRelayIdentity };
};
