import { create } from 'zustand';

import { api } from '@/lib/ipc';

export interface Layout {
  explorerWidth: number;
  editorHeight: number;
}

export const LAYOUT_LIMITS = {
  explorer: { min: 200, max: 420 },
  editor: { min: 90, max: 900 },
};

const DEFAULT_LAYOUT: Layout = { explorerWidth: 260, editorHeight: 250 };

interface LayoutState extends Layout {
  load: () => Promise<void>;
  set: (patch: Partial<Layout>) => void;
}

let timer: number | undefined;

/** Panel sizes, persisted across launches. */
export const useLayout = create<LayoutState>((set, get) => ({
  ...DEFAULT_LAYOUT,
  load: async () => {
    const saved = await api.getUiState<Partial<Layout>>('layout').catch(() => null);
    if (saved) set({ ...DEFAULT_LAYOUT, ...saved });
  },
  set: (patch) => {
    set(patch);
    window.clearTimeout(timer);
    timer = window.setTimeout(() => {
      const { explorerWidth, editorHeight } = get();
      void api.setUiState('layout', { explorerWidth, editorHeight }).catch(() => undefined);
    }, 400);
  },
}));
