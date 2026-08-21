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

import { featureUnavailable, ompFeatures } from './omp-parity.js';
import { normalizeDirectoryKey } from './registry.js';

const json = (data, init) => Response.json(data, init);

/** SDK widget line cap (09 §5.1; extensions self-cap, this is defensive). */
const MAX_WIDGET_LINES = 10;

const PLACEMENTS = new Set(['aboveEditor', 'belowEditor']);

const normalizeLines = (lines) => {
  if (lines === undefined) return undefined;
  if (!Array.isArray(lines)) return null;
  const rows = [];
  for (const row of lines.slice(0, MAX_WIDGET_LINES)) {
    if (typeof row !== 'string') return null;
    rows.push(row);
  }
  return rows;
};

const validKey = (key) => typeof key === 'string' && key.length > 0;

/**
 * @param {{
 *   publishFor?: (directory: string, payload: Record<string, unknown>) => void,
 *   now?: () => number,
 * }} [options]
 */
export const createDomainChrome = ({ publishFor, now = () => Date.now() } = {}) => {
  // Directory keys are normalized at every entry point (same contract as
  // domain-dialogs): the web proxy canonicalizes query directories via
  // realpath (backslashes on Windows) while extension contexts carry the
  // session's forward-slash key — one canonical form for the table.
  const dirKey = (directory) => normalizeDirectoryKey(directory);

  /** @type {Map<string, {widgets: Map<string, object>, status: Map<string, object>, dropped: Map<string, number>, revision: number}>} */
  const directories = new Map();

  const sliceFor = (directory) => {
    let slice = directories.get(dirKey(directory));
    if (!slice) {
      slice = { widgets: new Map(), status: new Map(), dropped: new Map(), revision: 0 };
      directories.set(dirKey(directory), slice);
    }
    return slice;
  };

  const publish = (directory, payload) => {
    try {
      publishFor?.(directory, payload);
    } catch {
      // A publish failure must not break the extension call path.
    }
  };

  /** Bridge-facing handlers for one session (stable per call; the dialog
   *  bridge caches its instance for the session lifetime). */
  const bridgeHandlersFor = (directory, sessionId) => ({
    setWidget: (key, content, options) => {
      // R-E3: component factories are TUI-bound — count, don't render.
      if (content !== undefined && !Array.isArray(content)) {
        noteDropped(directory, 'setWidget.factory');
        return;
      }
      setWidget(directory, sessionId, key, content, options?.placement);
    },
    setStatus: (key, text) => {
      if (text !== undefined && typeof text !== 'string') {
        noteDropped(directory, 'setStatus.invalid');
        return;
      }
      setStatus(directory, sessionId, key, text);
    },
    noteDropped: (method) => noteDropped(directory, method),
  });

  const setWidget = (directory, sessionId, key, content, placement) => {
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
      kind: 'widget',
      key,
      ...(cleared ? {} : { lines, ...(PLACEMENTS.has(placement) ? { placement } : {}) }),
    });
  };

  const setStatus = (directory, sessionId, key, text) => {
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
    publish(directory, { kind: 'status', key, ...(cleared ? {} : { text }) });
  };

  const noteDropped = (directory, method) => {
    if (typeof method !== 'string' || !method) return;
    const slice = sliceFor(dirKey(directory));
    slice.dropped.set(method, (slice.dropped.get(method) ?? 0) + 1);
    // No event: dropped counts are diagnostic state, consumed via snapshot.
  };

  const snapshot = (directory) => {
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

/**
 * Mount `GET /omp/chrome` (public path /api/omp/chrome). Capability
 * `extensionChrome.v1` gates the endpoint (501 when off, mirroring
 * commands.v1); the table itself keeps recording either way so flipping the
 * key back on needs no re-warm.
 */

export const registerChromeDomainRoutes = (route, { chrome, features = ompFeatures() } = {}) => {
  route('GET', '/omp/chrome', async (request) => {
    if (features?.['extensionChrome.v1'] !== true) return featureUnavailable('extensionChrome.v1');
    const url = new URL(request.url);
    const directory = url.searchParams.get('directory');
    if (!directory) return json({ error: 'directory is required' }, { status: 400 });
    return json(chrome.snapshot(directory));
  });
};
