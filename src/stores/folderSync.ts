import { listen } from '@tauri-apps/api/event';
import { create } from 'zustand';

import type { ScriptsFolderStatus } from '@/bindings/ScriptsFolderStatus';
import type { SyncStatus } from '@/bindings/SyncStatus';
import { api } from '@/lib/ipc';

import { useConnections } from './connections';
import { useSettings } from './settings';

/**
 * Status of the two folder syncs: connections (one encrypted file) and saved
 * scripts (`.sql` files). The backend also syncs when the folders change on
 * disk and reports every run through the `sync-status` and `scripts-folder`
 * events; this store adds the app-side triggers.
 */
interface FolderSyncState {
  sync: SyncStatus | null;
  scripts: ScriptsFolderStatus | null;
  /** Bumped when scripts changed on disk, so script lists reload. */
  scriptsVersion: number;
  load: () => Promise<void>;
  syncNow: () => Promise<void>;
  syncScripts: () => Promise<void>;
}

export const useFolderSync = create<FolderSyncState>((set) => ({
  sync: null,
  scripts: null,
  scriptsVersion: 0,
  load: async () => {
    const [sync, scripts] = await Promise.all([api.syncStatus(), api.scriptsFolderStatus()]);
    set({ sync, scripts });
  },
  // Results arrive through the events as well; errors are part of the status.
  syncNow: async () => set({ sync: await api.syncNow() }),
  syncScripts: async () => set({ scripts: await api.scriptsFolderSync() }),
}));

/** Connection changes are synced after this quiet period. */
const CHANGE_DEBOUNCE_MS = 2000;
/** Focusing the window syncs at most this often. */
const FOCUS_INTERVAL_MS = 15_000;

/** Starts the app-side triggers: window focus, connection edits, and the backend's events. */
export function startFolderSync(): () => void {
  const store = useFolderSync.getState();
  const configured = () => useSettings.getState().settings;
  void store.load().catch(() => undefined);

  // The first load at launch is not a change either: the backend syncs at launch itself.
  let applying = !useConnections.getState().loaded;
  const unlistenSync = listen<SyncStatus>('sync-status', (e) => {
    useFolderSync.setState({ sync: e.payload });
    if (e.payload.changed && !e.payload.running) {
      applying = true;
      void useConnections.getState().load();
    }
  });
  const unlistenScripts = listen<ScriptsFolderStatus>('scripts-folder', (e) => {
    useFolderSync.setState({ scripts: e.payload });
    const ids = e.payload.report?.changed_ids ?? [];
    if (!ids.length) return;
    useFolderSync.setState((s) => ({ scriptsVersion: s.scriptsVersion + 1 }));
    void import('@/features/history/scriptConflicts').then((m) => m.onScriptsChanged(ids));
  });

  let changeTimer: number | undefined;
  const unsubscribe = useConnections.subscribe((s, prev) => {
    if (s.connections === prev.connections && s.folders === prev.folders) return;
    // A reload caused by a sync is not a new local change.
    if (applying) {
      applying = false;
      return;
    }
    if (!configured().sync_folder) return;
    window.clearTimeout(changeTimer);
    changeTimer = window.setTimeout(() => void useFolderSync.getState().syncNow().catch(() => undefined), CHANGE_DEBOUNCE_MS);
  });

  let lastFocus = Date.now();
  const onFocus = () => {
    if (Date.now() - lastFocus < FOCUS_INTERVAL_MS) return;
    lastFocus = Date.now();
    const { sync_folder, scripts_folder } = configured();
    if (sync_folder) void useFolderSync.getState().syncNow().catch(() => undefined);
    if (scripts_folder) void useFolderSync.getState().syncScripts().catch(() => undefined);
  };
  window.addEventListener('focus', onFocus);

  return () => {
    window.clearTimeout(changeTimer);
    window.removeEventListener('focus', onFocus);
    unsubscribe();
    void unlistenSync.then((fn) => fn());
    void unlistenScripts.then((fn) => fn());
  };
}
