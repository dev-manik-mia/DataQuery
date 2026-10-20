import { create } from 'zustand';

import type { DbError } from '@/bindings/DbError';
import type { Node } from '@/bindings/Node';
import type { ObjectRef } from '@/bindings/ObjectRef';
import type { SessionInfo } from '@/bindings/SessionInfo';
import { EMPTY_SELECTION, type TreeSelection } from '@/features/explorer/selection';
import { toAppError } from '@/lib/errors';
import { api } from '@/lib/ipc';

export const ROOT = 'root';

export function nodeKey(o: ObjectRef | null): string {
  return o ? `${o.kind}\u0000${o.database}\u0000${o.schema ?? ''}\u0000${o.name}\u0000${o.parent ?? ''}` : ROOT;
}

interface ExplorerState {
  session: SessionInfo | null;
  children: Record<string, Node[]>;
  expanded: Record<string, boolean>;
  loading: Record<string, boolean>;
  errors: Record<string, DbError | undefined>;
  /** The cursor row. */
  selected: string | null;
  /** Rows selected with ⌘/Ctrl- and Shift-clicks; the cursor row alone otherwise. */
  selection: TreeSelection;
  filter: string;
  reset: (session: SessionInfo | null) => void;
  load: (parent: Node | null) => Promise<void>;
  toggle: (node: Node) => void;
  refresh: (node: Node | null) => void;
  /** Moves the cursor and selects only that row. */
  select: (key: string | null) => void;
  /** Moves the cursor and keeps a multi-selection. */
  setSelection: (selection: TreeSelection, cursor: string | null) => void;
  setFilter: (filter: string) => void;
}

export const useExplorer = create<ExplorerState>((set, get) => ({
  session: null,
  children: {},
  expanded: {},
  loading: {},
  errors: {},
  selected: null,
  selection: EMPTY_SELECTION,
  filter: '',

  reset: (session) => {
    set({ session, children: {}, expanded: {}, loading: {}, errors: {}, selected: null, selection: EMPTY_SELECTION, filter: '' });
    if (session) void get().load(null).then(() => autoExpand(session));
  },

  load: async (parent) => {
    const session = get().session;
    if (!session) return;
    const key = nodeKey(parent);
    set({ loading: { ...get().loading, [key]: true }, errors: { ...get().errors, [key]: undefined } });
    try {
      const nodes = await api.listChildren({ sessionId: session.session_id }, parent);
      if (get().session !== session) return;
      set({ children: { ...get().children, [key]: nodes } });
    } catch (e) {
      set({ errors: { ...get().errors, [key]: toAppError(e).info } });
    } finally {
      set({ loading: { ...get().loading, [key]: false } });
    }
  },

  toggle: (node) => {
    const key = nodeKey(node);
    const open = !get().expanded[key];
    set({ expanded: { ...get().expanded, [key]: open } });
    if (open && !get().children[key]) void get().load(node);
  },

  /** Reloads one node's children without touching the rest of the tree. */
  refresh: (node) => {
    const key = nodeKey(node);
    if (node && !get().expanded[key]) set({ expanded: { ...get().expanded, [key]: true } });
    void get().load(node);
  },

  select: (selected) => set({ selected, selection: selected ? { keys: [selected], anchor: selected } : EMPTY_SELECTION }),
  setSelection: (selection, selected) => set({ selection, selected }),
  setFilter: (filter) => set({ filter }),
}));

/** Opens the connected database and its first schema so tables are visible at once. */
async function autoExpand(session: SessionInfo) {
  const state = useExplorer.getState();
  const current = state.children[ROOT]?.find((n) => n.current) ?? state.children[ROOT]?.[0];
  if (!current) return;
  useExplorer.setState((s) => ({ expanded: { ...s.expanded, [nodeKey(current)]: true } }));
  await state.load(current);
  if (!session.capabilities.schemas) return;
  const schemas = useExplorer.getState().children[nodeKey(current)] ?? [];
  const first = schemas.find((s) => s.name === 'public') ?? schemas[0];
  if (first) useExplorer.getState().toggle(first);
}
