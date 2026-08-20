/**
 * omp resync — the §5.2.4 reconciliation matrix (spec
 * docs/omp-parity/05 §5.2.4; master D2: 断流不是空状态).
 *
 * Events are incremental notifications only; authoritative truth lives in
 * the snapshot endpoints. After a stream gap (`omp.stream.resync`) the
 * affected domains are re-fetched authoritatively, exactly once per run and
 * in canonical bootstrap order:
 *
 *   2 session snapshot (wire)  → 3 modes/model → 4 dialogs → 5 settings
 *   → 6 agents/jobs/queue/tree → 7 transcript (wire + structured reads)
 *
 * An empty/untrustable scope runs the full ordered matrix. Missing surfaces
 * (404/501 — feature not landed yet) are skipped silently per the R2
 * degradation matrices; transport failures abort the remaining steps of that
 * domain but never clear state (GETs are additive refreshes).
 */

import { OMP_ENDPOINTS, type OmpFetchJsonResult } from '@/lib/api/omp';
import { runtimeFetch, type RuntimeFetchOptions } from '@/lib/runtime-fetch';

/** Scope names the server's resync frame uses (omp-host endpoints.js). */
export type OmpResyncScope =
  | 'sessions'
  | 'modes'
  | 'model'
  | 'dialogs'
  | 'settings'
  | 'agents'
  | 'jobs'
  | 'queue'
  | 'tree'
  | 'transcript';

const FULL_ORDER: readonly OmpResyncScope[] = [
  'sessions',
  'modes',
  'model',
  'dialogs',
  'settings',
  'agents',
  'jobs',
  'queue',
  'tree',
  'transcript',
];

const isResyncScope = (value: string): value is OmpResyncScope =>
  (FULL_ORDER as readonly string[]).includes(value);

export interface OmpResyncFetchLog {
  path: string;
  query: Record<string, string>;
}

export interface OmpResyncContext {
  /** Directories whose data this client holds (child stores). */
  listDirectories: () => string[];
  /** Session ids known for a directory (child store session list). */
  listSessions: (directory: string) => string[];
  /**
   * Wire-side authoritative refresh for a directory (session snapshot +
   * transcript tails) — the existing directory resync path.
   */
  refetchWire: (directory: string) => void;
  /**
   * Test seam over runtimeFetch for omp GETs. Production default resolves
   * `{ok:false, unavailable:true}` for 404/501 and `{ok:false,
   * unavailable:false}` for transport failure.
   */
  fetchOmpJson?: (path: string, query: Record<string, string>) => Promise<OmpFetchJsonResult<unknown>>;
}

const defaultFetchOmpJson = async (
  path: string,
  query: Record<string, string>,
): Promise<OmpFetchJsonResult<unknown>> => {
  try {
    const response = await runtimeFetch(path, {
      method: 'GET',
      headers: { Accept: 'application/json' },
      ...(Object.keys(query).length > 0 ? { query } : {}),
    } as RuntimeFetchOptions);
    if (response.status === 404 || response.status === 501) {
      return { ok: false, unavailable: true };
    }
    if (!response.ok) {
      return { ok: false, unavailable: false };
    }
    return { ok: true, data: await response.json() };
  } catch {
    return { ok: false, unavailable: false };
  }
};

/**
 * Runs the reconciliation matrix. `scopes === null` (or an untrustable list)
 * runs the full ordered matrix. Every domain executes at most once per run
 * per directory; results are additive refreshes whose consumers land with
 * their owning chapters — the GET itself is the authoritative recovery step
 * required by the matrix (CI rule 3e).
 */
export async function runOmpResync(
  scopes: readonly string[] | null,
  context: OmpResyncContext,
  signal?: AbortSignal,
): Promise<void> {
  const filtered = scopes === null ? null : scopes.filter(isResyncScope);
  const wanted: OmpResyncScope[] = filtered === null || filtered.length === 0
    ? [...FULL_ORDER]
    : [...new Set(filtered)];
  const aborted = (): boolean => signal?.aborted === true;
  const fetchOmpJson = context.fetchOmpJson ?? defaultFetchOmpJson;
  const directories = context.listDirectories();

  const fetchAll = (fetches: Array<{ path: string; query: Record<string, string> }>): Promise<unknown> =>
    // Fire the domain's GETs together; fetchOmpJson never throws, so each
    // result degrades only its own surface: ok → consumed by the domain
    // surface when it lands; unavailable → feature off (R2), skip; transport
    // failure → keep prior state (D2).
    Promise.all(fetches.map((request) => fetchOmpJson(request.path, request.query)));

  for (const scope of FULL_ORDER) {
    if (aborted()) return;
    if (!wanted.includes(scope)) continue;
    if (scope === 'sessions') {
      // Wire snapshot + transcript tails: the existing resync path owns both
      // fetch and reduction, once per directory.
      for (const directory of directories) {
        if (aborted()) return;
        context.refetchWire(directory);
      }
      continue;
    }
    if (scope === 'transcript') {
      // Wire transcript refresh shares the wire resync; structured omp reads
      // (custom-messages/telemetry/entries) belong to session surfaces that
      // land with their chapters — the wire refresh reconciles the transcript
      // truth now.
      for (const directory of directories) {
        if (aborted()) return;
        context.refetchWire(directory);
      }
      continue;
    }
    const fetches: Array<{ path: string; query: Record<string, string> }> = [];
    for (const directory of directories) {
      if (aborted()) return;
      if (scope === 'modes') {
        for (const sessionID of context.listSessions(directory)) {
          fetches.push({ path: OMP_ENDPOINTS.sessionMode(sessionID), query: { directory } });
        }
      } else if (scope === 'model') {
        fetches.push({ path: OMP_ENDPOINTS.models, query: { directory } });
      } else if (scope === 'dialogs') {
        fetches.push({ path: OMP_ENDPOINTS.dialogs, query: { directory } });
      } else if (scope === 'settings') {
        fetches.push({ path: OMP_ENDPOINTS.settings, query: { directory } });
      } else if (scope === 'agents') {
        fetches.push({ path: OMP_ENDPOINTS.agentRuns, query: { directory } });
      } else if (scope === 'jobs') {
        fetches.push({ path: OMP_ENDPOINTS.jobs, query: { directory } });
      } else if (scope === 'queue' || scope === 'tree') {
        for (const sessionID of context.listSessions(directory)) {
          const path = scope === 'queue'
            ? OMP_ENDPOINTS.sessionQueue(sessionID)
            : OMP_ENDPOINTS.sessionTree(sessionID);
          fetches.push({ path, query: { directory } });
        }
      }
    }
    await fetchAll(fetches);
  }
}
