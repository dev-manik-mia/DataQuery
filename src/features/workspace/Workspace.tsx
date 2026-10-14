import { getCurrentWebview } from '@tauri-apps/api/webview';
import { useEffect } from 'react';

import { Button, EmptyState, Splitter } from '@/components/ui';
import { openSqlFiles } from '@/features/editor/fileActions';
import { QueryTabView } from '@/features/editor/QueryTabView';
import { Explorer } from '@/features/explorer/Explorer';
import { TableTabView } from '@/features/table/TableTabView';
import { shortcutLabel } from '@/lib/platform';
import { LAYOUT_LIMITS, useLayout } from '@/stores/layout';
import { useWorkspace } from '@/stores/workspace';

import { TabBar } from './TabBar';
import styles from './Workspace.module.css';

/** Explorer, tab bar and the open tabs. Inactive tabs stay mounted to keep editor state. */
export function Workspace() {
  const { tabs, activeTabId, openQueryTab } = useWorkspace();
  const { explorerWidth, set } = useLayout();

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

  return (
    <div className={styles.workspace}>
      <Explorer width={explorerWidth} />
      <Splitter axis="x" label="Resize explorer" size={explorerWidth} min={LAYOUT_LIMITS.explorer.min} max={LAYOUT_LIMITS.explorer.max} onResize={(w) => set({ explorerWidth: w })} />
      <main className={styles.main}>
        <TabBar />
        {tabs.map((tab) =>
          tab.kind === 'query' ? <QueryTabView key={tab.id} tab={tab} active={tab.id === activeTabId} /> : <TableTabView key={tab.id} tab={tab} active={tab.id === activeTabId} />,
        )}
        {tabs.length === 0 && (
          <EmptyState title="No open tabs">
            Open a table from the explorer or start a new query.
            <div style={{ marginTop: 12 }}>
              <Button variant="primary" shortcut={shortcutLabel('mod+t')} onClick={() => openQueryTab()}>
                New query
              </Button>
            </div>
          </EmptyState>
        )}
      </main>
    </div>
  );
}
