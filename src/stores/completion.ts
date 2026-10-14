import { create } from 'zustand';

import type { SchemaModel } from '@/bindings/SchemaModel';
import type { Snippet } from '@/bindings/Snippet';
import { api } from '@/lib/ipc';

/**
 * Catalog snapshots for autocomplete (tables and columns of the connected
 * database) and the user's snippets. Loaded once per session; refreshed on demand.
 */
interface CompletionState {
  sessionId: string | null;
  model: SchemaModel | null;
  snippets: Snippet[];
  load: (sessionId: string, database: string) => Promise<void>;
  loadSnippets: () => Promise<void>;
  reset: () => void;
}

export const useCompletion = create<CompletionState>((set, get) => ({
  sessionId: null,
  model: null,
  snippets: [],
  load: async (sessionId, database) => {
    if (get().sessionId === sessionId && get().model) return;
    set({ sessionId, model: null });
    try {
      const model = await api.schemaModel({ sessionId }, database, null);
      if (get().sessionId === sessionId) set({ model });
    } catch {
      // Autocomplete falls back to keywords.
    }
  },
  loadSnippets: async () => {
    set({ snippets: await api.listSnippets().catch(() => []) });
  },
  reset: () => set({ sessionId: null, model: null }),
}));
