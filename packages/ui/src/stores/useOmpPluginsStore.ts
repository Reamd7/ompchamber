import { create } from 'zustand';
import { getRegisteredRuntimeAPIs } from '@/contexts/runtimeAPIRegistry';
import { useProjectsStore } from '@/stores/useProjectsStore';
import { opencodeClient } from '@/lib/opencode/client';
import type {
  OmpAppliedSession,
  OmpExtensionRecord,
  OmpPluginMutationResult,
  OmpPluginRecord,
} from '@/lib/api/omp';

export type OmpPluginSelection = OmpPluginRecord | OmpExtensionRecord;

type OmpExtensionContent = {
  fileName: string;
  scope: 'user' | 'project';
  content: string;
  editable: boolean;
  source: 'native' | 'configured' | 'discovered' | 'plugin-manifest';
};

interface OmpPluginsStore {
  plugins: OmpPluginRecord[];
  extensions: OmpExtensionRecord[];
  applied: OmpAppliedSession[];
  selectedId: string | null;
  isLoading: boolean;
  isSaving: boolean;
  extensionContent: Record<string, OmpExtensionContent>;
  setSelected: (id: string | null) => void;
  load: (options?: { force?: boolean }) => Promise<boolean>;
  install: (spec: string, scope?: 'user' | 'project') => Promise<OmpPluginMutationResult>;
  updatePlugin: (input: {
    id: string;
    enabled?: boolean;
    enabledFeatures?: string[];
    setting?: { key: string; value?: unknown; remove?: boolean };
  }) => Promise<OmpPluginMutationResult>;
  togglePlugin: (id: string, enabled: boolean) => Promise<OmpPluginMutationResult>;
  removePlugin: (id: string) => Promise<OmpPluginMutationResult>;
  readExtension: (id: string) => Promise<OmpExtensionContent | null>;
  updateExtension: (id: string, content: string) => Promise<OmpPluginMutationResult>;
  removeExtension: (id: string) => Promise<OmpPluginMutationResult>;
  getById: (id: string) => OmpPluginSelection | undefined;
  revealPlugin: (id: string) => Promise<OmpPluginMutationResult>;
  revealExtension: (id: string) => Promise<OmpPluginMutationResult>;
  reloadPlugins: (sessionId?: string) => Promise<OmpPluginMutationResult & { sessionsRefreshed?: number }>;
}
const getDirectory = (): string | null => {
  const project = useProjectsStore.getState().getActiveProject?.();
  return project?.path?.trim() || opencodeClient.getDirectory()?.trim() || null;
};
export const useOmpPluginsStore = create<OmpPluginsStore>((set, get) => ({
  plugins: [],
  extensions: [],
  selectedId: null,
  applied: [],
  isLoading: false,
  isSaving: false,
  extensionContent: {},



  setSelected: (selectedId) => set({ selectedId }),
  install: async (spec, scope) => {
    const directory = getDirectory();
    const api = getRegisteredRuntimeAPIs()?.ompPlugins;
    if (!directory || !api) return { ok: false, unavailable: true };
    set({ isSaving: true });
    const result = await api.install({ spec, directory, scope });
    set({ isSaving: false });
    if (result.ok) await get().load({ force: true });
    return result;
  },

  reloadPlugins: async (sessionId) => {
    const directory = getDirectory();
    const api = getRegisteredRuntimeAPIs()?.ompPlugins;
    if (!directory || !api) return { ok: false, unavailable: true };
    const result = await api.reload({ directory, sessionId });
    if (result.ok) await get().load({ force: true });
    return result;
  },


  load: async () => {
    const directory = getDirectory();
    const api = getRegisteredRuntimeAPIs()?.ompPlugins;
    if (!directory || !api) return false;
    set({ isLoading: true });
    const result = await api.list({ directory });
    if (!result.ok) {
      set({ isLoading: false });
      return false;
    }
    const applied = await api.listApplied({ directory });
    set({
      plugins: result.data.plugins,
      extensions: result.data.extensions,
      applied: applied.ok ? applied.data : [],
      isLoading: false,
    });
    const selectedId = get().selectedId;
    if (selectedId && !result.data.plugins.some((item) => item.id === selectedId) && !result.data.extensions.some((item) => item.id === selectedId)) {
      set({ selectedId: null });
    }
    return true;
  },

  togglePlugin: async (id, enabled) => get().updatePlugin({ id, enabled }),

  updatePlugin: async ({ id, enabled, enabledFeatures, setting }) => {
    const directory = getDirectory();
    const api = getRegisteredRuntimeAPIs()?.ompPlugins;
    if (!directory || !api) return { ok: false, unavailable: true };
    set({ isSaving: true });
    const result = await api.update({ id, directory, enabled, enabledFeatures, setting });
    set({ isSaving: false });
    if (result.ok) await get().load({ force: true });
    return result;
  },
  removePlugin: async (id) => {
    const directory = getDirectory();
    const api = getRegisteredRuntimeAPIs()?.ompPlugins;
    if (!directory || !api) return { ok: false, unavailable: true };
    set({ isSaving: true });
    const result = await api.remove({ id, directory });
    set({ isSaving: false });
    if (result.ok) {
      if (get().selectedId === id) set({ selectedId: null });
      await get().load({ force: true });
    }
    return result;
  },

  readExtension: async (id) => {
    const cached = get().extensionContent[id];
    if (cached) return cached;
    const directory = getDirectory();
    const api = getRegisteredRuntimeAPIs()?.ompPlugins;
    if (!directory || !api) return null;
    const result = await api.readExtension({ id, directory });
    if (!result.ok) return null;
    set((state) => ({ extensionContent: { ...state.extensionContent, [id]: result.data } }));
    return result.data;
  },

  updateExtension: async (id, content) => {
    const directory = getDirectory();
    const api = getRegisteredRuntimeAPIs()?.ompPlugins;
    if (!directory || !api) return { ok: false, unavailable: true };
    set({ isSaving: true });
    const result = await api.updateExtension({ id, content, directory });
    set({ isSaving: false });
    if (result.ok) {
      const current = get().extensionContent[id];
      if (current) set((state) => ({ extensionContent: { ...state.extensionContent, [id]: { ...current, content } } }));
      await get().load();
    }
    return result;
  },
  revealPlugin: async (id) => {
    const directory = getDirectory();
    const api = getRegisteredRuntimeAPIs()?.ompPlugins;
    if (!directory || !api) return { ok: false, unavailable: true };
    return api.revealPlugin({ id, directory });
  },
  revealExtension: async (id) => {
    const directory = getDirectory();
    const api = getRegisteredRuntimeAPIs()?.ompPlugins;
    if (!directory || !api) return { ok: false, unavailable: true };
    return api.revealExtension({ id, directory });
  },
  removeExtension: async (id) => {
    const directory = getDirectory();
    const api = getRegisteredRuntimeAPIs()?.ompPlugins;
    if (!directory || !api) return { ok: false, unavailable: true };
    set({ isSaving: true });
    const result = await api.removeExtension({ id, directory });
    set({ isSaving: false });
    if (result.ok) {
      const nextContent = { ...get().extensionContent };
      delete nextContent[id];
      set({ extensionContent: nextContent, ...(get().selectedId === id ? { selectedId: null } : {}) });
      await get().load({ force: true });
    }
    return result;
  },

  getById: (id) => get().plugins.find((item) => item.id === id) ?? get().extensions.find((item) => item.id === id),
}));
