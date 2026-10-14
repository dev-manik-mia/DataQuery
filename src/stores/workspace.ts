import { create } from 'zustand';

import type { Cell } from '@/bindings/Cell';
import type { ColumnMeta } from '@/bindings/ColumnMeta';
import type { ColumnValue } from '@/bindings/ColumnValue';
import type { DbError } from '@/bindings/DbError';
import type { ExplainResult } from '@/bindings/ExplainResult';
import type { ObjectRef } from '@/bindings/ObjectRef';
import type { RunReport } from '@/bindings/RunReport';
import type { SessionInfo } from '@/bindings/SessionInfo';
import type { TxAction } from '@/bindings/TxAction';
import type { TxStatus } from '@/bindings/TxStatus';
import { toAppError } from '@/lib/errors';
import { formatCount, plural } from '@/lib/format';
import { uid } from '@/lib/id';
import { api } from '@/lib/ipc';

import { useExplorer } from './explorer';
import { useSettings } from './settings';
import { guarded, useUi } from './ui';

export type TableView = 'data' | 'structure' | 'ddl' | 'dependencies';

export interface ResultSet {
  id: string;
  statement: number;
  /** The statement text, for export (which re-runs it on a read-only connection). */
  sql: string;
  columns: ColumnMeta[];
  rows: Cell[][];
  hasMore: boolean;
  /** Only the latest statement keeps a server cursor open. */
  canFetchMore: boolean;
  command: string;
  durationMs: number;
}

export interface RunMessage {
  id: string;
  kind: 'ok' | 'error' | 'notice' | 'info';
  statement: number | null;
  text: string;
  error?: DbError;
  durationMs?: number;
}

export interface RunState {
  status: 'idle' | 'running' | 'done';
  startedAt: number | null;
  results: ResultSet[];
  messages: RunMessage[];
  active: ResultsTab;
  fetching: boolean;
  /** Absolute editor range of the error position, for the squiggle. */
  errorAt: number | null;
  lastDurationMs: number | null;
  /** The last EXPLAIN, shown in the Plan tab of the results panel. */
  plan: ExplainResult | null;
}

export type ResultsTab = number | 'messages' | 'plan';

export interface QueryTab {
  id: string;
  kind: 'query';
  title: string;
  sql: string;
  /** SQL as last saved to a script or file; the tab is dirty when it differs. */
  savedSql: string;
  scriptId: string | null;
  filePath: string | null;
  /** 1-based line numbers with a bookmark. */
  bookmarks: number[];
  /** Last values typed for `$1` / `?` parameters, by position. */
  paramValues: string[];
  run: RunState;
}

export interface TableTab {
  id: string;
  kind: 'table';
  title: string;
  object: ObjectRef;
  view: TableView;
  /** Exact-match filter, e.g. from foreign-key navigation. */
  matches?: ColumnValue[];
}

/** A schema (or MySQL database) as a whole: ER diagram or framework migrations. */
export interface ContainerTab {
  id: string;
  kind: 'diagram' | 'migrations';
  title: string;
  database: string;
  schema: string | null;
}

export interface MonitorTab {
  id: string;
  kind: 'monitor';
  title: string;
}

export type Tab = QueryTab | TableTab | ContainerTab | MonitorTab;

/** Which pane shows which tab when the editor area is split. */
export interface SplitState {
  rightTabId: string | null;
  focus: 'left' | 'right';
}

const EMPTY_RUN: RunState = {
  status: 'idle',
  startedAt: null,
  results: [],
  messages: [],
  active: 'messages',
  fetching: false,
  errorAt: null,
  lastDurationMs: null,
  plan: null,
};

export function isDirty(tab: Tab): boolean {
  return tab.kind === 'query' && tab.sql !== tab.savedSql && (tab.sql.trim() !== '' || tab.savedSql !== '');
}

interface OpenQueryOptions {
  sql?: string;
  title?: string;
  scriptId?: string | null;
  filePath?: string | null;
}

/** Health of the open session, shown in the title and status bars. */
export type LinkStatus = 'connected' | 'connecting' | 'failed' | 'disconnected';

interface WorkspaceState {
  session: SessionInfo | null;
  link: LinkStatus;
  connectingId: string | null;
  connectError: DbError | null;
  tabs: Tab[];
  activeTabId: string | null;
  split: SplitState | null;
  /** Transaction state of the query connection. */
  tx: TxStatus;

  connect: (connectionId: string) => Promise<boolean>;
  /** Reopens the session's connections, keeping tabs and their SQL. */
  reconnect: () => Promise<void>;
  disconnect: () => Promise<void>;
  openQueryTab: (options?: OpenQueryOptions) => string;
  openTableTab: (object: ObjectRef, view?: TableView, matches?: ColumnValue[]) => void;
  setTableMatches: (id: string, matches: ColumnValue[] | undefined) => void;
  openContainerTab: (kind: ContainerTab['kind'], database: string, schema: string | null) => void;
  openMonitorTab: () => void;
  toggleSplit: () => void;
  focusPane: (pane: 'left' | 'right') => void;
  moveToOtherPane: (id: string) => void;
  setBookmarks: (id: string, bookmarks: number[]) => void;
  transaction: (action: TxAction) => Promise<void>;
  explain: (tabId: string, sql: string, params: (string | null)[] | null, analyze: boolean) => Promise<void>;
  closeTab: (id: string) => Promise<void>;
  activate: (id: string) => void;
  cycleTab: (step: 1 | -1) => void;
  updateSql: (id: string, sql: string) => void;
  markSaved: (id: string, patch: { title?: string; scriptId?: string | null; filePath?: string | null }) => void;
  setTableView: (id: string, view: TableView) => void;
  run: (tabId: string, sql: string, base: number, params?: (string | null)[] | null) => Promise<void>;
  fetchMore: (tabId: string) => Promise<void>;
  cancel: () => Promise<void>;
  setActiveResult: (tabId: string, active: ResultsTab) => void;
}

function nextQueryTitle(tabs: Tab[]): string {
  const used = new Set(tabs.map((t) => t.title));
  for (let n = 1; ; n += 1) if (!used.has(`Query ${n}`)) return `Query ${n}`;
}

function newQueryTab(tabs: Tab[], options: OpenQueryOptions = {}): QueryTab {
  return {
    id: uid('tab'),
    kind: 'query',
    title: options.title ?? nextQueryTitle(tabs),
    sql: options.sql ?? '',
    savedSql: options.scriptId || options.filePath ? (options.sql ?? '') : '',
    scriptId: options.scriptId ?? null,
    filePath: options.filePath ?? null,
    bookmarks: [],
    paramValues: [],
    run: EMPTY_RUN,
  };
}

function sameObject(a: ObjectRef, b: ObjectRef): boolean {
  return a.kind === b.kind && a.database === b.database && a.schema === b.schema && a.name === b.name;
}

/** Builds result tabs and the Messages list from a run report. */
function digest(report: RunReport, sql: string, base: number, previous: RunState): Partial<RunState> {
  const results: ResultSet[] = [];
  const messages: RunMessage[] = [];
  let errorAt: number | null = null;
  for (const outcome of report.outcomes) {
    const r = outcome.result;
    if (r) {
      if (r.columns.length > 0) {
        results.push({
          id: uid('result'),
          statement: outcome.index,
          sql: sql.slice(outcome.from, outcome.to),
          columns: r.columns,
          rows: r.rows,
          hasMore: r.has_more,
          canFetchMore: outcome.can_fetch_more,
          command: r.command,
          durationMs: r.duration_ms,
        });
      }
      const summary =
        r.columns.length > 0
          ? `${plural(r.rows.length, 'row')}${r.has_more ? ' (more available)' : ''}`
          : r.affected !== null
            ? `${plural(r.affected, 'row')} affected`
            : 'Done';
      messages.push({ id: uid('msg'), kind: 'ok', statement: outcome.index, text: `${r.command || 'Statement'} · ${summary}`, durationMs: r.duration_ms });
      for (const notice of r.notices) messages.push({ id: uid('msg'), kind: 'notice', statement: outcome.index, text: notice });
    }
    if (outcome.error) {
      messages.push({ id: uid('msg'), kind: 'error', statement: outcome.index, text: outcome.error.message, error: outcome.error });
      if (outcome.error.position !== null) errorAt = base + outcome.from + outcome.error.position - 1;
    }
  }
  if (report.skipped > 0) {
    messages.push({ id: uid('msg'), kind: 'info', statement: null, text: `${plural(report.skipped, 'statement')} after the error ${report.skipped === 1 ? 'was' : 'were'} not run.` });
  }
  const failed = report.outcomes.some((o) => o.error);
  // Keep earlier result tabs when this run produced none (e.g. it failed).
  const keepPrevious = results.length === 0 && failed;
  const shown = keepPrevious ? previous.results.map((r) => ({ ...r, canFetchMore: false })) : results;
  return {
    results: shown,
    messages,
    errorAt,
    active: failed || shown.length === 0 ? 'messages' : shown.length - 1,
    lastDurationMs: report.duration_ms,
  };
}

export const useWorkspace = create<WorkspaceState>((set, get) => {
  const patchTab = (id: string, patch: (tab: Tab) => Tab) => set({ tabs: get().tabs.map((t) => (t.id === id ? patch(t) : t)) });
  const patchRun = (id: string, patch: Partial<RunState> | ((run: RunState) => Partial<RunState>)) =>
    patchTab(id, (t) => (t.kind === 'query' ? { ...t, run: { ...t.run, ...(typeof patch === 'function' ? patch(t.run) : patch) } } : t));

  /** Shows a tab in the focused pane. */
  const showTab = (id: string) => {
    const split = get().split;
    if (split?.focus === 'right' && id !== get().activeTabId) set({ split: { ...split, rightTabId: id } });
    else if (split?.rightTabId === id) set({ split: { ...split, focus: 'right' } });
    else set({ activeTabId: id });
  };
  const addTab = (tab: Tab) => {
    set({ tabs: [...get().tabs, tab] });
    showTab(tab.id);
  };

  return {
    session: null,
    link: 'connected',
    connectingId: null,
    connectError: null,
    tabs: [],
    activeTabId: null,
    split: null,
    tx: 'auto',

    connect: async (connectionId) => {
      const current = get().session;
      if (current?.connection.id === connectionId) {
        useUi.getState().navigate({ name: 'workspace' });
        return true;
      }
      set({ connectingId: connectionId, connectError: null });
      try {
        const session = await api.connect(connectionId);
        if (current) await get().disconnect();
        const saved = await api.getUiState<PersistedWorkspace>(`workspace:${connectionId}`).catch(() => null);
        const tabs = restoreTabs(saved);
        const initial = tabs.length ? tabs : [newQueryTab([])];
        set({
          session,
          link: 'connected',
          connectingId: null,
          tabs: initial,
          activeTabId: initial.some((t) => t.id === saved?.activeTabId) ? (saved?.activeTabId ?? null) : (initial[0]?.id ?? null),
          split: saved?.split && initial.some((t) => t.id === saved.split?.rightTabId) ? saved.split : null,
          tx: 'auto',
        });
        useExplorer.getState().reset(session);
        useUi.getState().navigate({ name: 'workspace' });
        return true;
      } catch (e) {
        set({ connectingId: null, connectError: toAppError(e).info });
        return false;
      }
    },

    reconnect: async () => {
      const session = get().session;
      if (!session || get().link === 'connecting') return;
      set({ link: 'connecting' });
      try {
        const fresh = await api.reconnect({ sessionId: session.session_id }, session.connection.id);
        // Results held server-side by the old connections are gone.
        set((s) => ({
          session: fresh,
          link: 'connected',
          tx: 'auto',
          tabs: s.tabs.map((t) => (t.kind === 'query' ? { ...t, run: { ...t.run, results: t.run.results.map((r) => ({ ...r, canFetchMore: false })) } } : t)),
        }));
        useExplorer.getState().reset(fresh);
        useUi.getState().toast(`Reconnected to ${fresh.connection.name}.`, 'positive');
      } catch (e) {
        set({ link: 'failed' });
        useUi.getState().toastError(e);
      }
    },

    disconnect: async () => {
      const session = get().session;
      if (!session) return;
      if (get().tx !== 'auto') {
        const ok = await useUi.getState().ask({
          title: 'Disconnect with an open transaction?',
          body: 'Uncommitted changes in the open transaction are rolled back by the server when the connection closes.',
          confirmLabel: 'Disconnect and roll back',
          danger: true,
        });
        if (!ok) return;
      }
      await persistNow();
      set({ session: null, tabs: [], activeTabId: null, split: null, tx: 'auto' });
      useExplorer.getState().reset(null);
      await api.disconnect({ sessionId: session.session_id }).catch(() => undefined);
    },

    openQueryTab: (options) => {
      const tab = newQueryTab(get().tabs, options);
      addTab(tab);
      return tab.id;
    },

    openTableTab: (object, view = 'data', matches) => {
      // Indexes, functions and other non-relations only have a definition.
      const shown: TableView = object.kind === 'table' || object.kind === 'view' || object.kind === 'materialized_view' ? view : 'ddl';
      const existing = get().tabs.find((t): t is TableTab => t.kind === 'table' && sameObject(t.object, object));
      if (existing) {
        showTab(existing.id);
        if (existing.view !== shown) get().setTableView(existing.id, shown);
        if (matches) get().setTableMatches(existing.id, matches);
        return;
      }
      const tab: TableTab = { id: uid('tab'), kind: 'table', title: object.name, object, view: shown, matches };
      addTab(tab);
    },

    setTableMatches: (id, matches) => patchTab(id, (t) => (t.kind === 'table' ? { ...t, matches } : t)),

    openContainerTab: (kind, database, schema) => {
      const existing = get().tabs.find((t): t is ContainerTab => t.kind === kind && t.database === database && t.schema === schema);
      if (existing) return showTab(existing.id);
      const where = schema ?? database;
      addTab({ id: uid('tab'), kind, database, schema, title: kind === 'diagram' ? `ER · ${where}` : `Migrations · ${where}` });
    },

    openMonitorTab: () => {
      const existing = get().tabs.find((t) => t.kind === 'monitor');
      if (existing) return showTab(existing.id);
      addTab({ id: uid('tab'), kind: 'monitor', title: 'Monitoring' });
    },

    toggleSplit: () => {
      const { split, tabs, activeTabId } = get();
      if (split) return set({ split: null });
      const other = tabs.find((t) => t.id !== activeTabId);
      set({ split: { rightTabId: other?.id ?? null, focus: 'right' } });
      if (!other) get().openQueryTab();
    },

    focusPane: (focus) => {
      const split = get().split;
      if (split) set({ split: { ...split, focus } });
    },

    moveToOtherPane: (id) => {
      const { split, activeTabId, tabs } = get();
      if (!split) {
        const remaining = tabs.find((t) => t.id !== id);
        return set({ split: { rightTabId: id, focus: 'right' }, activeTabId: activeTabId === id ? (remaining?.id ?? null) : activeTabId });
      }
      if (split.rightTabId === id) set({ split: { ...split, rightTabId: activeTabId === id ? null : split.rightTabId, focus: 'left' }, activeTabId: id });
      else set({ split: { rightTabId: id, focus: 'right' }, activeTabId: activeTabId === id ? (tabs.find((t) => t.id !== id)?.id ?? null) : activeTabId });
    },

    setBookmarks: (id, bookmarks) => patchTab(id, (t) => (t.kind === 'query' ? { ...t, bookmarks } : t)),

    transaction: async (action) => {
      const session = get().session;
      if (!session) return;
      try {
        set({ tx: await api.transaction({ sessionId: session.session_id }, action) });
        useUi.getState().toast(action === 'begin' ? 'Transaction started.' : action === 'commit' ? 'Committed.' : 'Rolled back.', 'positive');
      } catch (e) {
        useUi.getState().toastError(e);
        if (action !== 'begin') set({ tx: 'auto' });
      }
    },

    explain: async (tabId, sql, params, analyze) => {
      const session = get().session;
      if (!session || !sql.trim()) return;
      patchRun(tabId, { status: 'running', startedAt: Date.now() });
      try {
        const plan = await api.explain({ sessionId: session.session_id }, sql, params, analyze);
        patchRun(tabId, { status: 'done', plan, active: 'plan' });
      } catch (e) {
        const error = toAppError(e).info;
        patchRun(tabId, (run) => ({ status: 'done', active: 'messages', messages: [...run.messages, { id: uid('msg'), kind: 'error', statement: null, text: error.message, error }] }));
      }
    },

    closeTab: async (id) => {
      const tab = get().tabs.find((t) => t.id === id);
      if (!tab) return;
      if (isDirty(tab)) {
        const ok = await useUi.getState().ask({
          title: `Close ${tab.title}?`,
          body: 'The SQL in this tab has not been saved to a script or file and will be lost.',
          confirmLabel: 'Close tab',
          danger: true,
        });
        if (!ok) return;
      }
      const tabs = get().tabs;
      const index = tabs.findIndex((t) => t.id === id);
      const remaining = tabs.filter((t) => t.id !== id);
      const split = get().split;
      if (split?.rightTabId === id) {
        set({ tabs: remaining, split: { ...split, rightTabId: null, focus: 'left' } });
        return;
      }
      const pick = remaining.filter((t) => t.id !== split?.rightTabId);
      const activeTabId = get().activeTabId === id ? (pick[Math.min(index, pick.length - 1)]?.id ?? null) : get().activeTabId;
      set({ tabs: remaining, activeTabId });
    },

    activate: (id) => showTab(id),

    cycleTab: (step) => {
      const { tabs, activeTabId } = get();
      if (!tabs.length) return;
      const i = tabs.findIndex((t) => t.id === activeTabId);
      set({ activeTabId: tabs[(i + step + tabs.length) % tabs.length]?.id ?? null });
    },

    updateSql: (id, sql) => {
      patchTab(id, (t) => (t.kind === 'query' ? { ...t, sql, run: t.run.errorAt === null ? t.run : { ...t.run, errorAt: null } } : t));
      scheduleAutosave(id);
    },

    markSaved: (id, patch) =>
      patchTab(id, (t) => (t.kind === 'query' ? { ...t, savedSql: t.sql, title: patch.title ?? t.title, scriptId: patch.scriptId === undefined ? t.scriptId : patch.scriptId, filePath: patch.filePath === undefined ? t.filePath : patch.filePath } : t)),

    setTableView: (id, view) => patchTab(id, (t) => (t.kind === 'table' ? { ...t, view } : t)),

    run: async (tabId, sql, base, params = null) => {
      const session = get().session;
      if (!session || !sql.trim()) return;
      if (params) patchTab(tabId, (t) => (t.kind === 'query' ? { ...t, paramValues: params.map((p) => p ?? '') } : t));
      const before = (get().tabs.find((t) => t.id === tabId) as QueryTab | undefined)?.run ?? EMPTY_RUN;
      patchRun(tabId, { status: 'running', startedAt: Date.now(), errorAt: null });
      try {
        const report = await guarded((c) => api.runSql({ sessionId: session.session_id }, sql, params, c), {
          // Nothing runs while the confirmation dialog is open.
          onAsk: () => patchRun(tabId, { status: before.status === 'running' ? 'idle' : before.status }),
          onConfirmed: () => patchRun(tabId, { status: 'running', startedAt: Date.now() }),
        });
        if (!report) {
          patchRun(tabId, { ...before, status: before.status === 'running' ? 'idle' : before.status });
          return;
        }
        patchRun(tabId, (run) => ({ status: 'done', ...digest(report, sql, base, run) }));
        set({ tx: report.tx });
        noteLinkErrors(report.outcomes.map((o) => o.error));
      } catch (e) {
        const error = toAppError(e).info;
        noteLinkErrors([error]);
        patchRun(tabId, { status: 'done', active: 'messages', messages: [{ id: uid('msg'), kind: 'error', statement: null, text: error.message, error }] });
      }
    },

    fetchMore: async (tabId) => {
      const session = get().session;
      const tab = get().tabs.find((t): t is QueryTab => t.id === tabId && t.kind === 'query');
      const last = tab?.run.results[tab.run.results.length - 1];
      if (!session || !tab || !last?.canFetchMore) return;
      patchRun(tabId, { fetching: true });
      try {
        const page = await api.fetchMore({ sessionId: session.session_id }, useSettings.getState().settings.row_limit);
        patchRun(tabId, (run) => ({
          fetching: false,
          results: run.results.map((r) => (r.id === last.id ? { ...r, rows: [...r.rows, ...page.rows], hasMore: page.has_more, canFetchMore: page.has_more } : r)),
        }));
      } catch (e) {
        patchRun(tabId, (run) => ({ fetching: false, results: run.results.map((r) => (r.id === last.id ? { ...r, canFetchMore: false } : r)) }));
        noteLinkErrors([toAppError(e).info]);
        useUi.getState().toastError(e);
      }
    },

    cancel: async () => {
      const session = get().session;
      if (!session) return;
      try {
        await api.cancelQuery({ sessionId: session.session_id });
      } catch (e) {
        useUi.getState().toastError(e);
      }
    },

    setActiveResult: (tabId, active) => patchRun(tabId, { active }),
  };
});

/** The backend closed the session (idle timeout); tabs stay and Reconnect reopens it. */
export function noteSessionClosed(sessionId: string, reason: string) {
  const { session } = useWorkspace.getState();
  if (session?.session_id !== sessionId) return;
  useWorkspace.setState({ link: 'disconnected' });
  useUi.getState().toast(reason === 'idle' ? `Disconnected from ${session.connection.name} after the idle timeout. Reconnect to continue.` : `Disconnected from ${session.connection.name}.`);
}

/** A network error means the session's connection dropped; offer to reconnect. */
function noteLinkErrors(errors: (DbError | null)[]) {
  if (errors.some((e) => e?.kind === 'network')) useWorkspace.setState({ link: 'failed' });
}

// ---------------------------------------------------------------------------
// Persistence: tab SQL and layout only. Results are never stored and restored
// tabs never run until the user presses Run.
// ---------------------------------------------------------------------------

type PersistedTab =
  | { kind: 'query'; id: string; title: string; sql: string; savedSql: string; scriptId: string | null; filePath: string | null; bookmarks?: number[] }
  | { kind: 'table'; id: string; title: string; object: ObjectRef; view: TableView }
  | ContainerTab
  | MonitorTab;

interface PersistedWorkspace {
  tabs: PersistedTab[];
  activeTabId: string | null;
  split?: SplitState | null;
}

function restoreTabs(saved: PersistedWorkspace | null): Tab[] {
  if (!saved || !Array.isArray(saved.tabs)) return [];
  return saved.tabs.map((t) => (t.kind === 'query' ? { ...t, bookmarks: t.bookmarks ?? [], paramValues: [], run: EMPTY_RUN } : t));
}

function snapshot(): { key: string; value: PersistedWorkspace } | null {
  const { session, tabs, activeTabId, split } = useWorkspace.getState();
  if (!session) return null;
  return {
    key: `workspace:${session.connection.id}`,
    value: {
      activeTabId,
      split,
      tabs: tabs.map((t): PersistedTab => {
        switch (t.kind) {
          case 'query':
            return { kind: 'query', id: t.id, title: t.title, sql: t.sql, savedSql: t.savedSql, scriptId: t.scriptId, filePath: t.filePath, bookmarks: t.bookmarks };
          case 'table':
            return { kind: 'table', id: t.id, title: t.title, object: t.object, view: t.view };
          default:
            return t;
        }
      }),
    },
  };
}

let timer: number | undefined;
let lastSaved = '';

async function persistNow() {
  window.clearTimeout(timer);
  const snap = snapshot();
  if (!snap) return;
  const serialized = JSON.stringify(snap);
  if (serialized === lastSaved) return;
  lastSaved = serialized;
  await api.setUiState(snap.key, snap.value).catch((e) => console.warn('could not save workspace', e));
}

useWorkspace.subscribe((state, prev) => {
  if (state.tabs === prev.tabs && state.activeTabId === prev.activeTabId && state.split === prev.split) return;
  window.clearTimeout(timer);
  timer = window.setTimeout(() => void persistNow(), 600);
});

window.addEventListener('beforeunload', () => void persistNow());

export function resultSummary(result: ResultSet | undefined): string | null {
  if (!result) return null;
  return `${formatCount(result.rows.length)}${result.hasMore ? '+' : ''} rows`;
}

// ---------------------------------------------------------------------------
// Autosave: tabs opened from a saved script write their SQL back (setting).
// ---------------------------------------------------------------------------

const autosaveTimers = new Map<string, number>();

function scheduleAutosave(tabId: string) {
  if (!useSettings.getState().settings.autosave_scripts) return;
  window.clearTimeout(autosaveTimers.get(tabId));
  autosaveTimers.set(
    tabId,
    window.setTimeout(() => {
      autosaveTimers.delete(tabId);
      const tab = useWorkspace.getState().tabs.find((t): t is QueryTab => t.id === tabId && t.kind === 'query');
      if (!tab?.scriptId || tab.sql === tab.savedSql) return;
      const sql = tab.sql;
      api
        .updateScriptSql(tab.scriptId, sql)
        .then(() => {
          const now = useWorkspace.getState().tabs.find((t): t is QueryTab => t.id === tabId && t.kind === 'query');
          if (now?.sql === sql) useWorkspace.getState().markSaved(tabId, {});
        })
        .catch((e) => console.warn('autosave failed', e));
    }, 1200),
  );
}
