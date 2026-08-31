/**
 * useInternalUriViewerStore — ephemeral open target for the internal URI
 * viewer (spec 04 §5.2.5 GAP-02).
 *
 * Message markdown (bare-text uplift + explicit links) calls `open(url)`; the
 * viewer layer mounted by the chat view renders it for the active
 * (directory, sessionId) — those ids live on the layer, not here, because the
 * resolve endpoint pins to the session in view, not the session that emitted
 * the link. The artifacts browser additionally passes `target` so one
 * session's local:// files stay openable while another session is in view
 * (host-level supervision, spec 04): `target` pins BOTH the resolve ids;
 * without it the active-session behavior is unchanged. No persistence, no
 * history: dismissing closes it.
 */

import { create } from 'zustand';

interface InternalUriViewerTarget {
  sessionID: string;
  directory: string;
}

interface InternalUriViewerStore {
  url: string | null;
  target: InternalUriViewerTarget | null;
  open: (url: string, target?: InternalUriViewerTarget) => void;
  close: () => void;
}

export const useInternalUriViewerStore = create<InternalUriViewerStore>((set) => ({
  url: null,
  target: null,
  open: (url, target) => {
    const trimmed = url.trim();
    if (trimmed.length === 0) return;
    set({
      url: trimmed,
      target:
        target && typeof target.sessionID === 'string' && target.sessionID.length > 0
        && typeof target.directory === 'string' && target.directory.length > 0
          ? { sessionID: target.sessionID, directory: target.directory }
          : null,
    });
  },
  close: () => set({ url: null, target: null }),
}));

/** Imperative seam for non-React listeners (markdown delegated clicks). */
export const openInternalUriViewer = (url: string): void => {
  useInternalUriViewerStore.getState().open(url);
};
