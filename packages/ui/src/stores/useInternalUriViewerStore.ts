/**
 * useInternalUriViewerStore — ephemeral open target for the internal URI
 * viewer (spec 04 §5.2.5 GAP-02).
 *
 * Message markdown (bare-text uplift + explicit links) calls `open(url)`; the
 * viewer layer mounted by the chat view renders it for the active
 * (directory, sessionId) — those ids live on the layer, not here, because the
 * resolve endpoint pins to the session in view, not the session that emitted
 * the link. No persistence, no history: dismissing closes it.
 */

import { create } from 'zustand';

interface InternalUriViewerStore {
  url: string | null;
  open: (url: string) => void;
  close: () => void;
}

export const useInternalUriViewerStore = create<InternalUriViewerStore>((set) => ({
  url: null,
  open: (url) => {
    const trimmed = url.trim();
    if (trimmed.length > 0) set({ url: trimmed });
  },
  close: () => set({ url: null }),
}));

/** Imperative seam for non-React listeners (markdown delegated clicks). */
export const openInternalUriViewer = (url: string): void => {
  useInternalUriViewerStore.getState().open(url);
};
