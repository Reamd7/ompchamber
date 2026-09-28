// Per-server relay signing identity (ECDSA P-256), extracted from
// lib/notifications/apns-runtime.js so both the push relay and the private
// relay share the SAME keypair and thus the SAME serverId
// (base64url(SHA-256(canonical public JWK))). Storage format is unchanged:
// `settings.relaySigningKey = { privateJwk, publicJwk }` — existing installs'
// serverId must stay stable because push token binding depends on it.

/**
 * 【模块】每服务器 relay 签名身份（ECDSA P-256）。从 lib/notifications/apns-runtime.js
 * 抽出，使 push relay 与 private relay 共享同一密钥对、因而同一 serverId
 * （base64url(SHA-256(canonical public JWK))）。存储格式不变：
 * `settings.relaySigningKey = { privateJwk, publicJwk }` —— 已有安装的 serverId
 * 必须保持稳定，因为 push token 绑定依赖它。
 */
/**
 * @param {{
 *   crypto: typeof import('node:crypto'),
 *   readSettingsFromDiskMigrated: () => Promise<object>,
 *   writeSettingsToDisk: (settings: object) => Promise<void>,
 *   readSettingsStrict?: () => Promise<object>,
 * }} deps
 * @returns {Promise<{ privateKey: import('node:crypto').KeyObject, publicJwk: JsonWebKey }>}
 */
/**
 * 读取（或首次生成并持久化）relay 签名密钥对。已存在则包装为 KeyObject 返回；
 * "不存在"须经严格读取器（readSettingsStrict，损坏/不可读时抛错）复核后才生成新钥——
 * 宽松读取器把读取失败映射为 `{}`，与首次运行不可区分，而新密钥意味着新 serverId，
 * 会孤儿化所有已配对设备与 push 绑定，且后续写入会用空展开清掉整个 settings 文件。
 */
export const getOrCreateRelaySigningKeypair = async ({ crypto, readSettingsFromDiskMigrated, writeSettingsToDisk, readSettingsStrict }) => {
  /** 将存储的 { privateJwk, publicJwk } 还原为 { privateKey: KeyObject, publicJwk }。 */
  const toKeypair = (stored) => ({
    privateKey: crypto.createPrivateKey({ key: stored.privateJwk, format: 'jwk' }),
    publicJwk: stored.publicJwk,
  });
  const settings = await readSettingsFromDiskMigrated();
  const existing = settings?.relaySigningKey;
  if (existing && existing.privateJwk && existing.publicJwk) {
    return toKeypair(existing);
  }
  // Regeneration gate: the lenient settings reader maps read failures to `{}`,
  // indistinguishable from "first run". Minting a new keypair changes serverId,
  // which orphans every paired device and push binding AND the write below would
  // clobber the settings file with the empty spread. Re-verify with the strict
  // reader (throws on corrupt/unreadable) before generating; if it finds the
  // key the lenient read lost, use it and generate nothing.
  let verifiedSettings = settings;
  if (readSettingsStrict) {
    verifiedSettings = await readSettingsStrict();
    const verified = verifiedSettings?.relaySigningKey;
    if (verified && verified.privateJwk && verified.publicJwk) {
      return toKeypair(verified);
    }
  }
  // Loud on purpose: a new signing key means a new serverId — every previously
  // paired device and push binding is orphaned. Expected exactly once, on first run.
  console.warn('[relay-identity] Generating NEW relay signing keypair (serverId changes; previously paired devices must re-pair)');
  const { privateKey, publicKey } = crypto.generateKeyPairSync('ec', { namedCurve: 'P-256' });
  const privateJwk = privateKey.export({ format: 'jwk' });
  const publicJwk = publicKey.export({ format: 'jwk' });
  await writeSettingsToDisk({ ...settings, ...(verifiedSettings || {}), relaySigningKey: { privateJwk, publicJwk } });
  return { privateKey, publicJwk };
};

/** 固定字段顺序（crv/kty/x/y）序列化公钥 JWK，使哈希不受存储 JSON 字段顺序影响；与 ompchamber-website apps/api relay-auth.ts 的 canonicalJwk 逐字节一致。 */
// Fixed key order so the hash is stable regardless of stored JSON field order.
// Byte-for-byte mirror of canonicalJwk in ompchamber-website apps/api relay-auth.ts.
/** @param {JsonWebKey} jwk */
export const canonicalPublicJwkString = (jwk) =>
  JSON.stringify({ crv: jwk.crv, kty: jwk.kty, x: jwk.x, y: jwk.y });

/** 由公钥 JWK 派生 serverId：base64url(SHA-256(canonical 公钥 JWK))—— 两个 relay 共用的路由键。 */
/**
 * serverId = base64url(SHA-256(canonical public JWK)). Must match the push
 * relay's deriveServerId — this id is the routing key for both relays.
 * @param {{ crypto: typeof import('node:crypto') }} deps
 * @param {JsonWebKey} publicJwk
 */
export const deriveServerId = ({ crypto }, publicJwk) =>
  crypto.createHash('sha256').update(canonicalPublicJwkString(publicJwk)).digest('base64url');

/** 用 relay 签名私钥对消息做 ECDSA-SHA256 签名，输出 base64url（IEEE P1363 裸 r||s，WebCrypto 可验证的形态）。 */
/**
 * ECDSA-SHA256, IEEE P1363 (raw r||s) signature — the form WebCrypto verifies.
 * @param {{ crypto: typeof import('node:crypto') }} deps
 * @param {import('node:crypto').KeyObject} privateKey
 * @param {string} message
 */
export const signRelayMessage = ({ crypto }, privateKey, message) =>
  crypto.sign('SHA256', Buffer.from(message), { key: privateKey, dsaEncoding: 'ieee-p1363' }).toString('base64url');
