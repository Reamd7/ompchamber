// OpenCode-compatible endpoint implementations for the omp host.
//
// Every route here exists because OMPChamber's vendored wire client calls it
// (see packages/ui/src/lib/opencode/wire). Features with no omp equivalent
// respond with stable, explicit errors instead of pretending success.

/**
 * omp host 的 OpenCode 兼容端点实现（Node 参考实现；Rust 版见 server-rs）。
 *
 * 职责：把 OmpHostEngine 的能力挂载为 wire client（packages/ui/src/lib/opencode/wire）
 * 认识的 OpenCode 路由 —— session 生命周期、prompt 分发、SSE 事件流、config 与
 * provider 面板、file/find 工作区浏览，以及 /omp/* 的 omp 原生端点。没有 omp 对应
 * 实现的路由以稳定的 501/404/空集合显式应答，绝不伪装成功。跨模块关系：host.ts
 * 负责 Basic auth 与路由分发并调用 registerEndpoints；domain-* 模块注册各自的
 * /api/omp/* 子域路由；engine.ts 提供全部业务能力；events.ts 提供 SSE 事件总线。
 */
import fs from 'node:fs';
import path from 'node:path';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { BUILTIN_TOOLS, getAgentDir } from '@oh-my-pi/pi-coding-agent';
import { normalizeDirectoryKey } from './registry.ts';
import type { SessionMetadataValue } from './registry.ts';
import { buildCapabilities, featureUnavailable, ompFeatures } from './omp-parity.ts';
import { registerModelSettingsRoutes, buildModelsPayload } from './domain-models.ts';
import { ModeDomainError, registerModesDomainRoutes } from './domain-modes.ts';
import { registerCommandsDomainRoutes } from './domain-commands.ts';
import { registerChromeDomainRoutes } from './domain-chrome.ts';
import { registerPluginsDomainRoutes } from './domain-plugins.ts';
import { registerProvidersDomainRoutes } from './domain-providers.ts';
import type { Settings, Skill } from '@oh-my-pi/pi-coding-agent';
import type { OmpHostEngine } from './engine.ts';
import type { PublishFn, SettingsStore } from './domain-models.ts';
import type { ReplayState } from './events.ts';
import type { ProjectedMessage } from './projection.ts';


/**
 * Engine face defaultModelPointer consumes: just the lazy settings store
 * (structural — the parity tests inject a one-method stub).
 */
/**
 * defaultModelPointer 消费的最小引擎表面（structural typing）：只需懒加载就绪的
 * settings store；parity 测试会注入一个单方法 stub 替代真实引擎。
 */
export interface DefaultModelPointerEngine {
  /** 懒就绪的 keyed Settings store；boot 降级路径上可能解析为 null。 */
  settingsStoreReady: () => Promise<{ settingsFor: (directory?: string) => Promise<Settings> } | null>;
}

/** Wire default-model pointer: `{ model: 'provider/id' }` or `{}`. */
/** wire 协议的默认模型指针：{ model: 'provider/id' } 或空对象（未解析出时省略键）。 */
export interface DefaultModelPointer {
  /** 'provider/id' 形式的默认模型标识；省略即无角色默认值。 */
  model?: string;
}

/**
 * Resolve the wire config's default-model pointer from modelRoles.default
 * through the keyed Settings instance (spec 01 §5.3/GAP-03). Falls back to
 * omitting the key when no role default resolves — never pins the
 * alphabetically-first provider model.
 */
/**
 * 从 keyed Settings 的 modelRoles.default 解析 wire config 的默认模型指针：
 * 成功得到 provider 与 id 才返回 { model: 'provider/id' }；解析不到角色默认值、
 * settings 抛错或 store 为 null 时都返回空对象（省略 model 键），绝不退化为按
 * 字母序排在第一位的 provider 模型（spec 01 §5.3/GAP-03）。
 */
export const defaultModelPointer = async (engine: DefaultModelPointerEngine): Promise<DefaultModelPointer> => {
  try {
    const store = await engine.settingsStoreReady();
    if (!store) return {};
    const settings = await store.settingsFor(process.cwd());
    const roleDefault = buildModelsPayload(settings).roles?.default;
    if (roleDefault?.provider && roleDefault?.id) {
      return { model: `${roleDefault.provider}/${roleDefault.id}` };
    }
  } catch {
    // Settings unavailable: omit the pointer rather than pin a wrong model.
  }
  return {};
};
/** What `Response.json` itself accepts — this helper only forwards to it. */
/** Response.json 原生接受的负载类型 —— json helper 只做纯转发。 */
type ResponseJsonData = Parameters<typeof Response.json>[0];

/** 统一的 JSON Response 构造器：所有 handler 经此构造应答，保证负载形状一致。 */
const json = (data: ResponseJsonData, init?: ResponseInit): Response => Response.json(data, init);

/** promisify 后的 execFile：为 git() 提供 promise 化的子进程调用。 */
const execFileAsync = promisify(execFile);

// Domain errors from the prompt path (e.g. persona-not-found 404, 02 §5.1
// R2-M3) answer with their own status; anything else stays a 500 in host.js.
/** 把 prompt 路径的领域错误翻译成自带状态码的 Response（如 persona 不存在 → 404）；
 * 非领域异常原样上抛，由 host.js 统一按 500 处理。 */
const domainErrorsToResponse = async <T>(run: () => Promise<T>): Promise<T | Response> => {
  try {
    return await run();
  } catch (error) {
    if (error instanceof ModeDomainError) return json(error.body, { status: error.status });
    throw error;
  }
};
/** 404 应答：OpenCode wire 错误负载 { name: 'UnknownError', data: { message } }。 */
const notFound = (message: string): Response =>
  json({ name: 'UnknownError', data: { message } }, { status: 404 });

/** 400 应答：同一 wire 错误负载形状，用于参数缺失/非法等请求侧错误。 */
const badRequest = (message: string): Response =>
  json({ name: 'UnknownError', data: { message } }, { status: 400 });

/** 501 应答：该能力在 omp 中没有对应实现，明确拒绝而非假装成功。 */
const unsupported = (message: string): Response =>
  json({ name: 'UnknownError', data: { message } }, { status: 501 });

// Wire JSON body parse (same parse-time assertion convention as the domain
// modules — domain-modes.ts readJsonBody): the call-site type parameter is
// the contract; every field is runtime-validated by the handler.
/**
 * 解析请求 JSON body。约定与 domain 模块一致：调用点的类型参数即契约，字段由
 * handler 使用前逐个做运行时校验；body 非法或非 JSON 时降级为空对象（满足
 * T extends object 约束），handler 将缺失字段视为未提供。
 */
const readJsonBody = async <T extends object>(request: Request): Promise<T> => {
  try {
    // SAFETY: parse-time assertion convention — the call-site type parameter
    // is the contract; every field is runtime-validated by the handler
    // before use.
    return (await request.json()) as T;
  } catch {
    // SAFETY: a malformed body answers as `{}`, which satisfies the
    // `T extends object` bound; every handler treats missing fields as
    // absent.
    return {} as T;
  }
};

/**
 * Wire agent-mention source span (the generated AgentPart.source shape):
 * the mention text plus the character offsets it occupies.
 */
/** wire agent 提及（mention）的 source 跨度：提及文本及其在原文中的字符偏移。 */
export interface WirePromptPartSource {
  /** 提及的原文文本（如 '@scout'）。 */
  value?: string;
  /** 提及起点的字符偏移。 */
  start?: number;
  /** 提及终点的字符偏移。 */
  end?: number;
}

/**
 * One wire prompt part (SessionPromptAsyncData superset: text/file/agent/
 * subtask variants, plus the span fields that ride along with agent
 * mentions). Fields are runtime-validated per variant before use.
 */
/** wire prompt 的单个 part（SessionPromptAsyncData 的超集：text/file/agent/subtask
 * 变体，外加随 agent 提及同行的跨度字段）；各字段按变体在使用前逐个运行时校验。 */
export interface WirePromptPart {
  /** part 变体标记：'text' | 'file' | 'agent' | 'subtask'。 */
  type?: unknown;
  /** text/subtask 变体携带的文本内容。 */
  text?: unknown;
  /** file 变体的 data: URL 载荷。 */
  url?: unknown;
  /** agent 变体中被提及者的名称。 */
  name?: unknown;
  /** file 变体的原始文件名（内联 <file> 块时展示）。 */
  filename?: unknown;
  /** file 变体的 MIME 类型（data URL meta 缺失时的兜底）。 */
  mime?: string;
  /** subtask 变体的子任务提示词。 */
  prompt?: unknown;
  /** agent 变体的提及跨度（可选，即 AgentPart.source 形状）。 */
  source?: WirePromptPartSource | null;
}

/**
 * Prompt-endpoint JSON body: the parts-based SessionPromptAsyncData shape
 * plus the legacy `{ prompt: { text, files } }` form.
 */
/** prompt 端点的 JSON body：parts 形态（SessionPromptAsyncData）叠加 legacy 的
 * { prompt: { text, files } } 形态，两种形态由 promptPayloadFromWire 统一归并。 */
export interface WirePromptBody {
  /** 客户端指定的消息 id（对账/幂等用，可选）。 */
  messageID?: unknown;
  /** 新形态：part 数组（text/file/agent/subtask 变体）。 */
  parts?: WirePromptPart[] | null;
  /** legacy 形态：文本 + base64 文件列表（data + mime）。 */
  prompt?: { text?: string; files?: { data?: unknown; mime?: string }[] } | null;
}

/** Base64 image input the engine's prompt consumes. */
/** 引擎 prompt 消费的 base64 图片输入。 */
export interface PromptImage {
  /** base64 编码的图片字节。 */
  data: string;
  /** 图片 MIME 类型；可能为 undefined，由引擎侧兜底。 */
  mimeType: string | undefined;
}

/** Prompt inputs parsed from a wire body (promptPayloadFromWire's return). */
/** promptPayloadFromWire 的返回值：从 wire body 解析出的引擎 prompt 输入。 */
export interface PromptPayload {
  /** 归并后的完整文本（text 块与内联 <file> 块以空行连接）。 */
  text: string;
  /** 图片载荷数组（仅收 image/* 且非 SVG 的 part）。 */
  images: PromptImage[];
  /** 透传的客户端消息 id；未提供时为 undefined。 */
  messageID: string | undefined;
}

/**
 * Image MIME check for the prompt's image channel. `image/svg+xml` is XML
 * text — providers reject it as an image payload, so it inlines as text.
 */
/** 判定 MIME 能否进入图片通道：image/* 且排除 SVG —— SVG 是 XML 文本，provider
 * 会拒绝其作为图片载荷，因此改走文本内联路径。 */
const isImageMime = (mimeType: string): boolean =>
  mimeType.toLowerCase().startsWith('image/') && mimeType.toLowerCase() !== 'image/svg+xml';

/**
 * Attachment types whose bytes can never be inlined as text. Anything not on
 * this list still has to pass the byte sniff before it inlines.
 */
/** 一票判定为二进制的附件 MIME 前缀（pdf/zip/音视频/字体等）；不在列表内的类型
 * 仍需通过字节嗅探才允许内联为文本。 */
const BINARY_ATTACHMENT_MIME =
  /^(application\/(pdf|zip|x-[^/]+|gzip|msword|vnd\.|oasis\.)|audio\/|video\/|font\/)/i;

/** 字节嗅探的采样上限：只检查前 4KiB 即足以判定文本/二进制。 */
const ATTACHMENT_TEXT_SAMPLE_BYTES = 4096;

/**
 * Same heuristic the UI's attachment pipeline applies (NUL → binary, >30%
 * control bytes → binary), re-checked server-side so a mislabeled part never
 * reaches the provider as an image.
 */
/** 服务端复检的字节嗅探启发式（与 UI 附件管线一致：出现 NUL 判二进制，控制字节
 * 占比超 30% 判二进制），确保被误标的 part 永不以图片身份抵达 provider。 */
const looksLikeUtf8Text = (bytes: Buffer): boolean => {
  const sample = bytes.subarray(0, ATTACHMENT_TEXT_SAMPLE_BYTES);
  let control = 0;
  for (const byte of sample) {
    if (byte === 0) return false;
    if (byte < 9 || (byte > 13 && byte < 32)) control += 1;
  }
  return control / Math.max(1, sample.length) <= 0.3;
};

/** Decoded data-URL file part: raw bytes plus the effective MIME type. */
/** data URL 解码结果：原始字节加上生效的 MIME 类型。 */
interface WireFilePayload {
  /** 解码后的原始字节（base64 或 percent-decoded UTF-8）。 */
  bytes: Buffer;
  /** 生效 MIME：data URL meta 优先，其次 fallback，最后 octet-stream。 */
  mimeType: string;
}

/**
 * 解析 data: URL 文件 part 为原始字节。base64 形态直接解码；非 base64 形态按
 * RFC 2397 视为 percent-encoded UTF-8，解码失败时保留原始载荷而非让整个
 * prompt 失败。空载荷返回 null，调用方跳过该 part。
 */
const wireFileFromDataUrl = (url: string, fallbackMime: string | undefined): WireFilePayload | null => {
  const comma = url.indexOf(',');
  const meta = url.slice(5, comma === -1 ? url.length : comma);
  const payload = url.slice(comma === -1 ? url.length : comma + 1);
  const mimeType = meta.split(';')[0] || fallbackMime || 'application/octet-stream';
  let bytes: Buffer;
  if (/;base64$/i.test(meta)) {
    bytes = Buffer.from(payload, 'base64');
  } else {
    // Non-base64 data URLs are percent-encoded UTF-8 (RFC 2397); malformed
    // escapes keep the raw payload rather than failing the whole prompt.
    let decoded = payload;
    try {
      decoded = decodeURIComponent(payload);
    } catch {
      // keep raw payload
    }
    bytes = Buffer.from(decoded, 'utf8');
  }
  return bytes.length > 0 ? { bytes, mimeType } : null;
};

/** 转义会破坏 <file name="..."> 属性的四个字符（& " < >）。 */
const escapeFileAttr = (value: string): string =>
  value.replace(/&/g, '&amp;').replace(/"/g, '&quot;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

/**
 * Text-block form of a non-image attachment: decoded content inlined when it
 * is decodable text, an explicit omission note otherwise. The note matters —
 * silently dropping the block or pushing it into `images` is what made a
 * `.txt`/`.pdf` attachment 400 against vision providers and then poison every
 * subsequent turn by replaying the bad block from session history.
 */
/** 非图片附件的文本块形态：可解码为文本时内联原始内容，否则给出显式省略标注。
 * 不能静默丢块或塞进 images —— 那曾让 .txt/.pdf 附件在 vision provider 上 400，
 * 并因会话历史回放坏块污染后续每一轮。 */
const fileAttachmentText = (filename: string | undefined, mimeType: string, bytes: Buffer): string => {
  const name = escapeFileAttr(filename && filename.length > 0 ? filename : 'attachment');
  if (!BINARY_ATTACHMENT_MIME.test(mimeType) && looksLikeUtf8Text(bytes)) {
    return `<file name="${name}" mime="${mimeType}">\n${bytes.toString('utf8')}\n</file>`;
  }
  return `<file name="${name}" mime="${mimeType}">[attachment omitted: binary content is not supported]</file>`;
};

/**
 * Runtime string probe without `typeof` (anti-slop): returns the string
 * unchanged, undefined for anything else — including typed-but-wrong runtime
 * values in untrusted request JSON. Generic so callers can probe any declared
 * field type; the tag check does the verification.
 */
/** 运行时字符串探针（不依赖 typeof）：值确为字符串时原样返回，其余一律
 * undefined —— 用于逐字段校验不可信请求 JSON 中声明类型与运行时不符的值。 */
const stringOf = <T,>(value: T): string | undefined =>
  Object.prototype.toString.call(value) === '[object String]' ? String(value) : undefined;

/**
 * Parse a prompt request body into the engine's prompt inputs.
 *
 * The wire contract sends `{ parts: [text|file|agent|subtask], messageID }`
 * (see SessionPromptAsyncData in the vendored types). The legacy
 * `{ prompt: { text, files } }` shape is still accepted for the synchronous
 * `/message` consumer. Dropping `parts` here persisted every user message
 * with an empty part list, which the UI hides after a reload.
 *
 * Only `image/*` file parts become image payloads: `session.prompt` forwards
 * `images` to the provider verbatim, so a non-image part sent as an "image"
 * hard-fails the request against vision models and leaves a poisoned block
 * in session history. Text-decodable files inline as `<file>` blocks;
 * binaries degrade to an explicit omission note.
 */
/**
 * 把 wire prompt body 归并为引擎 prompt 输入。parts 形态逐个处理 text/file/
 * agent/subtask 变体；parts 为空时回落到 legacy { prompt: { text, files } }。
 * 只有 image/*（非 SVG）的 file part 进入 images —— session.prompt 会把 images
 * 原样转发给 provider，非图片伪装成图片会让 vision 模型硬失败并在会话历史留下
 * 坏块；可解码文本内联为 <file> 块，二进制降级为省略标注；agent 提及仅补一条
 * 未重复出现的 mention 文本。text 各块以空行连接。
 */
export const promptPayloadFromWire = (body: WirePromptBody): PromptPayload => {
  const messageID = stringOf(body?.messageID) || undefined;
  const parts = Array.isArray(body?.parts) ? body.parts : [];
  if (parts.length === 0) {
    const texts: string[] = [];
    const images: PromptImage[] = [];
    for (const file of body?.prompt?.files ?? []) {
      const data = stringOf(file?.data);
      if (!data) continue;
      const mimeType = stringOf(file?.mime) || 'application/octet-stream';
      if (isImageMime(mimeType)) {
        images.push({ data, mimeType });
      } else {
        texts.push(fileAttachmentText(undefined, mimeType, Buffer.from(data, 'base64')));
      }
    }
    const promptText = stringOf(body?.prompt?.text);
    if (promptText) texts.unshift(promptText);
    return { text: texts.join('\n\n'), images, messageID };
  }
  const texts: string[] = [];
  const images: PromptImage[] = [];
  for (const part of parts) {
    if (part.type === 'text') {
      const text = stringOf(part.text);
      if (text) texts.push(text);
    } else if (part.type === 'file') {
      const url = stringOf(part.url);
      if (!url || !url.startsWith('data:')) continue;
      const file = wireFileFromDataUrl(url, stringOf(part.mime));
      if (!file) continue;
      if (isImageMime(file.mimeType)) {
        images.push({ data: file.bytes.toString('base64'), mimeType: file.mimeType });
      } else {
        texts.push(fileAttachmentText(stringOf(part.filename), file.mimeType, file.bytes));
      }
    } else if (part.type === 'agent') {
      const name = stringOf(part.name);
      if (!name) continue;
      const mention = stringOf(part.source?.value) ?? `@${name}`;
      if (!texts.some((text) => text.includes(mention))) texts.push(mention);
    } else if (part.type === 'subtask') {
      const prompt = stringOf(part.prompt);
      if (prompt) texts.push(prompt);
    }
  }
  return { text: texts.join('\n\n'), images, messageID };
};

/** 从请求解析目录作用域：优先 query 的 directory / location[directory]，其次
 * x-opencode-directory header（percent-decode 后），统一过 normalizeDirectoryKey；
 * 两处皆无时返回 null，由调用方决定兜底。 */
const directoryFromRequest = ({ url, headers }: { url?: URL; headers?: Headers }): string | null => {
  const fromQuery = url?.searchParams.get('directory') ?? url?.searchParams.get('location[directory]');
  const fromHeader = headers?.get('x-opencode-directory');
  const raw = fromQuery ?? (fromHeader ? decodeURIComponent(fromHeader) : null);
  return raw ? normalizeDirectoryKey(raw) : null;
};

/** 在指定目录执行 git 子命令，返回 trimmed stdout；任何失败（非 git 仓库、命令
 * 报错等）返回 null，调用方按无 git 信息处理。 */
const git = async (cwd: string, ...args: string[]): Promise<string | null> => {
  try {
    const { stdout } = await execFileAsync('git', ['-C', cwd, ...args], { windowsHide: true });
    return stdout.trim();
  } catch {
    return null;
  }
};

/**
 * SSE response init: streaming headers Bun keeps open past its idle cap.
 * `x-omp-epoch` is the boot identity (plan §5.2.1): raw-fetch readers
 * (upstream-reader) compare it on connect; in-band consumers read the
 * `omp.stream.boot` frame instead.
 */
/** SSE 流式应答的 ResponseInit：no-cache/keep-alive/x-accel-buffering 等让 Bun
 * 不因空闲上限断连的头；x-omp-epoch 携带 boot 身份供 raw-fetch 读取端比对。 */
const sseResponseInit = (epoch: string): ResponseInit => ({
  status: 200,
  headers: {
    'content-type': 'text/event-stream',
    'cache-control': 'no-cache',
    connection: 'keep-alive',
    'x-accel-buffering': 'no',
    'x-omp-epoch': epoch,
  },
});

/** Queued-but-undrained bytes beyond this mark a dead-slow consumer. */
/** 慢消费者阈值：队列中未排空字节超过 16MiB 即判定死慢连接并关闭。 */
const MAX_SSE_BACKLOG_BYTES = 16 * 1024 * 1024;
/** A frame this large is only fatal while the connection is already behind. */
/** 单帧阈值：仅在连接本已落后时，超过 8MiB 的帧才视为致命。 */
const MAX_SSE_BACKLOG_FRAME_BYTES = 8 * 1024 * 1024;


/**
 * Shared SSE connection lifecycle (plan §5.3/§5.3.1): `send()` enqueues with
 * deterministic teardown on enqueue failure, request abort, or slow-consumer
 * backlog. `ReadableStream.enqueue()` carries no network backpressure, so a
 * client that stops draining would otherwise buffer without bound; the
 * stream's byte-sized queuing strategy makes `desiredSize` the drained-byte
 * signal — exceeding the backlog bound closes the connection (the client
 * reconnects into the gap/resync contract instead of silently queuing
 * gigabytes). desiredSize here counts BYTES (the size function returns
 * byteLength), never chunk count.
 */
/**
 * 共享的 SSE 连接生命周期：send() 入队，入队失败、请求 abort 或积压超限时确定性
 * 收尾（清心跳、退订、关闭 controller）。ReadableStream.enqueue 不携带网络背压，
 * 停止排空的客户端会无界缓冲；size 函数按字节计数使 desiredSize 成为未排空字节
 * 信号，超限即关连接让客户端转入重连的 gap/resync 契约。onReady 返回的退订函数
 * 在流已死于初始回放时会被立即调用，防止订阅泄漏。
 */
const startSseStream = (
  request: Request,
  epoch: string,
  onReady: (send: (frame: string) => void) => () => void,
): Response => {
  const stream = new ReadableStream(
    {
      start(controller) {
        const encoder = new TextEncoder();
        let closed = false;
        let unsubscribe: (() => void) | null = null;
        let heartbeat: ReturnType<typeof setInterval> | undefined;
        const finish = () => {
          if (closed) return;
          closed = true;
          if (heartbeat !== undefined) {
            clearInterval(heartbeat);
            heartbeat = undefined;
          }
          unsubscribe?.();
          unsubscribe = null;
          try {
            controller.close();
          } catch {
            // Already closed or errored by the consumer.
          }
        };
        const send = (frame: string) => {
          if (closed) return;
          const chunk = encoder.encode(frame);
          const backlog = controller.desiredSize ?? 0;
          if (backlog <= -MAX_SSE_BACKLOG_BYTES || (backlog < 0 && chunk.byteLength > MAX_SSE_BACKLOG_FRAME_BYTES)) {
            // Slow consumer: close, do not buffer unboundedly.
            finish();
            return;
          }
          try {
            controller.enqueue(chunk);
          } catch {
            finish();
          }
        };
        const onReadyResult = onReady(send);
        if (closed) {
          // finish() ran inside the initial replay (slow-consumer close or a
          // synchronous abort): detach the just-returned bus subscription
          // now — a closed stream never calls finish() again, so leaving it
          // would leak the subscriber for the bus's lifetime.
          try {
            onReadyResult?.();
          } catch {
            // Teardown is best-effort on an already-dead stream.
          }
          return;
        }
        unsubscribe = onReadyResult ?? null;
        // Heartbeat and the abort hook only exist once the stream is live:
        // creating the interval before onReady would leak it when the
        // initial replay throws.
        heartbeat = setInterval(() => send(':heartbeat\n\n'), 15000);
        request.signal.addEventListener('abort', finish, { once: true });
        if (request.signal.aborted) finish();
      },
    },
    // desiredSize must measure serialized bytes: the default count strategy
    // would let thousands of multi-megabyte frames pass under a chunk cap.
    { highWaterMark: 0, size: (chunk?: Uint8Array) => chunk?.byteLength ?? 0 },
  );
  return new Response(stream, sseResponseInit(epoch));
};

/** Structural bus face the resume planner needs. */
/** 断线续传规划所需的最小总线表面（structural typing）。 */
interface ResumableBus {
  /** 当前 boot 的事件流身份标识（epoch）。 */
  readonly epoch: string;
  /** 查询某 Last-Event-ID 在回放环中的可续传状态。 */
  replayState(lastEventId: number): ReplayState;
  /** 回放环尾部的最新事件 id；空环返回 null。 */
  tailEventId(): number | null;
}

/** planResume 的判定结果：是否 resync、订阅基线与回放环状态。 */
interface ResumePlan {
  /** true 表示先发 omp.stream.resync 控制帧并让客户端丢弃旧游标。 */
  resync: boolean;
  /** Subscription baseline: the client cursor when provable, else the tail. */
  baseline: number;
  /** 回放环对该游标的判定状态（ok / 被逐出 / 客户端领先等）。 */
  state: ReplayState;
}

/**
 * Decide replay-versus-resync for an SSE resume (plan §5.2/§5.2.1).
 * - Fresh connects (no cursor) replay the whole retained ring.
 * - A cursor the ring cannot bridge (evicted range, hole, restart) resyncs.
 * - A cursor without this boot's epoch cannot be proven to belong to this
 *   process — numeric comparison only detects client-ahead restarts — so
 *   epoch-less resuming clients (TUI, older builds) get the safe downgrade:
 *   discard-resume + explicit reconcile, never a guessed suffix.
 * - After a resync verdict the baseline switches to the real tail (0 when
 *   the ring is empty): replaying the old suffix would deliver the very
 *   half-events the control frame just disavowed.
 */
/**
 * 为 SSE 续连决定回放还是 resync：全新连接（无游标）回放整个保留环；环无法
 * 桥接的游标（被逐出/空洞/重启）resync；带游标但无本 boot epoch 的客户端无法
 * 证明游标属于本进程，安全降级为丢弃式 resync；一旦判定 resync，基线切换到
 * 真实尾部（空环为 0），避免重放出控制帧刚声明作废的半截事件。
 */
const planResume = (bus: ResumableBus, lastEventId: number, clientEpoch: string | null): ResumePlan => {
  const state: ReplayState = lastEventId > 0 ? bus.replayState(lastEventId) : { status: 'ok' };
  const epochMismatch = lastEventId > 0 && clientEpoch !== bus.epoch;
  const resync = state.status !== 'ok' || epochMismatch;
  return { resync, state, baseline: resync ? bus.tailEventId() ?? 0 : lastEventId };
};

/**
 * Per-request route context host.ts dispatches into every registered
 * handler (host.ts fetch: handler(request, { params, url, headers, engine })).
 */
/** host.ts 分发给每个已注册 handler 的每请求路由上下文。 */
export interface RouteContext {
  /** 路由模式中的命名参数（如 {sessionID}）。 */
  params: Record<string, string>;
  /** 已解析的请求 URL（含 query）。 */
  url: URL;
  /** 原始请求头。 */
  headers: Headers;
  /** 宿主引擎实例（所有业务能力的入口）。 */
  engine: OmpHostEngine;
}

/**
 * omp-host route handler (Basic auth is enforced by host.ts outside these).
 * `Promise<void>` covers the legacy `/session/{id}/command` handler, which
 * resolves without a Response exactly as it did pre-rename; host.ts answers
 * those requests at the Bun.serve boundary.
 */
/** omp-host 路由 handler 签名（Basic auth 由 host.ts 在这些 handler 之外统一
 * 强制）；Promise<void> 分支仅服务于 legacy 的 /session/{id}/command —— 它与
 * 改名前一样不带 Response 结束，由 host.ts 在 Bun.serve 边界应答。 */
export type RouteHandler = (
  request: Request,
  ctx: RouteContext,
) => Response | Promise<Response | void>;

/** `route(method, pattern, handler)` registration callback (host.ts). */
/** host.ts 提供的 route(method, pattern, handler) 注册回调。 */
export type RouteMount = (method: string, pattern: string, handler: RouteHandler) => void;

/** SSE upgrade handler host.ts routes /event and /global/event into. */
/** host.ts 把 /event 与 /global/event 路由到的 SSE handler；options.global
 * 为 true 时不按目录过滤事件。 */
export type SseHandler = (request: Request, options: { global: boolean }) => Response;

/** Context host.ts supplies to registerEndpoints. */
/** host.ts 传给 registerEndpoints 的上下文；本模块自身只消费 version。 */
export interface EndpointOptions {
  /** Host version echoed by /global/config and /global/health. */
  version: string;
  /** Host module directory (host.ts __dirname; asset resolution root).
   * Optional: registerEndpoints itself consumes only `version`. */
  dirname?: string;
}

/** JSON body of POST /session (session create). */
/** POST /session（创建会话）的 JSON body。 */
export interface SessionInitBody {
  /** 会话归属目录；缺省由请求上下文推导。 */
  directory?: string;
  /** 初始标题（可选）。 */
  title?: string;
  /** 父会话 id（派生会话时提供）。 */
  parentID?: string;
}
/**
 * Session metadata values: arbitrary JSON the wire client round-trips
 * opaquely (the host stores and returns them verbatim, never reading
 * individual entries).
 */
/** 透传 registry.ts 的会话元数据值类型：host 原样存取、从不逐项读取的任意 JSON。 */
export type { SessionMetadataValue };

/** JSON body of PATCH /session/{sessionID}. */
/** PATCH /session/{sessionID} 的 JSON body。 */
export interface SessionUpdateBody {
  /** 目标目录（优先于请求上下文推导）。 */
  directory?: string;
  /** 新标题（可选）。 */
  title?: string;
  /** 整体替换的元数据映射。 */
  metadata?: Record<string, SessionMetadataValue>;
  /** 时间戳字段；archived 非空即归档时间。 */
  time?: { archived?: number };
}

/** JSON body of the prompt-family routes (message, prompt_async, command). */
/** prompt 族路由（message、prompt_async、command）共用的 JSON body。 */
export interface SessionPromptBody extends WirePromptBody {
  /** 目标目录（优先于请求上下文推导）。 */
  directory?: string;
  /** legacy 显式模型选择（providerID/modelID）。 */
  model?: { providerID?: string; modelID?: string };
  /** 指定的 agent/persona 名称（可选）。 */
  agent?: string;
  /** 递交模式（如 'queue' 映射为 followUp）。 */
  delivery?: string;
  /** command 路由的斜杠命令文本。 */
  command?: string;
}

/** JSON body of POST /session/{sessionID}/revert. */
/** POST /session/{sessionID}/revert 的 JSON body。 */
export interface SessionRevertBody {
  /** 目标目录。 */
  directory?: string;
  /** 回退边界消息 id（必填，缺失即 400）。 */
  messageID?: string;
}

/** JSON body of POST /experimental/control-plane/move-session. */
/** POST /experimental/control-plane/move-session 的 JSON body。 */
export interface MoveSessionBody {
  /** 待迁移的会话 id。 */
  sessionID?: string;
  /** 目标位置（当前只消费 directory 字段）。 */
  destination?: { directory?: string };
}

/** JSON body of the directory-scoped session POST routes (unrevert, summarize, fork). */
/** 目录作用域会话 POST 路由（unrevert、summarize、fork）共用的 body。 */
export interface SessionDirectoryBody {
  /** 目标目录。 */
  directory?: string;
  /** Fork boundary (wire message id): present bounds the fork at that
   * message (TUI /branch); absent forks the whole transcript (TUI /fork). */
  messageID?: string;
}

/** JSON body of POST /omp/sessions/{id}/model (spec 01 GAP-02/04). */
/** POST /omp/sessions/{id}/model 的 JSON body（spec 01 GAP-02/04）。 */
export interface SessionModelBody {
  /** 目标模型 { providerID, modelID }；运行时校验为对象后才使用。 */
  model?: unknown;
  /** 思考档位；'inherit' 为清除显式设置的哨兵值。 */
  thinkingLevel?: string;
}


/** Wire app/skills row (AppSkillsResponses 200 element, types.gen.d.ts). */
/** wire app/skills 行（AppSkillsResponses 的 200 元素，见 types.gen.d.ts）。 */
export interface WireSkillRow {
  /** 技能名。 */
  name: string;
  /** frontmatter 描述；缺失时为空字符串。 */
  description: string;
  /** SKILL.md 文件路径（wire 字段名；SDK 侧称 filePath）。 */
  location: string;
  /** 去 frontmatter 后的正文；读取失败降级为空串。 */
  content: string;
}

/**
 * Strip a leading YAML frontmatter block (`---\n...\n---\n`) from a SKILL.md
 * file. Mirrors the SDK's own display-layer semantics (extensibility/skills.ts
 * buildSkillPromptMessage strips the same block; discovery/helpers.ts fills
 * `content` with the parseFrontmatter body). A file without frontmatter is
 * returned verbatim.
 */
/** 剥离 SKILL.md 开头的 YAML frontmatter 块（--- ... ---）；无 frontmatter 的
 * 文件原样返回。与 SDK 展示层语义一致。 */
const stripSkillFrontmatter = (content: string): string =>
  content.replace(/^---\r?\n[\s\S]*?\r?\n---\r?\n/, '');

/**
 * Project discovered SDK skills onto the wire app/skills rows the vendored
 * client consumes: `location` is the SKILL.md path (the wire field name —
 * the SDK calls it `filePath`) and `content` is the frontmatter-stripped
 * body. An unreadable SKILL.md degrades to empty content rather than
 * dropping the row: discovery just scanned it, so one vanished file must
 * not hide the surviving skills behind the route-wide `[]` fallback.
 */
/** 把 SDK 发现的技能投影为 wire app/skills 行：location 即 SKILL.md 路径，
 * content 为去 frontmatter 的正文；单个文件读取失败降级为空 content 而非丢行
 * （一个消失的文件不能把其余技能藏进路由级 [] 兜底）。 */
export const wireSkillRows = async (skills: readonly Pick<Skill, 'name' | 'description' | 'filePath'>[]): Promise<WireSkillRow[]> =>
  Promise.all(
    skills.map(async (skill) => ({
      name: skill.name,
      description: skill.description ?? '',
      location: skill.filePath,
      content: await fs.promises.readFile(skill.filePath, 'utf8').then(
        stripSkillFrontmatter,
        () => '',
      ),
    })),
  );

/**
 * Register every consumed route.
 * @param {(method: string, pattern: string, handler: Function) => void} route
 */
/**
 * 注册 wire client 消费的全部路由（host.js 启动时调用一次）。顺序：global →
 * session 族 → permission/question → config/app/skill → provider/auth → mcp →
 * path/project/vcs/file/find → experimental → domain-* 模块 → /omp 原生端点与
 * SSE。返回 { sseHandler } 供 host.ts 挂载 /event 与 /global/event。
 */
export const registerEndpoints = (route: RouteMount, engine: OmpHostEngine, { version }: EndpointOptions) => {
  // 构造 wire providers 载荷：把 engine 可用模型按 provider 分组（env 恒为空——尚无 env 透传）。
  const providersPayload = async () => {
    await engine.ready();
    const models = engine.availableModels();
    const byProvider = new Map();
    for (const model of models) {
      const list = byProvider.get(model.provider) ?? [];
      list.push(model);
      byProvider.set(model.provider, list);
    }
    const providers = [...byProvider.entries()].map(([id, list]) => ({
      id,
      name: id,
      // SAFETY: wire env list is empty in the omp payload (no env passthrough yet).
      env: [] as string[],
      models: list.map((model: { id: string; name?: string; reasoning?: boolean; contextWindow?: number; maxTokens?: number }) => ({
        id: model.id,
        name: model.name ?? model.id,
        ...(model.reasoning ? { reasoning: true } : {}),
        ...(model.contextWindow ? { limit: { context: model.contextWindow, output: model.maxTokens ?? 0 } } : {}),
      })),
    }));
    return providers;
  };

  // 构造 wire config 载荷：版本号 + defaultModelPointer 解析的默认模型指针。
  const configPayload = async () => {
    await engine.ready();
    return {
      version,
      // omp keeps its own model/provider config (~/.omp/agent/config.yml);
      // the default-model pointer resolves modelRoles.default through the
      // keyed Settings instance (spec 01 §5.3/GAP-03) instead of pinning
      // whichever provider model sorts first.
      ...(await defaultModelPointer(engine)),
      // Agent definitions are the omp discovery chain (02 §5.2); the legacy
      // wire payload carries no custom-agent list anymore.
      // SAFETY: wire payload carries no custom-agent list anymore (comment above).
      agents: [] as never[],
      // OpenCode-specific keys with no omp equivalent are absent.
    };
  };

  // 推导请求的项目目录：directoryFromRequest（query/header）优先，兜底 process.cwd()。
  const projectDirectory = (requestContext: { url?: URL }) =>
    directoryFromRequest(requestContext) ?? process.cwd();

  // ---- global ----
  // 健康检查：healthy 常量 + 版本与进程 uptime。
  route('GET', '/global/health', async () => json({ healthy: true, version, uptime: process.uptime() }));
  // 整机 dispose：先应答空对象，下一个事件循环退出进程（给响应留出冲刷窗口）。
  route('POST', '/global/dispose', async () => {
    setTimeout(() => process.exit(0), 0);
    return json({});
  });
  // 实例级 dispose：与 /global/dispose 同语义（旧客户端兼容路径）。
  route('POST', '/instance/dispose', async () => {
    setTimeout(() => process.exit(0), 0);
    return json({});
  });
  // 读取 wire config 载荷（版本 + 默认模型指针）。
  route('GET', '/global/config', async () => json(await configPayload()));
  // PATCH 面仅回读 config 载荷：omp 自持配置，host 不接受写。
  route('PATCH', '/global/config', async () => json(await configPayload()));

  // Counters-only observation port (plan §9.1, phase 0 slice; decision D9):
  // counts and declared byte estimates, never transcripts/payloads/paths.
  // Gated by the host's own Basic auth like every other route.
  route('GET', '/omp/diagnostics', async () => json(engine.getStreamDiagnostics()));

  // Memory monitor's per-row release: evict an idle resident session back to
  // cold. 'active' answers 409 — the session has work or a UI lease and the
  // monitor must not claim a release that did not happen.
  route('POST', '/omp/sessions/{sessionID}/release', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    const outcome = await engine.releaseSession({ sessionID: ctx.params.sessionID, directory });
    if (outcome === 'active') {
      return json({ error: 'session-active', released: false }, { status: 409 });
    }
    return json({ released: outcome === 'released' });
  });

  // ---- sessions ----
  // 列出目录作用域内的会话。
  route('GET', '/session', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    const sessions = await engine.listSessions({ directory });
    return json(sessions);
  });
  // 创建会话：body.directory 优先于请求推导；agent/model 显式置空交给 createSession 解构。
  route('POST', '/session', async (request, ctx) => {
    const body = await readJsonBody<SessionInitBody>(request);
    const directory = body.directory ?? projectDirectory(ctx);
    const session = await engine.createSession({
      directory,
      title: body.title,
      parentID: body.parentID,
      ...(body.parentID ? {} : {}),
      // createSession destructures the full wire shape; absent fields are
      // explicitly undefined (identical to missing keys at runtime).
      agent: undefined,
      model: undefined,
    });
    return json(session);
  });
  // 目录内全部会话的 busy/idle 状态快照。
  route('GET', '/session/status', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    return json(await engine.getSessionStatuses({ directory }));
  });
  // 读取单个会话；不存在答 404。
  route('GET', '/session/{sessionID}', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    const session = await engine.getSession({ sessionID: ctx.params.sessionID, directory });
    return session ? json(session) : notFound('session not found');
  });
  // 更新标题/元数据/归档时间；不存在答 404。
  route('PATCH', '/session/{sessionID}', async (request, ctx) => {
    const body = await readJsonBody<SessionUpdateBody>(request);
    const directory = body.directory ?? projectDirectory(ctx);
    const session = await engine.updateSession({
      sessionID: ctx.params.sessionID,
      directory,
      title: body.title,
      metadata: body.metadata,
      timeArchived: body.time?.archived,
    });
    return session ? json(session) : notFound('session not found');
  });
  // 删除会话：目录取 header/query，兜底 cwd；恒答 true。
  route('DELETE', '/session/{sessionID}', async (request, ctx) => {
    const url = ctx.url;
    const directory = directoryFromRequest(ctx) ?? url.searchParams.get('directory') ?? process.cwd();
    await engine.deleteSession({ sessionID: ctx.params.sessionID, directory });
    return json(true);
  });
  // 列出某会话的子会话（按 parentID 过滤目录会话列表）。
  route('GET', '/session/{sessionID}/children', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    const all = await engine.listSessions({ directory });
    return json(all.filter((s) => s.parentID === ctx.params.sessionID));
  });
  // 读取会话最新 TodoPhase 并投影为 wire todo。
  route('GET', '/session/{sessionID}/todo', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    return json(await engine.getTodos({ sessionID: ctx.params.sessionID, directory }));
  });
  // 分页读取消息：limit/before query，下一页游标经 x-next-cursor 响应头返回。
  route('GET', '/session/{sessionID}/message', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    const limitParam = Number(ctx.url.searchParams.get('limit'));
    const before = ctx.url.searchParams.get('before') ?? undefined;
    const page = await engine.getMessagesPage({
      sessionID: ctx.params.sessionID,
      directory,
      limit: Number.isFinite(limitParam) ? limitParam : undefined,
      before,
    });
    if (!page) return notFound('session not found');
    return page.cursor
      ? json(page.messages, { headers: { 'x-next-cursor': page.cursor } })
      : json(page.messages);
  });
  route('POST', '/session/{sessionID}/message', async (request, ctx) => {
    // Synchronous prompt variant (only consumer: gitApi small-model requests).
    const body = await readJsonBody<SessionPromptBody>(request);
    const directory = body.directory ?? projectDirectory(ctx);
    const payload = promptPayloadFromWire(body);
    const wire = await domainErrorsToResponse(() => engine.prompt({
      sessionID: ctx.params.sessionID,
      directory,
      text: payload.text,
      model: body.model,
      agent: body.agent,
      images: payload.images,
      messageID: payload.messageID,
      delivery: undefined,
    }));
    if (!wire) return notFound('session not found');
    const messages = await engine.getMessages({ sessionID: ctx.params.sessionID, directory });
    return json(messages ?? [wire]);
  });
  // 异步 prompt 主入口：解析 wire body → engine.prompt，应答 wire.info 载荷；
  // 领域错误（如 persona 404）由 domainErrorsToResponse 转为对应状态码的 Response。
  route('POST', '/session/{sessionID}/prompt_async', async (request, ctx) => {
    const body = await readJsonBody<SessionPromptBody>(request);
    const directory = body.directory ?? projectDirectory(ctx);
    const payload = promptPayloadFromWire(body);
    // engine.prompt's return is circularly inferred (it recurses for the
    // persona switch), so generic inference degrades to unknown — pin the
    // documented wire contract instead.
    const wire = await domainErrorsToResponse((): Promise<ProjectedMessage | null> => engine.prompt({
      sessionID: ctx.params.sessionID,
      directory,
      text: payload.text,
      model: body.model,
      agent: body.agent,
      images: payload.images,
      messageID: payload.messageID,
      delivery: body.delivery,
    }));
    if (!wire) return notFound('session not found');
    // Domain-error answers arrive as the Response itself; `info` narrows to
    // the payload branch (identical value to the historical untyped read).
    return json('info' in wire ? wire.info : undefined);
  });
  route('POST', '/session/{sessionID}/command', async (request, ctx) => {
    const body = await readJsonBody<SessionPromptBody>(request);
    const directory = body.directory ?? projectDirectory(ctx);
    // Slash-command execution: forward the command text as a prompt; omp
    // expands its own slash commands when the session is materialized.
    const wire = await domainErrorsToResponse(() => engine.prompt({
      sessionID: ctx.params.sessionID,
      directory,
      text: body.command ?? '',
      model: body.model,
      agent: body.agent,
      images: undefined,
      delivery: undefined,
      messageID: undefined,
    }));
  });
  route('POST', '/session/{sessionID}/abort', async (request, ctx) => {
    // Wire stop control (UI abortCurrentOperation, Esc shortcut, mobile
    // pill): 200 boolean per the vendored contract. `directory` rides the
    // query string / directory header; engine.abort resolves the live
    // session by ID (sessions are keyed by sessionID, not directory).
    const directory = directoryFromRequest(ctx);
    return json(await engine.abort({ sessionID: ctx.params.sessionID, directory: directory ?? undefined }));
  });
  // 交互式 shell 会话：omp 引擎不暴露，答 501。
  route('POST', '/session/{sessionID}/shell', async (request, ctx) => {
    return unsupported('Interactive session shells are not exposed by the omp engine.');
  });
  // 回退到指定消息：messageID 必填（缺失 400）；会话不存在答 404。
  route('POST', '/session/{sessionID}/revert', async (request, ctx) => {
    const body = await readJsonBody<SessionRevertBody>(request);
    if (!body?.messageID) return badRequest('messageID is required');
    const directory = body.directory ?? projectDirectory(ctx);
    const session = await engine.revert({
      sessionID: ctx.params.sessionID,
      directory,
      messageID: body.messageID,
    });
    return session ? json(session) : notFound('session not found');
  });
  // 撤销上一次回退；会话不存在答 404。
  route('POST', '/session/{sessionID}/unrevert', async (request, ctx) => {
    const body = await readJsonBody<SessionDirectoryBody>(request);
    const directory = body.directory ?? projectDirectory(ctx);
    const session = await engine.unrevert({ sessionID: ctx.params.sessionID, directory });
    return session ? json(session) : notFound('session not found');
  });
  // 触发会话总结后回读会话；引擎无返回时兜底空对象。
  route('POST', '/session/{sessionID}/summarize', async (request, ctx) => {
    const body = await readJsonBody<SessionDirectoryBody>(request);
    const directory = body.directory ?? projectDirectory(ctx);
    await engine.summarize({ sessionID: ctx.params.sessionID, directory });
    const session = await engine.getSession({ sessionID: ctx.params.sessionID, directory });
    return json(session ?? {});
  });
  route('POST', '/session/{sessionID}/fork', async (request, ctx) => {
    const body = await readJsonBody<SessionDirectoryBody>(request);
    const directory = body.directory ?? projectDirectory(ctx);
    // messageID (wire contract) bounds the fork at that message — TUI /branch
    // semantics. Absent → whole-transcript fork (TUI /fork semantics).
    const session = await engine.fork({
      sessionID: ctx.params.sessionID,
      directory,
      ...(typeof body.messageID === 'string' && body.messageID ? { messageID: body.messageID } : {}),
    });
    return session ? json(session) : notFound('session not found');
  });
  // 公开分享是 OpenCode 云端功能，omp 无对应实现 → 501。
  route('POST', '/session/{sessionID}/share', async () =>
    unsupported('Sharing sessions publicly is an OpenCode cloud feature with no omp equivalent.'),
  );
  // 取消分享：同上，omp 无对应实现 → 501。
  route('DELETE', '/session/{sessionID}/share', async () =>
    unsupported('Sharing sessions publicly is an OpenCode cloud feature with no omp equivalent.'),
  );
  // 会话文件 diff：omp 暂不提供，答空数组保持面板稳定。
  route('GET', '/session/{sessionID}/diff', async () => json([]));

  // ---- permissions / questions ----
  // The omp engine runs tools with its own approval policy; OMPChamber's
  // permission/question protocol has no live producer yet, so these answer
  // authoritatively empty until the approval bridge lands.
  // 权限等待队列为空（approval bridge 未落地，立场见上方组注释）。
  route('GET', '/permission', async () => json([]));
  // 权限应答：无生产者，答空对象。
  route('POST', '/permission/{requestID}/reply', async () => json({}));
  // 逐会话权限判定：一律 deny，与空队列立场一致。
  route('POST', '/api/session/{sessionID}/permission', async () => json({ id: '', effect: 'deny' }));
  // 单条权限请求查询：无挂起权限，答 404。
  route('GET', '/api/session/{sessionID}/permission/{requestID}', async () => notFound('no pending permission'));
  // 问题等待队列为空（同权限组立场）。
  route('GET', '/question', async () => json([]));
  // 问题应答：无生产者，答空对象。
  route('POST', '/question/{requestID}/reply', async () => json({}));
  // 问题拒绝：无生产者，答空对象。
  route('POST', '/question/{requestID}/reject', async () => json({}));

  // ---- config / app / commands / tools ----
  // /config 与 /global/config 同载荷（wire client 两处都会拉取）。
  route('GET', '/config', async () => json(await configPayload()));
  route('PATCH', '/config', async () => {
    // The agents write branch is gone (02 §5.8): agent definitions are the
    // omp discovery chain, managed via /api/omp/agent-definitions. The
    // legacy PATCH surface keeps answering the config payload unchanged.
    return json(await configPayload());
  });
  // provider 面板：providers 载荷 + 首个 provider 作为默认值。
  route('GET', '/config/providers', async () => {
    const providers = await providersPayload();
    return json({ providers, default: providers[0]?.id ?? '' });
  });
  route('GET', '/agent', async () => {
    await engine.ready();
    // Legacy wire face (02 §5.1 D-B1): the manufactured build/plan shells
    // keep old clients rendering; worker definitions live behind
    // /api/omp/agent-definitions and personas behind /api/omp/personas.
    return json([
      { name: 'build', description: 'General purpose coding agent', mode: 'primary', builtIn: true },
      { name: 'plan', description: 'Planning agent (read-only analysis before execution)', mode: 'primary', builtIn: true },
    ]);
  });
  route('GET', '/skill', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    try {
      // Dynamic import on purpose: a failed SDK load must degrade this one
      // request to `[]` (the catch below), not break host startup.
      const { discoverSkills } = await import('@oh-my-pi/pi-coding-agent');
      const { skills } = await discoverSkills(directory);
      return json(await wireSkillRows(skills ?? []));
    } catch {
      return json([]);
    }
  });
  route('GET', '/command', async () => json([]));
  route('GET', '/experimental/tool/ids', async () => json(BUILTIN_TOOLS ?? []));
  route('GET', '/lsp', async () => json({ servers: {} }));
  route('GET', '/formatter', async () => json({}));

  // ---- providers / auth ----
  route('GET', '/provider', async () => json(await providersPayload()));
  route('GET', '/provider/auth', async () => {
    const providers = await providersPayload();
    const models = engine.availableModels();
    const authenticated = new Set(models.map((model) => model.provider));
    const withAuth = providers.map((provider) => ({
      ...provider,
      auth: authenticated.has(provider.id),
    }));
    return json({ providers: withAuth.filter((p) => p.auth), agent: '' });
  });
  route('POST', '/provider/{providerID}/oauth/authorize', async () =>
    unsupported('Provider OAuth flows run through the omp CLI login, not the host API.'),
  );
  route('POST', '/provider/{providerID}/oauth/callback', async () =>
    unsupported('Provider OAuth flows run through the omp CLI login, not the host API.'),
  );
  route('PUT', '/auth/{providerID}', async () =>
    unsupported('API keys are managed by the omp engine credential store; run `omp` login flows.'),
  );
  route('DELETE', '/auth/{providerID}', async () =>
    unsupported('API keys are managed by the omp engine credential store; run `omp` logout.'),
  );

  // ---- mcp ----
  route('GET', '/mcp', async () => json({ servers: {} }));
  route('POST', '/mcp/{name}/connect', async () => json({}));
  route('POST', '/mcp/{name}/disconnect', async () => json({}));
  route('POST', '/mcp/{name}/auth', async () => unsupported('MCP OAuth is not bridged through the host yet.'));
  route('POST', '/mcp/{name}/auth/authenticate', async () => unsupported('MCP OAuth is not bridged through the host yet.'));
  route('POST', '/mcp/{name}/auth/callback', async () => unsupported('MCP OAuth is not bridged through the host yet.'));
  route('DELETE', '/mcp/{name}/auth', async () => json({}));

  // ---- path / project / vcs / file / find ----
  // 工作区信息：cwd + git 根/分支探测（detached 时 branch 报 'HEAD'）。
  route('GET', '/path', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    const gitRoot = await git(directory, 'rev-parse', '--show-toplevel');
    const branch = gitRoot ? await git(directory, 'branch', '--show-current') : null;
    return json({
      cwd: directory,
      git: gitRoot
        ? {
            branch: branch || 'HEAD',
            detached: !branch,
            created: Date.now(),
          }
        : undefined,
    });
  });
  // 项目列表：单一项目行（id 由引擎按目录推导；恒非 worktree）。
  route('GET', '/project', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    return json([
      {
        id: engine.projectIdFor(directory),
        directory,
        name: path.basename(directory) || directory,
        worktree: false,
      },
    ]);
  });
  // 当前项目：与 /project 同源数据的对象形态。
  route('GET', '/project/current', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    return json({
      id: engine.projectIdFor(directory),
      directory,
      name: path.basename(directory) || directory,
      worktree: false,
    });
  });
  // VCS 状态：仅当前分支；非 git 目录答空对象。
  route('GET', '/vcs', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    const branch = await git(directory, 'branch', '--show-current');
    return json(branch ? { branch } : {});
  });
  // 目录浏览：单层非隐藏条目（name/path/type/absolute）；读目录失败答 []。
  route('GET', '/file', async (request, ctx) => {
    const directory = projectDirectory(ctx);
    try {
      const entries = fs.readdirSync(directory, { withFileTypes: true });
      return json(
        entries
          .filter((entry) => !entry.name.startsWith('.'))
          .map((entry) => ({
            name: entry.name,
            path: path.join(directory, entry.name).replaceAll('\\', '/'),
            type: entry.isDirectory() ? 'directory' : 'file',
            absolute: path.join(directory, entry.name),
          })),
      );
    } catch {
      return json([]);
    }
  });
  // 读文件内容（path query 必填）：缺失 400，读不到 404。
  route('GET', '/file/content', async (request, ctx) => {
    const filePath = ctx.url.searchParams.get('path');
    if (!filePath) return badRequest('path is required');
    try {
      const content = fs.readFileSync(filePath, 'utf8');
      return json({ type: 'file', content });
    } catch {
      return notFound('file not found');
    }
  });
  // 文件 VCS 状态：omp 未接，答空 entries。
  route('GET', '/file/status', async () => json({ entries: {} }));
  // 文件名模糊搜索：深度 ≤6、命中 ≤50 的同步遍历（跳过隐藏与 node_modules）。
  route('GET', '/find/file', async (request, ctx) => {
    const query = (ctx.url.searchParams.get('pattern') ?? ctx.url.searchParams.get('query') ?? '').toLowerCase();
    const directory = projectDirectory(ctx);
    if (!query) return json({ hits: [], total: 0, processing: false });
    const hits: Array<{ type: string; name: string; path: string; score: number }> = [];
    const walk = (dir: string, depth: number) => {
      if (depth > 6 || hits.length >= 50) return;
      let entries;
      try {
        entries = fs.readdirSync(dir, { withFileTypes: true });
      } catch {
        return;
      }
      for (const entry of entries) {
        if (entry.name.startsWith('.') || entry.name === 'node_modules') continue;
        const full = path.join(dir, entry.name);
        if (entry.name.toLowerCase().includes(query)) {
          hits.push({
            path: full.replaceAll('\\', '/'),
            name: entry.name,
            type: entry.isDirectory() ? 'directory' : 'file',
            score: 1,
          });
          if (hits.length >= 50) return;
        }
        if (entry.isDirectory()) walk(full, depth + 1);
      }
    };
    walk(directory, 0);
    return json({ hits, total: hits.length, processing: false });
  });

  // ---- experimental ----
  // 实验性跨目录会话列表：默认 limit 500、按更新时间倒序；directory 参数严格
  // 过滤归属（防外目录记录污染各目录的子 store）。
  route('GET', '/experimental/session', async (request, ctx) => {
    const archived = ctx.url.searchParams.get('archived');
    const directory = ctx.url.searchParams.get('directory');
    const limit = Number(ctx.url.searchParams.get('limit') ?? 500);
    const byDirectory = await engine.listAllSessions({ archived: archived === 'false' ? false : undefined });
    let all = [...byDirectory.values()].flat();
    if (directory) {
      // Scoped callers (directory bootstrap, per-directory refresh) expect only
      // the sessions the named directory owns — the contract external OpenCode
      // runtimes honor and the proxy forwards. Answering with every
      // directory's sessions seeded foreign records into every directory child
      // store, poisoning containment-based session-directory resolution
      // (mis-addressed archive incident).
      const directoryKey = normalizeDirectoryKey(directory);
      all = all.filter((session) => normalizeDirectoryKey(session.directory ?? '') === directoryKey);
      // Subagent runs no longer join the session list (maintainer ruling:
      // sidebar stays host-sessions-only; the drill-in reads them through
      // getSession/getMessagesPage's subagent resolution instead).
    }
    all.sort((a, b) => (b.time?.updated ?? 0) - (a.time?.updated ?? 0));
    const page = Number.isFinite(limit) && limit > 0 ? all.slice(0, limit) : all;
    return json(page);
  });
  // 控制面会话迁移：sessionID 与 destination.directory 必填（缺失 400）；
  // 引擎找不到会话时答空对象。
  route('POST', '/experimental/control-plane/move-session', async (request) => {
    const body = await readJsonBody<MoveSessionBody>(request);
    const sessionID = body.sessionID;
    const destination = body.destination?.directory;
    if (!sessionID || !destination) return badRequest('sessionID and destination.directory are required');
    const moved = await engine.moveSession({ sessionID, destination });
    return json(moved ?? {});
  });
  // ---- domain modules (specs 01/02/03/04/06; public /api/omp/*) ----
  // 把引擎 ompBus.publish 适配为 domain 模块统一消费的 PublishFn。
  const ompPublish: PublishFn = (type, payload, scope) => engine.ompBus.publish(type, payload, scope);
  registerModelSettingsRoutes(route, {
    store: {
      settingsFor: async (directory) => {
        // SAFETY: a null store is the boot-degrade path; settings requests
        // then fail loudly (500) rather than masquerading as empty success.
        const store = (await engine.settingsStoreReady()) as SettingsStore;
        return store.settingsFor(directory);
      },
      getRevision: () => engine.settingsStore?.getRevision?.() ?? 0,
      bumpRevision: () => engine.settingsStore?.bumpRevision?.() ?? 0,
      chainWrites: (targetKey, task) => {
        const store = engine.settingsStore;
        return store ? store.chainWrites(targetKey, task) : Promise.resolve();
      },
      invalidateDerived: async () => {
        // SAFETY: same boot-degrade stance as settingsFor above.
        const store = (await engine.settingsStoreReady()) as SettingsStore;
        await store.invalidateDerived();
      },
      get boot() {
        // SAFETY: global writes only run after boot; the degrade path has
        // no store and PUT /omp/settings fails at settingsFor first.
        return (engine.settingsStore as SettingsStore).boot;
      },
      get bootDirectory() {
        // SAFETY: same boot-degrade stance as boot above.
        return (engine.settingsStore as SettingsStore).bootDirectory;
      },
    },
    publish: ompPublish,
    listModels: async () => {
      await engine.ready();
      return engine.availableModels();
    },
  });
  engine.dialogs.mount(route);
  registerModesDomainRoutes(route, engine.modesDomain, { features: ompFeatures() });
  engine.uriDomain.mount(route);
  // Processes domain (PLAN-session-process-monitor.md): per-session process
  // tracking over the engine's ledger.
  engine.processDomain.mount(route);
  registerCommandsDomainRoutes(route, { features: ompFeatures(), liveCommandsFor: (directory) => engine.liveCommandsFor(directory) });
  registerChromeDomainRoutes(route, { chrome: engine.chrome, features: ompFeatures() });
  registerPluginsDomainRoutes(route, {
    features: ompFeatures(),
    snapshots: () => engine.appliedPluginsSnapshots(),
    reloadSessions: (directory, sessionId) => engine.reloadAppliedPlugins(directory, sessionId),
  });
  registerProvidersDomainRoutes(route, {
    features: ompFeatures(),
    listEngineModels: () => {
      try {
        return engine.availableModels();
      } catch {
        return [];
      }
    },
    refreshModels: () => engine.refreshModels(),
  });

  // ---- omp parity foundation (spec docs/omp-parity; public paths
  // /api/omp/* — the web proxy strips the /api prefix, master D6-R3/R4) ----
  // Profile-scoped omp agent dir (spec 07 §5.13). The web server cannot
  // import the SDK (it runs under Node; the SDK is Bun/TS-only), so this is
  // the authoritative resolution point.
  route('GET', '/agent-dir', async () => json({ agentDir: getAgentDir() }));
  // omp parity 能力声明（feature flags）。
  route('GET', '/omp/capabilities', async () => json(buildCapabilities()));

  // 会话自定义消息（custom entries 投影）：directory 必填（400），无会话 404。
  route('GET', '/omp/sessions/{id}/custom-messages', async (request, ctx) => {
    const directory = directoryFromRequest({ url: new URL(request.url), headers: request.headers });
    if (!directory) return badRequest('directory is required');
    const messages = await engine.getCustomMessages({
      sessionID: ctx.params.id,
      directory,
    });
    return messages ? json(messages) : notFound('session not found');
  });

  // 会话遥测读取：directory 必填（400），无会话 404。
  route('GET', '/omp/sessions/{id}/telemetry', async (request, ctx) => {
    const directory = directoryFromRequest({ url: new URL(request.url), headers: request.headers });
    if (!directory) return badRequest('directory is required');
    const telemetry = await engine.getTelemetry({ sessionID: ctx.params.id, directory });
    return telemetry ? json(telemetry) : notFound('session not found');
  });

  // 会话原始 entry 读取：kinds query 逗号分隔过滤；directory 必填，无会话 404。
  route('GET', '/omp/sessions/{id}/entries', async (request, ctx) => {
    const url = new URL(request.url);
    const directory = directoryFromRequest({ url, headers: request.headers });
    if (!directory) return badRequest('directory is required');
    const kindsRaw = url.searchParams.get('kinds') ?? '';
    const kinds = kindsRaw ? kindsRaw.split(',').filter(Boolean) : undefined;
    const entries = await engine.getEntries({
      sessionID: ctx.params.id,
      directory,
      kinds,
    });
    return entries ? json(entries) : notFound('session not found');
  });

  // 会话级模型/思考档位切换（modelRoles.v1 特性门控；未启用答 featureUnavailable）。
  route('POST', '/omp/sessions/{id}/model', async (request, ctx) => {
    if (ompFeatures()['modelRoles.v1'] !== true) {
      return featureUnavailable('modelRoles.v1');
    }
    const directory = directoryFromRequest({ url: new URL(request.url), headers: request.headers });
    if (!directory) return badRequest('directory is required');
    const body = await readJsonBody<SessionModelBody>(request);
    const result = await engine.setSessionModel({
      sessionID: ctx.params.id,
      directory,
      model: body?.model && typeof body.model === 'object' ? body.model : undefined,
      thinkingLevel:
        typeof body?.thinkingLevel === 'string' && body.thinkingLevel.length > 0 ? body.thinkingLevel : undefined,
    });
    if (!result.ok) return badRequest(result.error ?? 'model switch failed');
    return json(result);
  });

  // '!' 本地 shell 执行（bash.v1 门控）：由会话自身 BashRunner 执行并持久化
  // bashExecution 记录，运行中投影为卡片更新。
  route('POST', '/omp/sessions/{id}/bash', async (request, ctx) => {
    if (ompFeatures()['bash.v1'] !== true) {
      return featureUnavailable('bash.v1');
    }
    const directory = directoryFromRequest({ url: new URL(request.url), headers: request.headers });
    if (!directory) return badRequest('directory is required');
    const body = await readJsonBody<{ command?: unknown; excludeFromContext?: unknown }>(request);
    const command = stringOf(body?.command)?.trim();
    if (!command) return badRequest('command is required');
    // `!` local execution (07 §3.2): the session's own BashRunner executes
    // the command and persists a bashExecution record; the engine projects
    // running → settled card updates onto the wire while it runs. `!!`
    // semantics ride `excludeFromContext` (result stays out of model context).
    const outcome = await engine.executeBash({
      sessionID: ctx.params.id,
      directory,
      command,
      excludeFromContext: body?.excludeFromContext === true,
    });
    if (outcome.status === 'notFound') return notFound('session not found');
    if (outcome.status === 'refused') return badRequest(outcome.error);
    return json({ ok: true, message: outcome.message, result: outcome.result });
  });

  route('GET', '/omp/events', async (request) => {
    // Single omp-native event channel (05 §5.2.1, master R1). Same frame
    // format as the wire SSE; Last-Event-ID resumes durable entries only,
    // with an omp.stream.resync control frame first when the ring can't
    // bridge the gap (断流不是空状态, master D2).
    const url = new URL(request.url);
    const rawDirectory = url.searchParams.get('directory');
    const directory = rawDirectory ? normalizeDirectoryKey(rawDirectory) : null;
    const bus = engine.ompBus;
    const lastEventId = Number(request.headers.get('last-event-id') ?? 0) || 0;
    const resyncPlan = planResume(bus, lastEventId, request.headers.get('x-omp-epoch'));
    return startSseStream(request, bus.epoch, (send) => {
      // omp boot identity rides an envelope-shaped frame (the native client
      // parses by event name); no SSE `id` line, never in the replay ring.
      send(`event: omp.stream.boot\ndata: ${JSON.stringify({ id: 0, type: 'omp.stream.boot', directory: directory ?? '', schemaVersion: bus.schemaVersion, createdAt: Date.now(), payload: { epoch: bus.epoch } })}\n\n`);
      if (resyncPlan.resync) {
        const reason = resyncPlan.state.status === 'ok' ? 'epoch' : resyncPlan.state.status;
        const envelope = {
          id: resyncPlan.baseline,
          type: 'omp.stream.resync',
          directory: directory ?? '',
          schemaVersion: bus.schemaVersion,
          createdAt: Date.now(),
          payload: {
            scope: ['sessions', 'modes', 'model', 'dialogs', 'chrome', 'settings', 'agents', 'jobs', 'queue', 'tree', 'transcript'],
            lastEventId,
            // Reconnectable tail (0 = fresh): never the phantom next id —
            // an id that never entered the ring makes every reconnect
            // re-trigger resync (plan §5.2).
            resumeFrom: resyncPlan.baseline,
            reason,
          },
        };
        send(`event: omp.stream.resync\nid: ${resyncPlan.baseline}\ndata: ${JSON.stringify(envelope)}\n\n`);
      }
      return bus.subscribeSince(
        resyncPlan.baseline,
        (entry) => {
          send(`id: ${entry.eventId}\nevent: ${entry.envelope.type}\ndata: ${JSON.stringify(entry.envelope)}\n\n`);
        },
        directory ? { directory } : {},
      );
    });
  });

  // ---- SSE ----
  // wire SSE handler（/event 与 /global/event 共用）：global 时不按目录过滤。
  const sseHandler = (request: Request, { global }: { global: boolean }) => {
    const bus = engine.bus;
    const directory = global ? null : directoryFromRequest({ url: new URL(request.url), headers: request.headers });
    const lastEventId = Number(request.headers.get('last-event-id') ?? 0) || 0;
    const resyncPlan = planResume(bus, lastEventId, request.headers.get('x-omp-epoch'));
    return startSseStream(request, bus.epoch, (send) => {
      // In-band boot identity, data-bearing so the generated SSE client
      // surfaces it through onSseEvent. Never carries an `id` (must not
      // move the cursor) and never enters the replay ring.
      send(`event: omp.stream.boot\ndata: ${JSON.stringify({ epoch: bus.epoch })}\n\n`);
      if (resyncPlan.resync) {
        // Data-less in-band wire control (plan §5.2): the `id` line is the
        // reconnectable tail — 0 clears stale cursors when the ring is empty.
        send(`event: omp.stream.resync\nid: ${resyncPlan.baseline}\n\n`);
      }
      return bus.subscribeSince(
        resyncPlan.baseline,
        (entry) => {
          send(`id: ${entry.eventId}\nevent: ${entry.envelope.type}\ndata: ${JSON.stringify(entry.envelope)}\n\n`);
        },
        directory ? { directory } : {},
      );
    });
  };

  return { sseHandler };
};
