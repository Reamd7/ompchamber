// Domain module: omp-parity chapter 09 §5.0-5.2 (extension host surfaces),
// server side.
//
// The extension chrome table mirrors the SDK's official host contract
// `RpcExtensionUIRequest` (rpc-types.d.ts:591-665; rpc-mode.ts:798-882):
// string-payload chrome — setWidget {widgetKey, widgetLines, widgetPlacement}
// and setStatus {statusKey, statusText} — is host surface per omp's own RPC
// semantics. The web dialog bridge (domain-dialogs.js D-C1) receives these
// calls from extensions and delegates here instead of dropping them.
//
// Semantics (09 §3 rulings):
//  - R-E1 official contract: field names and the undefined-clears meaning
//    follow RpcExtensionUIRequest verbatim.
//  - R-E2 passive surfaces are NOT lease-gated: dialogs fail closed without a
//    lease (03 D-C1) because nobody could answer them; chrome is pure display
//    and background sessions stay legitimate writers.
//  - R-E3 observable drops: component-factory payloads (and other TUI-bound
//    members) are counted per method and surfaced through the snapshot's
//    `dropped` section — never silently, never rendered.
//
// State is last-writer-wins per (directory, key); the snapshot carries the
// originating sessionId so the UI can label provenance when it matters.
// Every mutation publishes `omp.chrome.updated` (volatile; the snapshot GET
// is the reconnect authority — D2: a failed refetch freezes, never clears).
//
// SELF-CONTAINED BY CONTRACT: no engine.js/endpoints.js imports; the
// coordinator mounts `registerChromeDomainRoutes(route, { chrome, features })`.
/**
 * 扩展宿主 chrome 域（omp-parity 第 09 章 §5.0-5.2），服务端实现。
 *
 * chrome 表镜像 SDK 官方宿主契约 RpcExtensionUIRequest
 * （rpc-types.d.ts:591-665；rpc-mode.ts:798-882）：字符串 payload 的
 * setWidget {widgetKey, widgetLines, widgetPlacement} 与 setStatus
 * {statusKey, statusText} 按 omp 自身 RPC 语义属宿主表面；web 侧对话桥
 * （domain-dialogs.js D-C1）收到这些调用后委托到本模块而非丢弃。
 *
 * 关键裁决（09 §3）：R-E1 字段名与 undefined-清除语义逐字遵循官方契约；
 * R-E2 被动表面不做 lease 门控（后台会话仍是合法写者）；R-E3 可观测丢弃
 * ——component-factory 等 TUI 专属 payload 按方法计数并经快照 dropped 段
 * 暴露，绝不渲染。状态按（目录, key）后写者胜；每次变更发布
 * omp.chrome.updated（volatile，快照 GET 才是重连权威——D2）。
 * 契约上自包含：不 import engine.js/endpoints.js，由协调器挂载路由。
 */

import type { ExtensionWidgetContent } from '@oh-my-pi/pi-coding-agent/extensibility/extensions';
import { featureUnavailable, ompFeatures } from './omp-parity.ts';
import { normalizeDirectoryKey } from './registry.ts';

/** Response.json 直通的 JSON 响应构造器（平台持有的 passthrough）。 */
const json = <T,>(data: T, init?: ResponseInit) => Response.json(data, init);

/** SDK 的 widget 行数上限（09 §5.1；扩展本应自限，这里是防御）。 */
/** SDK widget line cap (09 §5.1; extensions self-cap, this is defensive). */
const MAX_WIDGET_LINES = 10;

/** 合法的 widget 摆放位置集合（aboveEditor/belowEditor）。 */
const PLACEMENTS = new Set(['aboveEditor', 'belowEditor']);

/** 归一化 widget 行数组：undefined 原样透传（清除信号）；非数组或含
 *  非字符串行返回 null（非法输入）；否则截断到 MAX_WIDGET_LINES。 */
const normalizeLines = (lines: readonly unknown[] | undefined): string[] | null | undefined => {
  if (lines === undefined) return undefined;
  if (!Array.isArray(lines)) return null;
  const rows: string[] = [];
  for (const row of lines.slice(0, MAX_WIDGET_LINES)) {
    if (typeof row !== 'string') return null;
    rows.push(row);
  }
  return rows;
};

/** key 必须是非空字符串才可作为 chrome 表的键。 */
const validKey = (key: string): boolean => typeof key === 'string' && key.length > 0;

/** createDomainChrome 的依赖注入：目录级事件发布与时钟。 */
export interface DomainChromeDeps {
  /** 每次表面变更时按目录回调（发布 omp.chrome.updated）。 */
  publishFor?: (directory: string, payload: ChromeUpdatedPayload) => void;
  /** 时间戳来源；默认 Date.now，测试可注入。 */
  now?: () => number;
}

/** 单个目录的 chrome 状态切片：widget/status/丢弃计数与 revision。 */
interface ChromeDirectorySlice {
  /** widgetKey → widget 行。 */
  widgets: Map<string, ChromeWidgetRow>;
  /** statusKey → status 行。 */
  status: Map<string, ChromeStatusRow>;
  /** 被丢弃方法名 → 计数（R-E3 可观测丢弃）。 */
  dropped: Map<string, number>;
  /** 本目录切片的修订号，任何变更单调递增。 */
  revision: number;
}

/** 快照中的 widget 行：内容、摆放与来源会话。 */
export interface ChromeWidgetRow {
  /** 扩展自定的 widget key。 */
  key: string;
  /** 文本行（至多 MAX_WIDGET_LINES 行）。 */
  lines: string[];
  /** 摆放位置（仅合法值会被保留）。 */
  placement?: string;
  /** 最后写入该 key 的会话 id（溯源标签）。 */
  sessionId: string;
  /** 最后更新时间戳（deps.now）。 */
  updatedAt: number;
}

/** 快照中的 status 行：单行文本状态。 */
export interface ChromeStatusRow {
  /** 扩展自定的 status key。 */
  key: string;
  /** 状态文本。 */
  text: string;
  /** 最后写入该 key 的会话 id。 */
  sessionId: string;
  /** 最后更新时间戳（deps.now）。 */
  updatedAt: number;
}

/** GET /omp/chrome 的目录快照：revision + widgets + status + dropped。 */
export interface ChromeSnapshot {
  /** 目录切片修订号（未知目录为 0）。 */
  revision: number;
  /** 当前 widget 行列表。 */
  widgets: ChromeWidgetRow[];
  /** 当前 status 行列表。 */
  status: ChromeStatusRow[];
  /** 被丢弃方法名 → 累计计数。 */
  dropped: Record<string, number>;
}
/** 扩展可经 bridge 推送的任意 JSON 值。 */
/** JSON value an extension may push over the bridge. */
type JsonValue = string | number | boolean | null | JsonValue[] | { [key: string]: JsonValue };

/** 结构化（非字符串）值出现在 statusText（契约类型为 string | undefined）
 *  时的载体——绝不渲染，计为 setStatus.invalid 丢弃（09 R-E3）。 */
/** Structured (non-string) value an extension may push where the
 *  RpcExtensionUIRequest contract carries `statusText: string | undefined` —
 *  never rendered; counted as a `setStatus.invalid` drop (09 R-E3). */
export type ChromeStructuredWireValue = Record<string, JsonValue>;

/** omp.chrome.updated 事件 payload（volatile；快照 GET 才是重连权威 D2）：
 *  变更的表面成员 kind + key，附新的 lines/placement（widget）或 text
 *  （status）；被清除的成员只带 kind + key。 */
/** omp.chrome.updated event payload (volatile; the snapshot GET is the
 *  reconnect authority, D2): the changed surface member — kind + key, plus
 *  the new lines/placement (widget) or text (status); cleared members carry
 *  only kind + key. */
export interface ChromeUpdatedPayload {
  kind: 'widget' | 'status';
  key: string;
  lines?: string[];
  placement?: string;
  text?: string;
}

/** 桥接面：dialog bridge 拿到的每会话 handler 集合。 */
export interface ChromeBridgeHandlers {
  /** 写入/清除一个 widget（字符串行）；factory payload 计为丢弃。 */
  setWidget: (key: string, content: ExtensionWidgetContent, options?: { placement?: string }) => void;
  /** 写入/清除一行状态文本；非字符串 payload 计为丢弃。 */
  setStatus: (key: string, text: string | ChromeStructuredWireValue | undefined) => void;
  /** 显式登记一次可观测丢弃（按方法名计数）。 */
  noteDropped: (method: string) => void;
}

/** chrome 域的公开 API：桥接 handler、写入/查询与目录释放。 */
export interface DomainChrome {
  /** 取某会话的桥接 handler 集合（dialog bridge 按会话缓存）。 */
  bridgeHandlersFor: (directory: string, sessionId: string) => ChromeBridgeHandlers;
  /** 写入/清除一个目录的 widget（undefined 或空数组即清除）。 */
  setWidget: (
    directory: string,
    sessionId: string,
    key: string,
    content: readonly unknown[] | undefined,
    placement?: string,
  ) => void;
  /** 写入/清除一个目录的状态文本（undefined 或空串即清除）。 */
  setStatus: (directory: string, sessionId: string, key: string, text: string | undefined) => void;
  /** 登记一次可观测丢弃（按方法名计数，不发布事件）。 */
  noteDropped: (directory: string, method: string) => void;
  /** 读取目录快照（未知目录返回空快照而非错误）。 */
  snapshot: (directory: string) => ChromeSnapshot;
  /** 释放一个目录的切片（见下方英文契约：仅当目录最后一个 live 会话
   *  离开时由引擎调用，防止表跨所有打开过的项目累积）。 */
  /**
   * Drop one directory's slice (docs/plan.md §6): the engine calls this when
   * the directory's last live session leaves, so per-directory widget/status
   * tables cannot accumulate across every project ever opened. A directory
   * with a remaining live session must never be released by the caller.
   */
  releaseDirectory: (directory: string) => void;
}

/** 创建 chrome 域实例：目录键统一走 normalizeDirectoryKey，每个表面
 *  变更递增 revision 并经 publishFor 发布（发布失败不影响写入路径）。 */
/**
 * @param {{
 *   publishFor?: (directory: string, payload: ChromeUpdatedPayload) => void,
 *   now?: () => number,
 * }} [options]
 */
export const createDomainChrome = ({ publishFor, now = () => Date.now() }: DomainChromeDeps = {}): DomainChrome => {
  // Directory keys are normalized at every entry point (same contract as
  // domain-dialogs): the web proxy canonicalizes query directories via
  // realpath (backslashes on Windows) while extension contexts carry the
  // session's forward-slash key — one canonical form for the table.
  const dirKey = (directory: string) => normalizeDirectoryKey(directory);

  /** 目录键 → 该目录的状态切片（惰性创建，见 sliceFor）。 */
  /** @type {Map<string, {widgets: Map<string, object>, status: Map<string, object>, dropped: Map<string, number>, revision: number}>} */
  const directories = new Map<string, ChromeDirectorySlice>();

  /** 取目录切片，不存在则创建空切片并登记（惰性初始化）。 */
  const sliceFor = (directory: string) => {
    let slice = directories.get(dirKey(directory));
    if (!slice) {
      slice = { widgets: new Map(), status: new Map(), dropped: new Map(), revision: 0 };
      directories.set(dirKey(directory), slice);
    }
    return slice;
  };

  /** 发布目录级 omp.chrome.updated；发布方抛错被吞掉以保护扩展调用路径。 */
  const publish = (directory: string, payload: ChromeUpdatedPayload) => {
    try {
      publishFor?.(directory, payload);
    } catch {
      // A publish failure must not break the extension call path.
    }
  };

  /** 面向 bridge 的每会话 handler：数组/undefined 判定后转发到域级
   *  写入函数，其余 payload 按方法名记为丢弃。 */
  /** Bridge-facing handlers for one session (stable per call; the dialog
   *  bridge caches its instance for the session lifetime). */
  const bridgeHandlersFor = (directory: string, sessionId: string): ChromeBridgeHandlers => ({
    setWidget: (key: string, content: ExtensionWidgetContent, options?: { placement?: string }) => {
      // strict:false disables null/undefined equality narrowing, so narrow
      // via Array.isArray/typeof and forward literal undefined instead.
      if (Array.isArray(content)) {
        setWidget(directory, sessionId, key, content, options?.placement);
      } else if (content === undefined) {
        setWidget(directory, sessionId, key, undefined, options?.placement);
      } else {
        // R-E3: component factories are TUI-bound — count, don't render.
        noteDropped(directory, 'setWidget.factory');
      }
    },
    setStatus: (key: string, text: string | ChromeStructuredWireValue | undefined) => {
      if (typeof text === 'string') {
        setStatus(directory, sessionId, key, text);
      } else if (text === undefined) {
        setStatus(directory, sessionId, key, undefined);
      } else {
        noteDropped(directory, 'setStatus.invalid');
      }
    },
    noteDropped: (method: string) => noteDropped(directory, method),
  });

  /** 域级 widget 写入：非法 key 或非法行直接忽略；undefined/空数组视为
   *  清除（不存在的 key 清除是 no-op）；写后 revision+1 并发布事件。 */
  const setWidget = (directory: string, sessionId: string, key: string, content: readonly unknown[] | undefined, placement?: string) => {
    if (!validKey(key)) return;
    const lines = normalizeLines(content);
    if (lines === null) return;
    const slice = sliceFor(dirKey(directory));
    const cleared = lines === undefined || lines.length === 0;
    if (cleared) {
      if (!slice.widgets.has(key)) return;
      slice.widgets.delete(key);
    } else {
      slice.widgets.set(key, {
        key,
        lines,
        ...(placement !== undefined && PLACEMENTS.has(placement) ? { placement } : {}),
        sessionId,
        updatedAt: now(),
      });
    }
    slice.revision += 1;
    publish(directory, {
      kind: 'widget' as const,
      key,
      ...(cleared ? {} : { lines, ...(placement !== undefined && PLACEMENTS.has(placement) ? { placement } : {}) }),
    });
  };

  /** 域级状态文本写入：undefined/空串视为清除；写后 revision+1 并发布。 */
  const setStatus = (directory: string, sessionId: string, key: string, text: string | undefined) => {
    if (!validKey(key)) return;
    const slice = sliceFor(dirKey(directory));
    const cleared = text === undefined || text === '';
    if (cleared) {
      if (!slice.status.has(key)) return;
      slice.status.delete(key);
    } else {
      slice.status.set(key, { key, text, sessionId, updatedAt: now() });
    }
    slice.revision += 1;
    publish(directory, { kind: 'status' as const, key, ...(cleared ? {} : { text }) });
  };

  /** 按方法名累计一次丢弃；纯诊断状态，不发布事件，经快照读取。 */
  const noteDropped = (directory: string, method: string) => {
    if (typeof method !== 'string' || !method) return;
    const slice = sliceFor(dirKey(directory));
    slice.dropped.set(method, (slice.dropped.get(method) ?? 0) + 1);
    // No event: dropped counts are diagnostic state, consumed via snapshot.
  };

  /** 读取目录快照；未知目录返回 revision 0 的空快照。 */
  const snapshot = (directory: string): ChromeSnapshot => {
    const slice = directories.get(dirKey(directory));
    if (!slice) return { revision: 0, widgets: [], status: [], dropped: {} };
    return {
      revision: slice.revision,
      widgets: [...slice.widgets.values()],
      status: [...slice.status.values()],
      dropped: Object.fromEntries(slice.dropped),
    };
  };

  /** 丢弃整个目录切片（引擎在目录最后一个 live 会话离开时调用）。 */
  const releaseDirectory = (directory: string) => {
    directories.delete(dirKey(directory));
  };

  return { bridgeHandlersFor, setWidget, setStatus, noteDropped, snapshot, releaseDirectory };
};

/** registerChromeDomainRoutes 的挂载选项：chrome 实例与 feature 开关。 */
export interface ChromeRouteMountOptions {
  /** chrome 域实例；缺省时路由对请求返回 400。 */
  chrome?: DomainChrome;
  /** feature 键值表；默认取 ompFeatures()。 */
  features?: Record<string, boolean>;
}

/** 路由挂载函数类型：（method, pattern, handler）三元组注册。 */
type ChromeRouteMount = (
  method: string,
  pattern: string,
  handler: (request: Request) => Response | Promise<Response>,
) => void;

/** 挂载 GET /omp/chrome（公开路径 /api/omp/chrome）。由 capability
 *  `extensionChrome.v1` 门控（关闭时 501，与 commands.v1 一致）；但
 *  chrome 表本身无论开关都在记录，重新打开 key 无需预热。 */
/**
 * Mount `GET /omp/chrome` (public path /api/omp/chrome). Capability
 * `extensionChrome.v1` gates the endpoint (501 when off, mirroring
 * commands.v1); the table itself keeps recording either way so flipping the
 * key back on needs no re-warm.
 */

export const registerChromeDomainRoutes = (
  route: ChromeRouteMount,
  { chrome, features = ompFeatures() }: ChromeRouteMountOptions = {},
) => {
  route('GET', '/omp/chrome', async (request) => {
    if (features?.['extensionChrome.v1'] !== true) return featureUnavailable('extensionChrome.v1');
    const url = new URL(request.url);
    const directory = url.searchParams.get('directory');
    if (!directory || !chrome) return json({ error: 'directory is required' }, { status: 400 });
    return json(chrome.snapshot(directory));
  });
};
