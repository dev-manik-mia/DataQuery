import { Icon } from '@/components/ui';
import { objectLabel, objectPath } from '@/lib/objects';
import { shortcutLabel } from '@/lib/platform';
import { isDirty, useWorkspace } from '@/stores/workspace';

import styles from './Workspace.module.css';

const KIND = { table: 'T', view: 'V', materialized_view: 'M' } as const;

export function TabBar() {
  const { tabs, activeTabId, activate, closeTab, openQueryTab } = useWorkspace();
  return (
    <div className={styles.tabbar} role="tablist" aria-label="Open tabs">
      {tabs.map((tab) => {
        const selected = tab.id === activeTabId;
        return (
          <div
            key={tab.id}
            role="tab"
            tabIndex={selected ? 0 : -1}
            aria-selected={selected}
            className={styles.tab}
            title={tab.kind === 'query' ? (tab.filePath ?? tab.title) : objectPath(tab.object)}
            onClick={() => activate(tab.id)}
            onAuxClick={(e) => e.button === 1 && void closeTab(tab.id)}
            onKeyDown={(e) => e.key === 'Enter' && activate(tab.id)}
          >
            {tab.kind === 'table' && <span className={styles.tabKind}>{KIND[tab.object.kind as keyof typeof KIND] ?? 'T'}</span>}
            <span className="ellipsis">{tab.kind === 'table' ? objectLabel(tab.object) : tab.title}</span>
            {isDirty(tab) && (
              <span className={styles.dirty} aria-label="Unsaved changes">
                ●
              </span>
            )}
            <button
              type="button"
              className={styles.close}
              aria-label={`Close ${tab.title}`}
              title={`Close (${shortcutLabel('mod+w')})`}
              onClick={(e) => {
                e.stopPropagation();
                void closeTab(tab.id);
              }}
            >
              <Icon name="close" size={10} />
            </button>
          </div>
        );
      })}
      <button type="button" className={styles.newTab} aria-label="New query tab" title={`New query (${shortcutLabel('mod+t')})`} onClick={() => openQueryTab()}>
        <Icon name="plus" />
      </button>
    </div>
  );
}
