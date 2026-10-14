import { useEffect, useRef, useState } from 'react';

import { Button, Menu, PromptDialog, Splitter, type MenuEntry } from '@/components/ui';
import { ResultsPanel } from '@/features/results/ResultsPanel';
import { formatCount } from '@/lib/format';
import { api } from '@/lib/ipc';
import { shortcutLabel } from '@/lib/platform';
import { LAYOUT_LIMITS, useLayout } from '@/stores/layout';
import { useSettings } from '@/stores/settings';
import { useUi } from '@/stores/ui';
import { useWorkspace, type QueryTab } from '@/stores/workspace';

import { openSqlFiles, saveTabAsScript, saveTabToFile } from './fileActions';
import styles from './QueryTab.module.css';
import { SqlEditor, type SqlEditorHandle } from './SqlEditor';

/** One SQL tab: toolbar, editor and results. */
export function QueryTabView({ tab, active }: { tab: QueryTab; active: boolean }) {
  const editor = useRef<SqlEditorHandle>(null);
  const session = useWorkspace((s) => s.session);
  const { updateSql, run, cancel } = useWorkspace();
  const settings = useSettings((s) => s.settings);
  const navigate = useUi((s) => s.navigate);
  const { editorHeight, set: setLayout } = useLayout();
  const [naming, setNaming] = useState(false);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const running = tab.run.status === 'running';
  const flavor = session?.flavor ?? 'postgres';

  const runStatement = async () => {
    if (running) return;
    const target = await editor.current?.statementTarget();
    if (target?.sql.trim()) await run(tab.id, target.sql, target.base);
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

  // ⌘↵ / ⇧⌘↵ pressed outside the editor (e.g. in the results grid).
  useEffect(() => {
    if (!active) return;
    const onRun = (e: Event) => void ((e as CustomEvent<'statement' | 'script'>).detail === 'script' ? runScript() : runStatement());
    window.addEventListener('dq:run', onRun);
    return () => window.removeEventListener('dq:run', onRun);
  });

  const fileMenu: MenuEntry[] = [
    { label: 'Save', hint: shortcutLabel('mod+s'), onSelect: saveTab },
    { label: 'Save as script…', onSelect: () => setNaming(true) },
    { label: 'Save as .sql file…', onSelect: () => void saveTabToFile(tab, true) },
    { type: 'separator' },
    { label: 'Open .sql file…', onSelect: () => void openSqlFiles() },
  ];

  return (
    <div className={styles.tab} hidden={!active} style={active ? undefined : { display: 'none' }}>
      <div className={styles.toolbar} role="toolbar" aria-label="Query">
        <Button variant="primary" size="sm" shortcut={shortcutLabel('mod+enter')} disabled={running} onClick={() => void runStatement()}>
          Run statement
        </Button>
        <Button size="sm" shortcut={shortcutLabel('shift+mod+enter')} disabled={running} onClick={() => void runScript()}>
          Run script
        </Button>
        <Button size="sm" disabled={!running} onClick={() => void cancel()}>
          Cancel
        </Button>
        <Button size="sm" disabled={!tab.sql.trim()} onClick={() => void format()}>
          Format
        </Button>
        <Button
          size="sm"
          onClick={(e) => {
            const r = e.currentTarget.getBoundingClientRect();
            setMenu({ x: r.left, y: r.bottom + 4 });
          }}
        >
          File
        </Button>
        <span style={{ flex: 1 }} />
        <button type="button" className={styles.meta} onClick={() => navigate({ name: 'settings', section: 'results' })} title="Change in Settings">
          Row limit <span className={styles.metaValue}>{formatCount(settings.row_limit)}</span>
        </button>
        <button type="button" className={styles.meta} onClick={() => navigate({ name: 'settings', section: 'results' })} title="Change in Settings">
          Timeout <span className={styles.metaValue}>{settings.timeout_secs ? `${settings.timeout_secs}s` : 'off'}</span>
        </button>
      </div>
      <div className={styles.editor} style={{ height: editorHeight, ['--editor-font-size' as string]: `${settings.editor_font_size}px` }}>
        <SqlEditor
          ref={editor}
          value={tab.sql}
          flavor={flavor}
          errorAt={tab.run.errorAt}
          active={active}
          onChange={(sql) => updateSql(tab.id, sql)}
          onRunStatement={() => void runStatement()}
          onRunScript={() => void runScript()}
          onSave={saveTab}
        />
      </div>
      <Splitter
        axis="y"
        label="Resize editor"
        size={editorHeight}
        min={LAYOUT_LIMITS.editor.min}
        max={Math.min(LAYOUT_LIMITS.editor.max, window.innerHeight - 260)}
        onResize={(h) => setLayout({ editorHeight: h })}
      />
      <ResultsPanel tab={tab} />
      {menu && <Menu x={menu.x} y={menu.y} items={fileMenu} onClose={() => setMenu(null)} label="File" />}
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
