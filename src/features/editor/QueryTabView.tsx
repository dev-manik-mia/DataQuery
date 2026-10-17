import { useEffect, useRef, useState } from 'react';
import { useShallow } from 'zustand/react/shallow';

import { Button, Menu, PromptDialog, Splitter, type MenuEntry } from '@/components/ui';
import { AiPanel } from '@/features/ai/AiPanel';
import { commandKeys, QUERY_EVENT, type QueryAction } from '@/features/commands/commands';
import { ResultsPanel } from '@/features/results/ResultsPanel';
import { t } from '@/i18n';
import { formatCount } from '@/lib/format';
import { api } from '@/lib/ipc';
import { shortcutLabel } from '@/lib/platform';
import { useCompletion } from '@/stores/completion';
import { LAYOUT_LIMITS, useLayout } from '@/stores/layout';
import { useSettings } from '@/stores/settings';
import { useUi } from '@/stores/ui';
import { useWorkspace, type QueryTab } from '@/stores/workspace';

import { openSqlFiles, saveTabAsScript, saveTabToFile } from './fileActions';
import type { QueryAnalysis } from '@/bindings/QueryAnalysis';
import { ParamsDialog } from './ParamsDialog';
import { AnalysisBanner, SessionOptions } from './SessionBar';
import styles from './QueryTab.module.css';
import { SqlEditor, type SqlEditorHandle, type StatementTarget } from './SqlEditor';

type Pending = { target: StatementTarget; mode: 'run' | 'explain' | 'explain-analyze' };

/** One SQL tab: toolbar, editor, results and the optional AI assistant panel. */
export function QueryTabView({ tab, active }: { tab: QueryTab; active: boolean }) {
  const editor = useRef<SqlEditorHandle>(null);
  const session = useWorkspace((s) => s.session);
  const tx = useWorkspace((s) => s.tx);
  const { updateSql, run, cancel, explain, transaction, setBookmarks } = useWorkspace(useShallow((s) => ({ updateSql: s.updateSql, run: s.run, cancel: s.cancel, explain: s.explain, transaction: s.transaction, setBookmarks: s.setBookmarks })));
  const settings = useSettings((s) => s.settings);
  const navigate = useUi((s) => s.navigate);
  const { editorHeight, set: setLayout } = useLayout(useShallow((s) => ({ editorHeight: s.editorHeight, set: s.set })));
  const { model, snippets } = useCompletion();
  const [naming, setNaming] = useState(false);
  const [menu, setMenu] = useState<{ x: number; y: number; items: MenuEntry[]; label: string } | null>(null);
  const [pending, setPending] = useState<Pending | null>(null);
  const [aiOpen, setAiOpen] = useState(false);
  const [analysis, setAnalysis] = useState<QueryAnalysis | null>(null);
  const running = tab.run.status === 'running';
  const flavor = session?.flavor ?? 'postgres';
  const caps = session?.capabilities;

  // Load the catalog snapshot for autocomplete once per session.
  useEffect(() => {
    if (session && active) void useCompletion.getState().load(session.session_id, session.server.database);
  }, [session, active]);

  // CQL: classify the statement at the cursor (single partition, several, scan) as the user types.
  const sessionId = session?.session_id;
  useEffect(() => {
    if (!sessionId || session?.flavor !== 'cql' || !active) return;
    let stale = false;
    const timer = window.setTimeout(() => {
      void editor.current
        ?.statementTarget()
        .then((target) => (target?.sql.trim() ? api.queryAnalysis({ sessionId }, target.sql) : null))
        .then((a) => !stale && setAnalysis(a ?? null))
        .catch(() => !stale && setAnalysis(null));
    }, 400);
    return () => {
      stale = true;
      window.clearTimeout(timer);
    };
  }, [tab.sql, sessionId, session?.flavor, active]);

  const execute = async (target: StatementTarget, mode: Pending['mode'], params: (string | null)[] | null) => {
    if (mode === 'run') await run(tab.id, target.sql, target.base, params);
    else await explain(tab.id, target.sql, params, mode === 'explain-analyze');
  };

  /** Runs or explains the statement at the cursor, asking for parameter values first. */
  const withTarget = async (mode: Pending['mode']) => {
    if (running) return;
    const target = await editor.current?.statementTarget();
    if (!target?.sql.trim()) return;
    if (target.paramCount > 0) setPending({ target, mode });
    else await execute(target, mode, null);
  };

  const runScript = async () => {
    if (running) return;
    const target = editor.current?.scriptTarget();
    if (target?.sql.trim()) await run(tab.id, target.sql, target.base);
  };

  const format = async () => {
    try {
      updateSql(tab.id, await api.formatSql(tab.sql, flavor));
    } catch (e) {
      useUi.getState().toastError(e);
    }
  };

  const saveTab = () => {
    if (tab.filePath) void saveTabToFile(tab);
    else if (tab.scriptId) void saveTabAsScript(tab, tab.title);
    else setNaming(true);
  };

  // Commands from the palette and shortcuts pressed outside the editor.
  useEffect(() => {
    if (!active) return;
    const onAction = (e: Event) => {
      const action = (e as CustomEvent<QueryAction>).detail;
      switch (action) {
        case 'statement':
          return void withTarget('run');
        case 'script':
          return void runScript();
        case 'explain':
          return void withTarget('explain');
        case 'explain-analyze':
          return void withTarget('explain-analyze');
        case 'format':
          return void format();
        case 'save':
          return saveTab();
        case 'ai':
          return setAiOpen(true);
        case 'bookmark':
          return editor.current?.toggleBookmark();
        case 'next-bookmark':
          return editor.current?.jumpBookmark(1);
        case 'prev-bookmark':
          return editor.current?.jumpBookmark(-1);
      }
    };
    window.addEventListener(QUERY_EVENT, onAction);
    return () => window.removeEventListener(QUERY_EVENT, onAction);
  });

  const openMenu = (e: React.MouseEvent<HTMLElement>, items: MenuEntry[], label: string) => {
    const r = e.currentTarget.getBoundingClientRect();
    setMenu({ x: r.left, y: r.bottom + 4, items, label });
  };

  const keyHint = (id: string) => {
    const k = commandKeys(id);
    return k ? shortcutLabel(k) : undefined;
  };

  const fileMenu: MenuEntry[] = [
    { label: 'Save', hint: keyHint('query.save'), onSelect: saveTab },
    { label: 'Save as script…', onSelect: () => setNaming(true) },
    { label: 'Save as .sql file…', onSelect: () => void saveTabToFile(tab, true) },
    { type: 'separator' },
    { label: 'Open .sql file…', onSelect: () => void openSqlFiles() },
    { type: 'separator' },
    { label: 'Toggle bookmark', hint: keyHint('query.bookmark'), onSelect: () => editor.current?.toggleBookmark() },
    { label: 'Next bookmark', hint: keyHint('query.nextBookmark'), disabled: !tab.bookmarks.length, onSelect: () => editor.current?.jumpBookmark(1) },
  ];

  const explainMenu: MenuEntry[] = [
    { label: 'Explain', hint: keyHint('query.explain'), onSelect: () => void withTarget('explain') },
    { label: 'Explain analyze (runs read-only statements)', onSelect: () => void withTarget('explain-analyze') },
  ];

  const editorStyle = {
    height: editorHeight,
    ['--editor-font-size' as string]: `${settings.editor_font_size}px`,
    ['--editor-font-family' as string]: `"${settings.editor_font_family}", var(--font-mono)`,
    ['--editor-line-height' as string]: String(settings.editor_line_height / 100),
    ['--editor-ligatures' as string]: settings.editor_ligatures ? 'normal' : 'none',
  };

  return (
    <div className={styles.tab} hidden={!active} style={active ? undefined : { display: 'none' }}>
      <div className={styles.toolbar} role="toolbar" aria-label="Query">
        <Button variant="primary" size="sm" shortcut={keyHint('query.runStatement')} disabled={running} onClick={() => void withTarget('run')}>
          {t('action.runStatement')}
        </Button>
        <Button size="sm" shortcut={keyHint('query.runScript')} disabled={running} onClick={() => void runScript()}>
          {t('action.runScript')}
        </Button>
        <Button size="sm" disabled={!running} onClick={() => void cancel()}>
          {t('action.cancel')}
        </Button>
        {caps?.explain && (
          <Button size="sm" disabled={running} onClick={(e) => openMenu(e, explainMenu, 'Explain')}>
            {t('action.explain')} ▾
          </Button>
        )}
        <Button size="sm" disabled={!tab.sql.trim()} onClick={() => void format()}>
          {t('action.format')}
        </Button>
        <Button size="sm" onClick={(e) => openMenu(e, fileMenu, 'File')}>
          File
        </Button>
        {caps?.transactions && (
          <span className={styles.txGroup} aria-label="Transaction">
            {tx === 'auto' ? (
              <Button size="sm" title="Start a transaction on this connection" onClick={() => void transaction('begin')}>
                {t('action.begin')}
              </Button>
            ) : (
              <>
                <Button size="sm" variant="primary" disabled={tx === 'failed'} onClick={() => void transaction('commit')}>
                  {t('action.commit')}
                </Button>
                <Button size="sm" variant="caution" onClick={() => void transaction('rollback')}>
                  {t('action.rollback')}
                </Button>
              </>
            )}
          </span>
        )}
        <span style={{ flex: 1 }} />
        {sessionId && <SessionOptions sessionId={sessionId} />}
        {settings.ai_enabled && (
          <Button size="sm" aria-pressed={aiOpen} onClick={() => setAiOpen((o) => !o)}>
            Assistant
          </Button>
        )}
        <button type="button" className={styles.meta} onClick={() => navigate({ name: 'settings', section: 'results' })} title="Change in Settings">
          Row limit <span className={styles.metaValue}>{formatCount(settings.row_limit)}</span>
        </button>
        <button type="button" className={styles.meta} onClick={() => navigate({ name: 'settings', section: 'results' })} title="Change in Settings">
          Timeout <span className={styles.metaValue}>{settings.timeout_secs ? `${settings.timeout_secs}s` : 'off'}</span>
        </button>
      </div>
      <div className={styles.body}>
        <div className={styles.main}>
          <div className={styles.editor} style={editorStyle}>
            <SqlEditor
              ref={editor}
              value={tab.sql}
              flavor={flavor}
              errorAt={tab.run.errorAt}
              active={active}
              keys={{
                runStatement: commandKeys('query.runStatement'),
                runScript: commandKeys('query.runScript'),
                explain: caps?.explain ? commandKeys('query.explain') : undefined,
                save: commandKeys('query.save'),
                bookmark: commandKeys('query.bookmark'),
                nextBookmark: commandKeys('query.nextBookmark'),
                prevBookmark: commandKeys('query.prevBookmark'),
              }}
              model={model}
              snippets={snippets}
              bookmarks={tab.bookmarks}
              onChange={(sql) => updateSql(tab.id, sql)}
              onBookmarks={(lines) => setBookmarks(tab.id, lines)}
              onRunStatement={() => void withTarget('run')}
              onRunScript={() => void runScript()}
              onExplain={() => void withTarget('explain')}
              onSave={saveTab}
            />
          </div>
          <AnalysisBanner analysis={analysis} />
          <Splitter
            axis="y"
            label="Resize editor"
            size={editorHeight}
            min={LAYOUT_LIMITS.editor.min}
            max={Math.min(LAYOUT_LIMITS.editor.max, window.innerHeight - 260)}
            onResize={(h) => setLayout({ editorHeight: h })}
          />
          <ResultsPanel tab={tab} />
        </div>
        {aiOpen && settings.ai_enabled && <AiPanel tab={tab} onClose={() => setAiOpen(false)} onInsert={(sql) => editor.current?.insert(sql)} />}
      </div>
      {menu && <Menu x={menu.x} y={menu.y} items={menu.items} onClose={() => setMenu(null)} label={menu.label} />}
      {pending && (
        <ParamsDialog
          count={pending.target.paramCount}
          flavor={flavor}
          initial={tab.paramValues}
          sql={pending.target.sql}
          onClose={() => setPending(null)}
          onRun={(values) => {
            const p = pending;
            setPending(null);
            void execute(p.target, p.mode, values);
          }}
        />
      )}
      {naming && (
        <PromptDialog
          title="Save as script"
          label="Script name"
          initial={tab.scriptId ? tab.title : /^Query \d+$/.test(tab.title) ? '' : tab.title}
          confirmLabel="Save script"
          hint="Saved scripts are stored locally and listed under History → Saved scripts."
          onClose={() => setNaming(false)}
          onSubmit={(name) => saveTabAsScript(tab, name)}
        />
      )}
    </div>
  );
}
