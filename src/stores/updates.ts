import { relaunch } from '@tauri-apps/plugin-process';
import { check, type Update } from '@tauri-apps/plugin-updater';
import { create } from 'zustand';

import { errorMessage } from '@/lib/errors';
import { isWindows } from '@/lib/platform';

import { useSettings } from './settings';
import { useUi } from './ui';

/** How often a running app looks for a new release when automatic checks are on. */
const CHECK_EVERY_MS = 6 * 60 * 60 * 1000;

export type UpdateState =
  | { step: 'idle' }
  | { step: 'checking' }
  | { step: 'none'; checkedAt: number }
  | { step: 'available'; update: Update }
  | { step: 'downloading'; update: Update; done: number; total: number | null }
  /** Downloaded; on Windows the installer runs (and closes the app) on restart. */
  | { step: 'ready'; update: Update }
  | { step: 'error'; message: string };

interface UpdatesState {
  state: UpdateState;
  /** Looks for a signed update. `quiet` checks (automatic ones) do not report "up to date" or errors. */
  check: (quiet?: boolean) => Promise<void>;
  /** Downloads the update, verifies its signature and installs it (staged on Windows). */
  install: () => Promise<void>;
  /** Restarts into the installed update. */
  restart: () => Promise<void>;
}

export const useUpdates = create<UpdatesState>((set, get) => ({
  state: { step: 'idle' },
  check: async (quiet = false) => {
    const busy = get().state.step;
    if (busy === 'checking' || busy === 'downloading' || busy === 'ready') return;
    set({ state: { step: 'checking' } });
    try {
      const update = await check({ timeout: 30_000 });
      if (!update) {
        set({ state: { step: 'none', checkedAt: Date.now() } });
        return;
      }
      set({ state: { step: 'available', update } });
      if (useSettings.getState().settings.auto_install_updates) await get().install();
      else if (quiet) useUi.getState().toast(`DataQuery ${update.version} is available. Install it from Settings → About.`);
    } catch (e) {
      set({ state: quiet ? { step: 'idle' } : { step: 'error', message: errorMessage(e) } });
    }
  },
  install: async () => {
    const current = get().state;
    if (current.step !== 'available') return;
    const { update } = current;
    let done = 0;
    let total: number | null = null;
    set({ state: { step: 'downloading', update, done, total } });
    try {
      // The Windows installer closes the app, so it only downloads now and installs on restart.
      const run = isWindows ? update.download.bind(update) : update.downloadAndInstall.bind(update);
      await run((event) => {
        if (event.event === 'Started') total = event.data.contentLength ?? null;
        if (event.event === 'Progress') done += event.data.chunkLength;
        set({ state: { step: 'downloading', update, done, total } });
      });
      set({ state: { step: 'ready', update } });
      useUi.getState().toast(`DataQuery ${update.version} is ready. Restart to finish updating.`, 'positive');
    } catch (e) {
      set({ state: { step: 'error', message: `The update was not installed: ${errorMessage(e)}` } });
    }
  },
  restart: async () => {
    const current = get().state;
    if (current.step !== 'ready') return;
    try {
      if (isWindows) await current.update.install();
      await relaunch();
    } catch (e) {
      set({ state: { step: 'error', message: errorMessage(e) } });
    }
  },
}));

/**
 * Checks at launch and every few hours while automatic checks are on.
 * Returns a function that stops the timer.
 */
export function startUpdateChecks(): () => void {
  const tick = () => {
    if (useSettings.getState().settings.check_updates_on_launch) void useUpdates.getState().check(true);
  };
  tick();
  const timer = window.setInterval(tick, CHECK_EVERY_MS);
  return () => window.clearInterval(timer);
}
