import { create } from 'zustand';

import type { ConnectionDraft } from '@/bindings/ConnectionDraft';
import type { Engine } from '@/bindings/Engine';
import type { EngineInfo } from '@/bindings/EngineInfo';
import type { Folder } from '@/bindings/Folder';
import type { SavedConnection } from '@/bindings/SavedConnection';
import { api } from '@/lib/ipc';

import { useUi } from './ui';

interface ConnectionsState {
  engines: EngineInfo[];
  connections: SavedConnection[];
  folders: Folder[];
  loaded: boolean;
  load: () => Promise<void>;
  save: (draft: ConnectionDraft) => Promise<SavedConnection>;
  remove: (id: string) => Promise<void>;
  duplicate: (id: string) => Promise<void>;
  toggleFavorite: (id: string) => Promise<void>;
  createFolder: (name: string) => Promise<Folder | null>;
  renameFolder: (id: string, name: string) => Promise<void>;
  deleteFolder: (id: string) => Promise<void>;
}

const report = (e: unknown) => useUi.getState().toastError(e);

export const useConnections = create<ConnectionsState>((set, get) => ({
  engines: [],
  connections: [],
  folders: [],
  loaded: false,

  load: async () => {
    try {
      const [engines, list] = await Promise.all([get().engines.length ? get().engines : api.listEngines(), api.listConnections()]);
      set({ engines, connections: list.connections, folders: list.folders, loaded: true });
    } catch (e) {
      set({ loaded: true });
      report(e);
    }
  },

  save: async (draft) => {
    const saved = await api.saveConnection(draft);
    await get().load();
    return saved;
  },

  remove: async (id) => {
    try {
      await api.deleteConnection(id);
      await get().load();
    } catch (e) {
      report(e);
    }
  },

  duplicate: async (id) => {
    try {
      await api.duplicateConnection(id);
      await get().load();
    } catch (e) {
      report(e);
    }
  },

  toggleFavorite: async (id) => {
    const connection = get().connections.find((c) => c.id === id);
    if (!connection) return;
    set({ connections: get().connections.map((c) => (c.id === id ? { ...c, favorite: !c.favorite } : c)) });
    try {
      await api.setFavorite(id, !connection.favorite);
    } catch (e) {
      report(e);
      await get().load();
    }
  },

  createFolder: async (name) => {
    try {
      const folder = await api.createFolder(name);
      await get().load();
      return folder;
    } catch (e) {
      report(e);
      return null;
    }
  },

  renameFolder: async (id, name) => {
    try {
      await api.renameFolder(id, name);
      await get().load();
    } catch (e) {
      report(e);
    }
  },

  deleteFolder: async (id) => {
    try {
      await api.deleteFolder(id);
      await get().load();
    } catch (e) {
      report(e);
    }
  },
}));

export function useEngine(engine: Engine | undefined): EngineInfo | undefined {
  return useConnections((s) => s.engines.find((e) => e.engine === engine));
}
