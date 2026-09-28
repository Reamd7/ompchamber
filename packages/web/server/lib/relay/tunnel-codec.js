// Tunnel mux frame codec (Layer 3 of the protocol spec). Pure functions, no I/O.
// JS mirror of packages/ui/src/lib/relay/tunnel-codec.ts (+ the Layer 3
// constants from protocol.ts) — MUST stay byte-compatible with those modules.
// Frame layout: [1 byte frameType (high bit = fragment-continues)][4 byte BE streamId][payload].
// Client-initiated streams use odd streamIds starting at 1; even ids are reserved.
// Spec: .opencode/plans/private-relay/01-protocol-spec.md (Layer 3).
/**
 * 隧道多路复用帧编解码（协议 Layer 3）。全部为纯函数、无 I/O；是
 * packages/ui/src/lib/relay/tunnel-codec.ts 的 JS 镜像（外加 protocol.ts
 * 的 Layer 3 常量），必须与 TS 版保持字节级兼容。帧布局：
 * [1 字节 frameType（最高位 = 分片继续标记)][4 字节大端 streamId][payload]；
 * 客户端发起的流使用从 1 开始的奇数 streamId，偶数 id 保留。所有解码
 * 错误统一抛 TunnelCodecError，由调用方决定关闭连接还是丢弃该帧。
 */

import { MAX_PLAINTEXT_FRAME_BYTES } from './e2ee.js';


/** 隧道帧固定头部长度：1 字节类型 + 4 字节 streamId。 */
export const TUNNEL_FRAME_HEADER_BYTES = 5;
/** frameType 首字节的最高位标记：置 1 表示该帧之后还有分片。 */
export const TUNNEL_FRAGMENT_FLAG = 0x80;

// 批量封装容器（protocol.ts 的镜像）：仅在双方协商出 `batch` 能力时使用。
// 从单帧 payload 预算中预留封装开销，保证任意单帧仍能塞进 64 KiB 的
// 加密明文。
// Batch envelope container (mirror of protocol.ts). Only used when both peers
// negotiated `batch`. Reserve the per-frame envelope overhead from the payload
// budget so any single frame still fits one 64 KiB encrypted plaintext.
/** 容器 tag：明文只携带单个隧道帧。 */
export const BATCH_CONTAINER_TAG_SINGLE = 0x00;
/** 容器 tag：明文携带一批以长度前缀分隔的隧道帧。 */
export const BATCH_CONTAINER_TAG_BATCH = 0x01;
/** 批量封装中每个帧的 4 字节大端长度前缀。 */
export const BATCH_FRAME_LENGTH_BYTES = 4;
/** 单帧走批量封装时的信封开销（1 字节 tag + 4 字节长度）。 */
export const BATCH_ENVELOPE_RESERVED_BYTES = 1 + BATCH_FRAME_LENGTH_BYTES;
/** 单个隧道帧 payload 的上限（64 KiB 明文减去帧头与批量信封预留）。 */
export const MAX_TUNNEL_PAYLOAD_BYTES =
  MAX_PLAINTEXT_FRAME_BYTES - TUNNEL_FRAME_HEADER_BYTES - BATCH_ENVELOPE_RESERVED_BYTES;

/**
 * 隧道帧类型枚举（协议 Layer 3 的帧种类）。1-5 为 HTTP 流帧，6-10 为
 * WebSocket 流帧，11-12 为 keepalive；HttpResponse 与 WsOpened 只由
 * host 发出。
 */
export const TunnelFrameType = {
  HttpRequest: 1,
  HttpBody: 2,
  HttpResponse: 3,
  StreamEnd: 4,
  StreamAbort: 5,
  WsOpen: 6,
  WsOpened: 7,
  WsText: 8,
  WsBinary: 9,
  WsClose: 10,
  Ping: 11,
  Pong: 12,
};

/** 合法的 frameType 数值集合，用于解码校验。 */
const TUNNEL_FRAME_TYPE_VALUES = new Set(Object.values(TunnelFrameType));

// 判断数值是否为已知的隧道帧类型（解码时拒绝未知类型）。
/** @param {number} value */
export const isTunnelFrameType = (value) => TUNNEL_FRAME_TYPE_VALUES.has(value);

/** streamId 的上限（32 位无符号），encodeTunnelFrame 据此做范围校验。 */
const MAX_STREAM_ID = 0xffffffff;

/** 隧道编解码错误：帧过短、streamId 非法、payload 超限、JSON 畸形等。 */
export class TunnelCodecError extends Error {
  constructor(message) {
    super(message);
    this.name = 'TunnelCodecError';
  }
}

/**
 * 编码单个隧道帧：写入类型字节（含分片标记）与大端 streamId，再拼接
 * payload。streamId 越界或 payload 超过 MAX_TUNNEL_PAYLOAD_BYTES 时抛
 * TunnelCodecError。
 */
/**
 * @param {number} frameType
 * @param {number} streamId
 * @param {Uint8Array} payload
 * @param {boolean} [hasMoreFragments]
 */
export const encodeTunnelFrame = (frameType, streamId, payload, hasMoreFragments = false) => {
  if (!Number.isInteger(streamId) || streamId < 0 || streamId > MAX_STREAM_ID) {
    throw new TunnelCodecError('invalid stream id');
  }
  if (payload.length > MAX_TUNNEL_PAYLOAD_BYTES) {
    throw new TunnelCodecError('tunnel payload exceeds maximum size');
  }
  const frame = new Uint8Array(TUNNEL_FRAME_HEADER_BYTES + payload.length);
  frame[0] = hasMoreFragments ? frameType | TUNNEL_FRAGMENT_FLAG : frameType;
  frame[1] = (streamId >>> 24) & 0xff;
  frame[2] = (streamId >>> 16) & 0xff;
  frame[3] = (streamId >>> 8) & 0xff;
  frame[4] = streamId & 0xff;
  frame.set(payload, TUNNEL_FRAME_HEADER_BYTES);
  return frame;
};

/**
 * 解码单个隧道帧：校验最短长度与已知 frameType，剥离分片标记位，还原
 * 大端 streamId；payload 为原帧的切片副本。帧过短或类型未知时抛
 * TunnelCodecError。
 */
/**
 * @param {Uint8Array} frame
 * @returns {{ frameType: number, streamId: number, payload: Uint8Array, hasMoreFragments: boolean }}
 */
export const decodeTunnelFrame = (frame) => {
  if (frame.length < TUNNEL_FRAME_HEADER_BYTES) {
    throw new TunnelCodecError('tunnel frame too short');
  }
  const rawType = frame[0];
  const hasMoreFragments = (rawType & TUNNEL_FRAGMENT_FLAG) !== 0;
  const frameType = rawType & ~TUNNEL_FRAGMENT_FLAG;
  if (!isTunnelFrameType(frameType)) {
    throw new TunnelCodecError(`unknown tunnel frame type ${frameType}`);
  }
  const streamId = ((frame[1] << 24) | (frame[2] << 16) | (frame[3] << 8) | frame[4]) >>> 0;
  return {
    frameType,
    streamId,
    payload: frame.slice(TUNNEL_FRAME_HEADER_BYTES),
    hasMoreFragments,
  };
};

// JSON payload 编码用的共享 TextEncoder 单例。
const textEncoder = new TextEncoder();
// JSON payload 解码用的共享 TextDecoder 单例。
const textDecoder = new TextDecoder();

// 把任意值序列化为 UTF-8 JSON 字节串。
/** @param {unknown} value */
export const encodeJsonPayload = (value) => textEncoder.encode(JSON.stringify(value));

/**
 * 解析 JSON payload 并用调用方提供的 validate 做形状校验；解析失败或
 * 校验不通过都抛 TunnelCodecError（消息区分解析错误与形状错误）。
 */
/**
 * @param {Uint8Array} payload
 * @param {(parsed: unknown) => boolean} validate
 */
export const decodeJsonPayload = (payload, validate) => {
  let parsed;
  try {
    parsed = JSON.parse(textDecoder.decode(payload));
  } catch {
    throw new TunnelCodecError('malformed JSON tunnel payload');
  }
  if (!validate(parsed)) {
    throw new TunnelCodecError('unexpected JSON tunnel payload shape');
  }
  return parsed;
};

/**
 * 把消息/请求体按 payload 尺寸切块；空输入返回一个空块，保证调用方
 * 总能发出至少一帧。
 */
/**
 * Split a body/message into payload-sized chunks. Empty input yields one empty chunk.
 * @param {Uint8Array} bytes
 * @param {number} [chunkSize]
 */
export const chunkPayload = (bytes, chunkSize = MAX_TUNNEL_PAYLOAD_BYTES) => {
  if (chunkSize <= 0 || chunkSize > MAX_TUNNEL_PAYLOAD_BYTES) {
    throw new TunnelCodecError('invalid chunk size');
  }
  if (bytes.length === 0) return [new Uint8Array(0)];
  const chunks = [];
  for (let offset = 0; offset < bytes.length; offset += chunkSize) {
    chunks.push(bytes.slice(offset, offset + chunkSize));
  }
  return chunks;
};

/**
 * 把一条逻辑消息编码为一帧或多帧：除最后一帧外都置分片标记。用于
 * 超过单帧预算的 WS 消息。
 */
/**
 * Encode one logical message as one or more frames, setting the fragment flag
 * on all but the last. Used for WS messages that exceed the frame budget.
 * @param {number} frameType
 * @param {number} streamId
 * @param {Uint8Array} payload
 */
export const encodeFragmentedMessage = (frameType, streamId, payload) => {
  const chunks = chunkPayload(payload);
  return chunks.map((chunk, index) => encodeTunnelFrame(frameType, streamId, chunk, index < chunks.length - 1));
};

/**
 * 创建分片重组器：按 (streamId, frameType) 维度累积分片，凑齐后返回
 * 完整消息；总字节数超过 maxMessageBytes 时丢弃并抛错，防止恶意分片
 * 撑爆内存。流关闭时用 dropStream 清理该流的全部待重组分片。
 */
/**
 * Reassembles fragmented messages per (streamId, frameType). Bounded to protect memory.
 * @param {number} [maxMessageBytes]
 */
export const createFragmentAssembler = (maxMessageBytes = 16 * 1024 * 1024) => {
  // 待重组分片表：`${streamId}:${frameType}` -> { chunks, totalBytes }。
  const pending = new Map();
  return {
    /**
     * Returns the complete message payload once all fragments arrived, or null
     * while more fragments are expected.
     * @param {{ frameType: number, streamId: number, payload: Uint8Array, hasMoreFragments: boolean }} frame
     */
    push(frame) {
      const key = `${frame.streamId}:${frame.frameType}`;
      const entry = pending.get(key);
      if (!frame.hasMoreFragments && !entry) {
        return frame.payload;
      }
      const chunks = entry?.chunks ?? [];
      const totalBytes = (entry?.totalBytes ?? 0) + frame.payload.length;
      if (totalBytes > maxMessageBytes) {
        pending.delete(key);
        throw new TunnelCodecError('fragmented message exceeds maximum size');
      }
      chunks.push(frame.payload);
      if (frame.hasMoreFragments) {
        pending.set(key, { chunks, totalBytes });
        return null;
      }
      pending.delete(key);
      const message = new Uint8Array(totalBytes);
      let offset = 0;
      for (const chunk of chunks) {
        message.set(chunk, offset);
        offset += chunk.length;
      }
      return message;
    },
    /** @param {number} streamId */
    dropStream(streamId) {
      for (const key of pending.keys()) {
        if (key.startsWith(`${streamId}:`)) pending.delete(key);
      }
    },
  };
};

/**
 * 批量封装编码：把一或多帧打包进单个明文。单帧走 SINGLE tag 直接前缀；
 * 多帧走 BATCH tag 并为每帧加 4 字节大端长度前缀。总长超过 64 KiB
 * 明文上限时抛 TunnelCodecError（信封必须装进一次加密调用）。
 */
/**
 * Batch envelope encoder (mirror of tunnel-codec.ts encodeFrameBatch). Only used
 * when both peers negotiated `batch`. One encrypted WS message still equals one
 * encrypt() call — this only changes how many tunnel frames it carries.
 * @param {Uint8Array[]} frames
 * @returns {Uint8Array}
 */
export const encodeFrameBatch = (frames) => {
  if (frames.length === 0) {
    throw new TunnelCodecError('cannot encode an empty frame batch');
  }
  if (frames.length === 1) {
    const frame = frames[0];
    const out = new Uint8Array(1 + frame.length);
    out[0] = BATCH_CONTAINER_TAG_SINGLE;
    out.set(frame, 1);
    if (out.length > MAX_PLAINTEXT_FRAME_BYTES) {
      throw new TunnelCodecError('frame batch exceeds maximum plaintext size');
    }
    return out;
  }
  let total = 1;
  for (const frame of frames) total += BATCH_FRAME_LENGTH_BYTES + frame.length;
  if (total > MAX_PLAINTEXT_FRAME_BYTES) {
    throw new TunnelCodecError('frame batch exceeds maximum plaintext size');
  }
  const out = new Uint8Array(total);
  out[0] = BATCH_CONTAINER_TAG_BATCH;
  let offset = 1;
  for (const frame of frames) {
    out[offset] = (frame.length >>> 24) & 0xff;
    out[offset + 1] = (frame.length >>> 16) & 0xff;
    out[offset + 2] = (frame.length >>> 8) & 0xff;
    out[offset + 3] = frame.length & 0xff;
    offset += BATCH_FRAME_LENGTH_BYTES;
    out.set(frame, offset);
    offset += frame.length;
  }
  return out;
};

/**
 * 批量封装解码：还原出有序的隧道帧列表。tag 非法、长度前缀或帧体被
 * 截断、解析后一帧不剩时抛 TunnelCodecError。
 */
/**
 * Decodes a batch-envelope plaintext into its ordered tunnel frames.
 * @param {Uint8Array} plaintext
 * @returns {Uint8Array[]}
 */
export const decodeFrameBatch = (plaintext) => {
  if (plaintext.length < 1) {
    throw new TunnelCodecError('empty batch plaintext');
  }
  const tag = plaintext[0];
  if (tag === BATCH_CONTAINER_TAG_SINGLE) {
    return [plaintext.slice(1)];
  }
  if (tag !== BATCH_CONTAINER_TAG_BATCH) {
    throw new TunnelCodecError(`unknown batch container tag ${tag}`);
  }
  const frames = [];
  let offset = 1;
  while (offset < plaintext.length) {
    if (offset + BATCH_FRAME_LENGTH_BYTES > plaintext.length) {
      throw new TunnelCodecError('truncated batch frame length');
    }
    const length =
      ((plaintext[offset] << 24)
        | (plaintext[offset + 1] << 16)
        | (plaintext[offset + 2] << 8)
        | plaintext[offset + 3]) >>> 0;
    offset += BATCH_FRAME_LENGTH_BYTES;
    if (offset + length > plaintext.length) {
      throw new TunnelCodecError('truncated batch frame body');
    }
    frames.push(plaintext.slice(offset, offset + length));
    offset += length;
  }
  if (frames.length === 0) {
    throw new TunnelCodecError('empty frame batch');
  }
  return frames;
};

// 只有高吞吐的 body/stream 数据帧才进批量缓冲；建连/拆链/keepalive 帧
// 立即冲刷，保证 TTFT、终端回显与活性探测的即时性。
// Only high-volume body/stream data is buffered; setup/teardown/keepalive frames
// flush immediately so TTFT, terminal echo, and liveness stay snappy.
const BUFFERED_FRAME_TYPES = new Set([
  TunnelFrameType.HttpBody,
  TunnelFrameType.WsText,
  TunnelFrameType.WsBinary,
]);

// 批量窗口默认值的依据见 TS 镜像（tunnel-codec.ts）：聊天渲染管线的
// 100ms 输入节流 + 约 64ms 的匀速显影动画让 150ms 批量窗口对用户不可见。
// See the TS mirror (tunnel-codec.ts) for the 150ms rationale: the chat render pipeline's
// 100ms input throttle + ~64ms paced-reveal smoothing make a 150ms batch window invisible.
export const DEFAULT_BATCH_WINDOW_MS = 150;
// 单个批量明文的字节预算（帧含长度前缀的累计成本）。
export const DEFAULT_BATCH_MAX_BYTES = 24 * 1024;
// 单个批量明文最多携带的帧数。
export const DEFAULT_BATCH_MAX_FRAMES = 32;

/**
 * 创建出站帧批量缓冲器（tunnel-codec.ts createOutboundFrameBatcher 的
 * 镜像）：非缓冲帧型立即冲刷；缓冲帧型在 windowMs 窗口内聚合，直到
 * 超出字节/帧数预算或距上次冲刷已隔满一个窗口。timer/now 可注入以便
 * 测试。
 */
/**
 * Outbound batching buffer (mirror of tunnel-codec.ts createOutboundFrameBatcher).
 * @param {{
 *   windowMs?: number,
 *   maxBatchBytes?: number,
 *   maxBatchFrames?: number,
 *   sendBatch: (plaintext: Uint8Array) => void,
 *   now?: () => number,
 *   setTimer?: (fn: () => void, ms: number) => any,
 *   clearTimer?: (handle: any) => void,
 * }} options
 */
export const createOutboundFrameBatcher = (options) => {
  const windowMs = options.windowMs ?? DEFAULT_BATCH_WINDOW_MS;
  const maxBatchBytes = options.maxBatchBytes ?? DEFAULT_BATCH_MAX_BYTES;
  const maxBatchFrames = options.maxBatchFrames ?? DEFAULT_BATCH_MAX_FRAMES;
  const now = options.now ?? (() => Date.now());
  const setTimer = options.setTimer ?? ((fn, ms) => setTimeout(fn, ms));
  const clearTimer = options.clearTimer ?? ((handle) => clearTimeout(handle));

  // 批量缓冲状态：待发帧、累计字节数、冲刷定时器与上次冲刷时刻。
  let buffer = [];
  let bufferedBytes = 0;
  let timer = null;
  let lastFlushAt = 0;
  let disposed = false;

  // 取消尚未到期的冲刷定时器（冲刷与销毁时调用）。
  const clearPendingTimer = () => {
    if (timer !== null) {
      clearTimer(timer);
      timer = null;
    }
  };

  // 立即发出缓冲中的全部帧（经 encodeFrameBatch 打包），并记录冲刷时刻。
  const flush = () => {
    clearPendingTimer();
    if (buffer.length === 0) return;
    const frames = buffer;
    buffer = [];
    bufferedBytes = 0;
    lastFlushAt = now();
    options.sendBatch(encodeFrameBatch(frames));
  };

  // 入队一帧：按帧型决定立即冲刷还是进入窗口聚合；预算溢出先冲刷再入队。
  const enqueue = (frame) => {
    if (disposed) return;
    const frameType = frame[0] & ~TUNNEL_FRAGMENT_FLAG;
    if (!BUFFERED_FRAME_TYPES.has(frameType)) {
      buffer.push(frame);
      flush();
      return;
    }
    const at = now();
    if (buffer.length === 0 && at - lastFlushAt >= windowMs) {
      buffer.push(frame);
      flush();
      return;
    }
    const frameCost = BATCH_FRAME_LENGTH_BYTES + frame.length;
    if (buffer.length > 0 && 1 + bufferedBytes + frameCost > MAX_PLAINTEXT_FRAME_BYTES) {
      flush();
    }
    buffer.push(frame);
    bufferedBytes += frameCost;
    if (bufferedBytes >= maxBatchBytes || buffer.length >= maxBatchFrames) {
      flush();
      return;
    }
    if (timer === null) timer = setTimer(flush, windowMs);
  };

  return {
    enqueue,
    flush,
    dispose() {
      disposed = true;
      clearPendingTimer();
      buffer = [];
      bufferedBytes = 0;
    },
  };
};
