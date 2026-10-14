import { create } from 'zustand';

import type { Settings } from '@/bindings/Settings';
import { api } from '@/lib/ipc';

import { useUi } from './ui';

export const DEFAULT_SETTINGS: Settings = {
  theme: 'dark',
  row_limit: 500,
  timeout_secs: 30,
  confirm_destructive: true,
  confirm_production_writes: true,
  keep_history: true,
  editor_font_size: 13,
  page_size: 100,
};

interface SettingsState {
  settings: Settings;
  loaded: boolean;
  load: () => Promise<void>;
  update: (patch: Partial<Settings>) => Promise<void>;
}

export const useSettings = create<SettingsState>((set, get) => ({
  settings: DEFAULT_SETTINGS,
  loaded: false,
  load: async () => {
    try {
      set({ settings: await api.getSettings(), loaded: true });
    } catch (e) {
      set({ loaded: true });
      useUi.getState().toastError(e);
    }
  },
  update: async (patch) => {
    const previous = get().settings;
    set({ settings: { ...previous, ...patch } });
    try {
      set({ settings: await api.saveSettings({ ...previous, ...patch }) });
    } catch (e) {
      set({ settings: previous });
      useUi.getState().toastError(e);
    }
  },
}));

/** Applies the theme to <html data-mode>, following the OS when set to System. */
export function applyTheme(theme: Settings['theme']) {
  const dark = theme === 'dark' || (theme === 'system' && window.matchMedia('(prefers-color-scheme: dark)').matches);
  document.documentElement.dataset.mode = dark ? 'dark' : 'light';
}
