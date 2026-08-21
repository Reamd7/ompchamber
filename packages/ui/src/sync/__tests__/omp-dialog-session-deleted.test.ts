/**
 * session.deleted full-clean for omp dialogs (spec 03 §5.6.4 C10 slice):
 * the wire deletion path must drop the deleted session's pending dialogs
 * from useOmpDialogStore alongside the persisted-session cleanup, scoped to
 * the confirmed runtime, while other sessions' dialogs survive.
 *
 * Drives the real `handleEvent` from sync-context with a minimal child-store
 * stub; only the persisted-cleanup module is mocked to observe the identity
 * it was handed.
 */
import { beforeEach, describe, expect, mock, test } from "bun:test";
import type { Event } from "@/lib/opencode/wire";
import type { ChildStoreManager } from "../child-store";

const cleanedIdentities: Array<{ runtimeKey: string; directory: string; sessionId: string }> = [];

mock.module("@/sync/session-deletion-cleanup", () => ({
  cleanupPersistedSessionState: (identity: { runtimeKey: string; directory: string; sessionId: string }) => {
    cleanedIdentities.push(identity);
  },
}));

import { getRuntimeKey } from "@/lib/runtime-switch";
import { createEventRoutingIndex, handleEvent } from "../sync-context";
import { useOmpDialogStore } from "../useOmpDialogStore";
import type { OmpPendingDialog } from "@/lib/api/omp";

const approvalDialog = (id: string, sessionId: string): OmpPendingDialog => ({
  id,
  sessionId,
  createdAt: 1000,
  kind: "approval",
  approval: { prompt: "Run bash?" },
});

const childStoresStub = {
  children: new Map(),
  getChild: () => undefined,
  mark: () => {},
} as unknown as ChildStoreManager;

const deleteEvent = (sessionId: string): Event =>
  ({ type: "session.deleted", properties: { sessionID: sessionId } } as Event);

const pendingIds = (): string[] =>
  Object.keys(useOmpDialogStore.getState().directories["/repo"]?.dialogs ?? {});

describe("session.deleted clears the session's pending omp dialogs", () => {
  let runtimeKey: string;

  beforeEach(() => {
    runtimeKey = getRuntimeKey();
    cleanedIdentities.length = 0;
    useOmpDialogStore.setState({ directories: {}, runtimeKey: "" });
    useOmpDialogStore.getState().adoptRuntime(runtimeKey);
    useOmpDialogStore.getState().ingestRequested(runtimeKey, "/repo", approvalDialog("dlg_a", "ses_a"));
    useOmpDialogStore.getState().ingestRequested(runtimeKey, "/repo", approvalDialog("dlg_b", "ses_b"));
  });

  test("drops only the deleted session's dialogs and forwards the identity", () => {
    handleEvent("/repo", deleteEvent("ses_a"), childStoresStub, createEventRoutingIndex(), runtimeKey);

    expect(pendingIds()).toEqual(["dlg_b"]);
    expect(cleanedIdentities).toEqual([{ runtimeKey, directory: "/repo", sessionId: "ses_a" }]);
  });

  test("a stale runtime key clears nothing", () => {
    handleEvent("/repo", deleteEvent("ses_b"), childStoresStub, createEventRoutingIndex(), `${runtimeKey}-stale`);

    expect(pendingIds()).toEqual(["dlg_a", "dlg_b"]);
    expect(cleanedIdentities).toEqual([]);
  });
});
