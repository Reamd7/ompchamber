import { beforeEach, describe, expect, test } from "bun:test";

import { resolveDeepLinkSessionDirectory } from "./useRouter";
import { useSessionUIStore } from "@/sync/session-ui-store";
import { useGlobalSessionsStore } from "@/stores/useGlobalSessionsStore";
import type { Session } from "@/lib/opencode/wire";

/**
 * A cold `?session=<id>` link names a session no local index knows yet.
 * Selecting it against a guessed (active) directory leaves the transcript
 * blank until `adoptAuthoritativeSessionDirectory` corrects the guess. These
 * tests pin the router contract: the directory hint waits for the global
 * sessions authority when local state cannot answer, re-reads state after the
 * load settles, and degrades to the old guess behavior (null hint) only when
 * even the global lookup cannot name the session.
 */
const DIRECTORY = "/repo/sharkly";
const SESSION_ID = "ses_deep_link_cold";

// SAFETY: only `id` and `directory` are read by the directory-resolution path
// under test; the remaining wire Session fields never influence these
// assertions, so a minimal record is a faithful stand-in.
const sessionRecord = (directory: string): Session => ({
  id: SESSION_ID,
  slug: SESSION_ID,
  directory,
  title: "deep link target",
} as Session);

beforeEach(() => {
  useSessionUIStore.getState().setCurrentSession(null);
  useGlobalSessionsStore.getState().applySnapshot([], [], "idle");
});

describe("resolveDeepLinkSessionDirectory", () => {
  test("resolves without a load when the stores already know the session", async () => {
    useGlobalSessionsStore.getState().applySnapshot([sessionRecord("/warm/dir")], [], "ready");

    let loads = 0;
    const resolved = await resolveDeepLinkSessionDirectory(SESSION_ID, async () => {
      loads += 1;
    });

    expect(resolved).toBe("/warm/dir");
    expect(loads).toBe(0);
  });

  test("awaits the global sessions load for a locally unknown session and re-reads after it settles", async () => {
    let loads = 0;
    let releaseLoad: () => void = () => {};
    const gate = new Promise<void>((resolveGate) => {
      releaseLoad = resolveGate;
    });

    const resolved = resolveDeepLinkSessionDirectory(SESSION_ID, async () => {
      loads += 1;
      await gate;
      useGlobalSessionsStore.getState().applySnapshot([sessionRecord(DIRECTORY)], [], "ready");
    });

    // A snapshot naming the session lands while the load is still pending —
    // the resolver must still take its answer from the post-load state.
    useGlobalSessionsStore.getState().applySnapshot([sessionRecord("/stale-early")], [], "ready");

    releaseLoad();
    expect(await resolved).toBe(DIRECTORY);
    expect(loads).toBe(1);
  });

  test("returns null when the global lookup still cannot name the session", async () => {
    const resolved = await resolveDeepLinkSessionDirectory(SESSION_ID, async () => {
      useGlobalSessionsStore.getState().applySnapshot([], [], "error");
    });

    expect(resolved).toBeNull();
  });
});
