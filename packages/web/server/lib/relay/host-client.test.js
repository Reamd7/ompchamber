/**
 * 集成测试套件：假中继（最小 Layer 1）+ 真实 host-client + 一个用 JS
 * e2ee 发起方脚本化的客户端。验证完整握手、经隧道转发的 HTTP GET
 * /health（含 origin 改写与连接标记透传），并断言握手之后经过中继的
 * 帧全部是二进制（明文只允许 hello/ready 各一条）。
 */
// Integration test: fake relay (minimal Layer 1) + real host-client + a scripted
// client using the JS e2ee initiator. Verifies the full handshake and a tunneled
// HTTP GET /health, and asserts only binary frames cross the relay post-handshake.

import { afterAll, beforeAll, describe, expect, it } from 'bun:test';
import http from 'node:http';
import crypto from 'node:crypto';
import { WebSocket, WebSocketServer } from 'ws';

import { startRelayHost } from './host-client.js';
import {
  bytesToBase64Url,
  createFrameDecryptor,
  createFrameEncryptor,
  deriveSessionKeys,
  exportPublicKeyJwk,
  generateEcdhKeyPair,
  generateHandshakeNonce,
  importEcdhPrivateKey,
  RELAY_PROTOCOL_VERSION,
} from './e2ee.js';
import {
  TunnelFrameType,
  decodeTunnelFrame,
  encodeJsonPayload,
  encodeTunnelFrame,
} from './tunnel-codec.js';

// ---------------------------------------------------------------------------
// Fake relay: routes host-control <-> host-data <-> client by (serverId, connectionId).
// Forwards frames verbatim, never inspects them.
// ---------------------------------------------------------------------------
// 假中继：按 (serverId, connectionId) 在 host-control / host-data / client
// 三类 socket 间路由，帧原样转发、从不查看内容。
// 中文补充：state.relayFrames 记录每条转发帧的来源与是否二进制，供
// 后文的“仅二进制帧过中继”断言使用。
const startFakeRelay = () => {
  const server = http.createServer();
  const wss = new WebSocketServer({ server });
  // 假中继的核心状态：控制腿、按 connectionId 索引的数据腿/客户端腿、
  // 早于 host-data 到达的客户端帧缓冲与转发帧观测记录。
  const state = {
    control: null,
    hostData: new Map(), // connectionId -> ws
    clients: new Map(), // connectionId -> ws
    buffered: new Map(), // connectionId -> [[data, isBinary]] awaiting host-data
    relayFrames: [], // observed forwarded frames (for plaintext assertions)
  };

  wss.on('connection', (ws, req) => {
    const url = new URL(req.url, 'http://localhost');
    const role = url.searchParams.get('role');
    const connectionId = url.searchParams.get('connectionId');

    if (role === 'host-control') {
      state.control = ws;
      // Announce any already-waiting clients.
      ws.send(JSON.stringify({ type: 'sync', connectionIds: [...state.clients.keys()] }));
      for (const id of state.clients.keys()) {
        ws.send(JSON.stringify({ type: 'connected', connectionId: id }));
      }
      return;
    }

    if (role === 'host-data') {
      state.hostData.set(connectionId, ws);
      // Flush any client frames that arrived before this socket attached.
      const buffered = state.buffered.get(connectionId) || [];
      state.buffered.delete(connectionId);
      for (const [data, isBinary] of buffered) ws.send(data, { binary: isBinary });
      ws.on('message', (data, isBinary) => {
        state.relayFrames.push({ from: 'host', isBinary });
        const client = state.clients.get(connectionId);
        if (client && client.readyState === WebSocket.OPEN) client.send(data, { binary: isBinary });
      });
      ws.on('close', () => state.hostData.delete(connectionId));
      return;
    }

    if (role === 'client') {
      state.clients.set(connectionId, ws);
      ws.on('message', (data, isBinary) => {
        state.relayFrames.push({ from: 'client', isBinary });
        const host = state.hostData.get(connectionId);
        if (host && host.readyState === WebSocket.OPEN) {
          host.send(data, { binary: isBinary });
        } else {
          const queue = state.buffered.get(connectionId) || [];
          queue.push([data, isBinary]);
          state.buffered.set(connectionId, queue);
        }
      });
      ws.on('close', () => state.clients.delete(connectionId));
      if (state.control && state.control.readyState === WebSocket.OPEN) {
        state.control.send(JSON.stringify({ type: 'connected', connectionId }));
      }
    }
  });

  // 监听随机端口后返回 wsUrl、state 与 stop()。
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => {
      const port = server.address().port;
      resolve({
        wsUrl: `ws://127.0.0.1:${port}`,
        state,
        stop: () => new Promise((r) => {
          wss.close();
          server.close(() => r());
        }),
      });
    });
  });
};

// 一个只服务 /health 的桩回环源站：校验 origin 必须是自身的回环地址，
// 并回显中继连接标记，用于验证 host 侧的头部改写。
// A stub loopback origin serving /health.
const startLoopbackOrigin = () =>
  new Promise((resolve) => {
    const server = http.createServer((req, res) => {
      if (req.url === '/health') {
        const expectedOrigin = `http://127.0.0.1:${server.address().port}`;
        if (req.headers.origin !== expectedOrigin) {
          res.writeHead(403, { 'content-type': 'application/json' });
          res.end(JSON.stringify({ error: 'Invalid origin' }));
          return;
        }
        res.writeHead(200, { 'content-type': 'application/json' });
        res.end(JSON.stringify({
          ok: true,
          service: 'stub',
          relayConn: req.headers['x-ompchamber-relay-connection'] || null,
          origin: req.headers.origin,
        }));
        return;
      }
      res.writeHead(404);
      res.end();
    });
    server.listen(0, '127.0.0.1', () => resolve({ port: server.address().port, stop: () => new Promise((r) => server.close(() => r())) }));
  });

// 用全新密钥对构造 host 身份（ECDH 加密密钥 + ECDSA 签名密钥），
// serverId 为签名公钥规范 JWK 的 base64url SHA-256。
// Build the host identity around a fresh keypair (ECDH enc key + ECDSA sign key).
const buildIdentity = async () => {
  const enc = await generateEcdhKeyPair();
  const encPrivJwk = await globalThis.crypto.subtle.exportKey('jwk', enc.privateKey);
  const { privateKey: signPriv, publicKey: signPub } = crypto.generateKeyPairSync('ec', { namedCurve: 'P-256' });
  const signPubJwk = signPub.export({ format: 'jwk' });
  const canonical = JSON.stringify({ crv: signPubJwk.crv, kty: signPubJwk.kty, x: signPubJwk.x, y: signPubJwk.y });
  const serverId = crypto.createHash('sha256').update(canonical).digest('base64url');
  return {
    serverId,
    hostEncPubJwk: await exportPublicKeyJwk(enc.publicKey),
    hostEncPrivateKey: await importEcdhPrivateKey(encPrivJwk),
    signRelayAuth: (role, connectionId) => {
      const ts = Date.now();
      const sig = crypto
        .sign('SHA256', Buffer.from(`${ts}.${serverId}.${role}.${connectionId ?? ''}`), { key: signPriv, dsaEncoding: 'ieee-p1363' })
        .toString('base64url');
      return { ts, sig, pk: Buffer.from(canonical, 'utf8').toString('base64url') };
    },
  };
};

// 使用 JS 发起方实现的脚本化客户端：连接中继、完成握手、发出一次
// GET /health 并在 StreamEnd 时归并响应体返回结果。
// Scripted client using the JS initiator: connects, handshakes, does a GET.
const runScriptedClient = async ({ relayUrl, serverId, hostEncPubJwk }) => {
  const connectionId = 'conn-test-1';
  const url = new URL(`${relayUrl}/`);
  url.searchParams.set('v', String(RELAY_PROTOCOL_VERSION));
  url.searchParams.set('role', 'client');
  url.searchParams.set('serverId', serverId);
  url.searchParams.set('connectionId', connectionId);
  const ws = new WebSocket(url.toString());

  const hostPub = await globalThis.crypto.subtle.importKey(
    'jwk',
    { kty: hostEncPubJwk.kty, crv: hostEncPubJwk.crv, x: hostEncPubJwk.x, y: hostEncPubJwk.y, ext: true },
    { name: 'ECDH', namedCurve: 'P-256' },
    true,
    [],
  );
  const ephemeral = await generateEcdhKeyPair();
  const nonce = generateHandshakeNonce();

  // 客户端侧信道与响应收集：握手成功前 channel 为 null。
  let channel = null;
  const responseChunks = [];
  let responseStatus = null;
  let resolveDone;
  const done = new Promise((resolve) => {
    resolveDone = resolve;
  });

  ws.on('open', async () => {
    ws.send(JSON.stringify({
      t: 'hello',
      v: RELAY_PROTOCOL_VERSION,
      clientPubJwk: await exportPublicKeyJwk(ephemeral.publicKey),
      nonce: bytesToBase64Url(nonce),
    }));
  });

// 串行化消息处理：ws 的 async 处理器会让多条消息并发执行，导致
// StreamEnd 反超 HttpBody、触发解密器的严格计数器校验（生产端隧道
// 客户端同样是链式解密）。
  // Serialize message handling: an async ws handler runs per-message tasks
  // concurrently, letting StreamEnd overtake HttpBody and trip the decryptor's
  // strict counter ordering (the production tunnel client chains decrypts).
  let processing = Promise.resolve();
  // 单条消息处理：ready -> 派生双向密钥并发起 HTTP GET；二进制帧解密
  // 后按帧型收集响应，StreamEnd 时拼接 body 并完成 Promise。
  const handleMessage = async (data, isBinary) => {
    if (!isBinary) {
      const msg = JSON.parse(data.toString('utf8'));
      if (msg.t === 'ready') {
        const keys = await deriveSessionKeys(ephemeral.privateKey, hostPub, nonce);
        channel = {
          encryptor: createFrameEncryptor(keys.clientToHost),
          decryptor: createFrameDecryptor(keys.hostToClient),
        };
        // Send an HTTP GET /health over stream 1.
        const req = encodeTunnelFrame(TunnelFrameType.HttpRequest, 1, encodeJsonPayload({
          method: 'GET',
          path: '/health',
          query: '',
          headers: { accept: 'application/json' },
        }));
        ws.send(await channel.encryptor.encrypt(req), { binary: true });
        ws.send(await channel.encryptor.encrypt(encodeTunnelFrame(TunnelFrameType.StreamEnd, 1, new Uint8Array(0))), { binary: true });
      }
      return;
    }
    if (!channel) return;
    const plaintext = await channel.decryptor.decrypt(new Uint8Array(data));
    const frame = decodeTunnelFrame(plaintext);
    if (frame.frameType === TunnelFrameType.HttpResponse) {
      responseStatus = JSON.parse(new TextDecoder().decode(frame.payload)).status;
    } else if (frame.frameType === TunnelFrameType.HttpBody) {
      responseChunks.push(frame.payload);
    } else if (frame.frameType === TunnelFrameType.StreamEnd) {
      const total = responseChunks.reduce((n, c) => n + c.length, 0);
      const body = new Uint8Array(total);
      let off = 0;
      for (const c of responseChunks) {
        body.set(c, off);
        off += c.length;
      }
      resolveDone({ status: responseStatus, body: JSON.parse(new TextDecoder().decode(body)) });
      ws.close();
    }
  };
  ws.on('message', (data, isBinary) => {
    processing = processing.then(() => handleMessage(data, isBinary));
  });

  return done;
};

// 主 describe：端到端验证 host-client 的注册、握手与隧道 HTTP 转发。
describe('relay host-client integration', () => {
  let relay;
  let origin;
  let host;

  beforeAll(async () => {
    relay = await startFakeRelay();
    origin = await startLoopbackOrigin();
  });

  afterAll(async () => {
    host?.stop();
    await relay?.stop();
    await origin?.stop();
  });

  it('tunnels an HTTP GET /health with only binary frames post-handshake', async () => {
    const identity = await buildIdentity();
    host = startRelayHost({
      relayUrl: `${relay.wsUrl}/`,
      identity,
      getLocalPort: () => origin.port,
      onStatus: () => {},
      logger: { warn: () => {} },
    });

    // Give the control socket a moment to connect before the client arrives.
    await new Promise((r) => setTimeout(r, 200));

    const result = await runScriptedClient({
      relayUrl: relay.wsUrl,
      serverId: identity.serverId,
      hostEncPubJwk: identity.hostEncPubJwk,
    });

    expect(result.status).toBe(200);
    expect(result.body.ok).toBe(true);
    expect(result.body.relayConn).toBe('conn-test-1');
    expect(result.body.origin).toBe(`http://127.0.0.1:${origin.port}`);

    // Every forwarded frame after the two plaintext handshake frames (client
    // hello, host ready) must be binary.
    const forwarded = relay.state.relayFrames;
    const plaintextForwarded = forwarded.filter((f) => !f.isBinary);
    expect(plaintextForwarded.length).toBe(2); // hello + ready only
    expect(forwarded.filter((f) => f.isBinary).length).toBeGreaterThan(0);
  });
});
