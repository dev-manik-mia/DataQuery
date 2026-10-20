import { getCurrentWebview } from '@tauri-apps/api/webview';
import { memo, useEffect, type ReactNode } from 'react';
import { useShallow } from 'zustand/react/shallow';

import { Button, EmptyState, Splitter } from '@/components/ui';
import { openSqlFiles } from '@/features/editor/fileActions';
import { QueryTabView } from '@/features/editor/QueryTabView';
import { Explorer } from '@/features/explorer/Explorer';
import { Console } from '@/features/keys/Console';
import { KeyBrowser } from '@/features/keys/KeyBrowser';
import { ProGate } from '@/features/license/ProGate';
import { MonitorView } from '@/features/monitor/MonitorView';
import { DiagramView } from '@/features/schema/DiagramView';
import { MigrationsView } from '@/features/schema/MigrationsView';
import { TableTabView } from '@/features/table/TableTabView';
import { UsersView } from '@/features/users/UsersView';
import { t } from '@/i18n';
import { shortcutLabel } from '@/lib/platform';
import { LAYOUT_LIMITS, useLayout } from '@/stores/layout';
import { useWorkspace, type Tab } from '@/stores/workspace';

import { TabBar } from './TabBar';
import styles from './Workspace.module.css';

/** One tab's view; memoized so typing in one tab does not re-render the others. */
const TabContent = memo(function TabContent({ tab, active }: { tab: Tab; active: boolean }) {
  switch (tab.kind) {
    case 'query':
      return <QueryTabView tab={tab} active={active} />;
    case 'table':
      return <TableTabView tab={tab} active={active} />;
    case 'diagram':
      return <DiagramView tab={tab} active={active} />;
    case 'migrations':
      return <MigrationsView tab={tab} active={active} />;
    case 'console':
      return <Console full />;
    case 'monitor':
      return (
        <ProGate feature="Monitoring" summary="Watch the server live: connections against the limit, running queries you can cancel or terminate, locks and blocking chains, replication lag, server settings and the largest tables.">
          <MonitorView active={active} />
        </ProGate>
      );
    case 'users':
      return <UsersView active={active} />;
  }
});

/**
 * Explorer, tab bar and the open tabs. Every tab stays mounted (editor state
 * survives switching); with a split, each tab is placed in the left or right
 * column of one grid, so moving it between panes never remounts it.
 */
export function Workspace() {
  const { tabs, activeTabId, split, openQueryTab, focusPane } = useWorkspace(useShallow((s) => ({ tabs: s.tabs, activeTabId: s.activeTabId, split: s.split, openQueryTab: s.openQueryTab, focusPane: s.focusPane })));
  const { explorerWidth, set } = useLayout(useShallow((s) => ({ explorerWidth: s.explorerWidth, set: s.set })));
  // Key-value engines browse keys with SCAN instead of an object tree (screen 12).
  const keyValue = useWorkspace((s) => s.session?.keys != null);

  // Drop .sql files anywhere on the window to open them.
  useEffect(() => {
    const unlisten = getCurrentWebview().onDragDropEvent((event) => {
      if (event.payload.type !== 'drop') return;
      const files = event.payload.paths.filter((p) => /\.(sql|txt)$/i.test(p));
      if (files.length) void openSqlFiles(files);
    });
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, []);

  const slot = (tab: Tab): ReactNode => {
    const right = split?.rightTabId === tab.id;
    const visible = right || tab.id === activeTabId;
    return (
      <div
        key={tab.id}
        className={styles.slot}
        style={{ gridColumn: right ? 3 : 1, display: visible ? 'flex' : 'none' }}
        data-focused={split ? (split.focus === (right ? 'right' : 'left')) : undefined}
        onMouseDownCapture={() => split && focusPane(right ? 'right' : 'left')}
      >
        <TabContent tab={tab} active={visible} />
      </div>
    );
  };

  return (
    <div className={styles.workspace}>
      {keyValue ? <KeyBrowser width={explorerWidth} /> : <Explorer width={explorerWidth} />}
      <Splitter axis="x" label="Resize explorer" size={explorerWidth} min={LAYOUT_LIMITS.explorer.min} max={LAYOUT_LIMITS.explorer.max} onResize={(w) => set({ explorerWidth: w })} />
      <main className={styles.main}>
        <TabBar />
        <div className={styles.panes} style={{ gridTemplateColumns: split ? 'minmax(0, 1fr) 1px minmax(0, 1fr)' : 'minmax(0, 1fr)' }}>
          {tabs.map(slot)}
          {split && <div className={styles.paneDivider} style={{ gridColumn: 2 }} aria-hidden />}
          {split && !split.rightTabId && (
            <div className={styles.slot} style={{ gridColumn: 3 }}>
              <EmptyState title={t('workspace.emptyPane')}>{t('workspace.emptyPaneHint')}</EmptyState>
            </div>
          )}
        </div>
        {tabs.length === 0 && (
          <EmptyState title={t('workspace.noTabs')}>
            {t('workspace.noTabsHint')}
            <div style={{ marginTop: 12 }}>
              <Button variant="primary" shortcut={shortcutLabel('mod+t')} onClick={() => openQueryTab()}>
                {t('action.newQuery')}
              </Button>
            </div>
          </EmptyState>
        )}
      </main>
    </div>
  );
}
