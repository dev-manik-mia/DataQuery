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
  editor_font_family: 'JetBrains Mono',
  editor_line_height: 150,
  editor_ligatures: false,
  editor_vim: false,
  keymap: {},
  page_size: 100,
  language: 'en',
  manual_commit: false,
  mask_columns: true,
  mask_patterns: ['password', 'passwd', 'secret', 'token', 'api_key', 'apikey', 'ssn', 'card_number', 'cvv', 'iban'],
  history_max_entries: 20000,
  history_max_days: 0,
  history_redact_literals: false,
  autosave_scripts: true,
  ai_enabled: false,
  ai_provider: 'anthropic',
  ai_model: '',
  ai_endpoint: 'http://localhost:11434/v1',
  check_updates_on_launch: true,
  auto_install_updates: false,
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
