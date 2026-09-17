import { createOmpTreeAPI, type OmpFetchJsonResult, type OmpSessionTreeSnapshot } from '@/lib/api/omp';

const treeApi = createOmpTreeAPI();

/**
 * Ancestor session ids of `sessionId` inside its fork tree. Forks inherit
 * the artifacts directory of the session they forked from, so a run's
 * agent-runs row is keyed to an ancestor session while its task card lives
 * in the fork's transcript — this set is what lets the fork's panel adopt
 * those rows. Cycle- and gap-tolerant: a malformed parentId chain yields
 * whatever was collected before the break.
 */
export const ancestorLineageOf = (snapshot: OmpSessionTreeSnapshot, sessionId: string): Set<string> => {
  const byId = new Map(snapshot.nodes.map((node) => [node.id, node]));
  const lineage = new Set<string>();
  const visited = new Set<string>([sessionId]);
  let cursor = byId.get(sessionId);
  while (cursor?.parentId) {
    const parent = byId.get(cursor.parentId);
    if (!parent || visited.has(parent.id)) break;
    visited.add(parent.id);
    lineage.add(parent.id);
    cursor = parent;
  }
  return lineage;
};

/** Tree fetch source; overridable in tests (mock.module on the api module is
 * not reliable for this import graph, so the seam lives here). */
let fetchSessionTree: (sessionId: string, directory: string) =>
  Promise<OmpFetchJsonResult<OmpSessionTreeSnapshot>> = (sessionId, directory) =>
    treeApi.getSessionTree(sessionId, { directory });

const lineageByKey = new Map<string, Set<string>>();
const lineageInFlight = new Map<string, Promise<Set<string> | null>>();

/**
 * Fetch and cache the fork lineage once per (directory, session). Failures
 * (tree.v1 off, transport, malformed payload) leave the cache unset so the
 * section keeps its current-session-only behavior — never an authoritative
 * "no ancestors" answer built from a failed read.
 */
export const primeSubagentLineage = (sessionId: string, directory: string): Promise<Set<string> | null> => {
  const key = `${directory}\n${sessionId}`;
  const cached = lineageByKey.get(key);
  if (cached) return Promise.resolve(cached);
  const existing = lineageInFlight.get(key);
  if (existing) return existing;
  const request = (async () => {
    const result = await fetchSessionTree(sessionId, directory);
    if (!result.ok) return null;
    const lineage = ancestorLineageOf(result.data, sessionId);
    if (lineage.size > 0) lineageByKey.set(key, lineage);
    return lineage;
  })().catch(() => null).finally(() => {
    lineageInFlight.delete(key);
  });
  lineageInFlight.set(key, request);
  return request;
};

/** Test seam: clears the lineage cache between cases (capabilityGate pattern). */
export const __clearSubagentLineageForTests = (): void => {
  lineageByKey.clear();
  lineageInFlight.clear();
};

/** Test seam: replaces the tree fetch source. */
export const __setSubagentTreeSourceForTests = (
  source: (sessionId: string, directory: string) => Promise<OmpFetchJsonResult<OmpSessionTreeSnapshot>>,
): void => {
  fetchSessionTree = source;
};

/** Synchronous cache read; `primeSubagentLineage` fills it. */
export const subagentLineageOf = (sessionId: string, directory: string): Set<string> | null =>
  lineageByKey.get(`${directory}\n${sessionId}`) ?? null;
