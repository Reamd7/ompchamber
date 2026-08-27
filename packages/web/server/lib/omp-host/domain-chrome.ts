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

import type { ExtensionWidgetContent } from '@oh-my-pi/pi-coding-agent/extensibility/extensions';
import { featureUnavailable, ompFeatures } from './omp-parity.ts';
import { normalizeDirectoryKey } from './registry.ts';

const json = <T,>(data: T, init?: ResponseInit) => Response.json(data, init);

/** SDK widget line cap (09 §5.1; extensions self-cap, this is defensive). */
const MAX_WIDGET_LINES = 10;

const PLACEMENTS = new Set(['aboveEditor', 'belowEditor']);

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

const validKey = (key: string): boolean => typeof key === 'string' && key.length > 0;

export interface DomainChromeDeps {
  publishFor?: (directory: string, payload: ChromeUpdatedPayload) => void;
  now?: () => number;
}

interface ChromeDirectorySlice {
  widgets: Map<string, ChromeWidgetRow>;
  status: Map<string, ChromeStatusRow>;
  dropped: Map<string, number>;
  revision: number;
}

export interface ChromeWidgetRow {
  key: string;
  lines: string[];
  placement?: string;
  sessionId: string;
  updatedAt: number;
}

export interface ChromeStatusRow {
  key: string;
  text: string;
  sessionId: string;
  updatedAt: number;
}

export interface ChromeSnapshot {
  revision: number;
  widgets: ChromeWidgetRow[];
  status: ChromeStatusRow[];
  dropped: Record<string, number>;
}
/** JSON value an extension may push over the bridge. */
type JsonValue = string | number | boolean | null | JsonValue[] | { [key: string]: JsonValue };

/** Structured (non-string) value an extension may push where the
 *  RpcExtensionUIRequest contract carries `statusText: string | undefined` —
 *  never rendered; counted as a `setStatus.invalid` drop (09 R-E3). */
export type ChromeStructuredWireValue = Record<string, JsonValue>;

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

export interface ChromeBridgeHandlers {
  setWidget: (key: string, content: ExtensionWidgetContent, options?: { placement?: string }) => void;
  setStatus: (key: string, text: string | ChromeStructuredWireValue | undefined) => void;
  noteDropped: (method: string) => void;
}

export interface DomainChrome {
  bridgeHandlersFor: (directory: string, sessionId: string) => ChromeBridgeHandlers;
  setWidget: (
    directory: string,
    sessionId: string,
    key: string,
    content: readonly unknown[] | undefined,
    placement?: string,
  ) => void;
  setStatus: (directory: string, sessionId: string, key: string, text: string | undefined) => void;
  noteDropped: (directory: string, method: string) => void;
  snapshot: (directory: string) => ChromeSnapshot;
}

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

  /** @type {Map<string, {widgets: Map<string, object>, status: Map<string, object>, dropped: Map<string, number>, revision: number}>} */
  const directories = new Map<string, ChromeDirectorySlice>();

  const sliceFor = (directory: string) => {
    let slice = directories.get(dirKey(directory));
    if (!slice) {
      slice = { widgets: new Map(), status: new Map(), dropped: new Map(), revision: 0 };
      directories.set(dirKey(directory), slice);
    }
    return slice;
  };

  const publish = (directory: string, payload: ChromeUpdatedPayload) => {
    try {
      publishFor?.(directory, payload);
    } catch {
      // A publish failure must not break the extension call path.
    }
  };

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
        ...(PLACEMENTS.has(placement) ? { placement } : {}),
        sessionId,
        updatedAt: now(),
      });
    }
    slice.revision += 1;
    publish(directory, {
      kind: 'widget' as const,
      key,
      ...(cleared ? {} : { lines, ...(PLACEMENTS.has(placement) ? { placement } : {}) }),
    });
  };

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

  const noteDropped = (directory: string, method: string) => {
    if (typeof method !== 'string' || !method) return;
    const slice = sliceFor(dirKey(directory));
    slice.dropped.set(method, (slice.dropped.get(method) ?? 0) + 1);
    // No event: dropped counts are diagnostic state, consumed via snapshot.
  };

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

  return { bridgeHandlersFor, setWidget, setStatus, noteDropped, snapshot };
};

export interface ChromeRouteMountOptions {
  chrome?: DomainChrome;
  features?: Record<string, boolean>;
}

type ChromeRouteMount = (
  method: string,
  pattern: string,
  handler: (request: Request) => Response | Promise<Response>,
) => void;

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
    if (!directory) return json({ error: 'directory is required' }, { status: 400 });
    return json(chrome.snapshot(directory));
  });
};
