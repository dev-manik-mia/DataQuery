import { Icon, Menu, useContextMenu, type MenuEntry } from '@/components/ui';
import { t } from '@/i18n';
import { KIND_MARK, objectLabel, objectPath } from '@/lib/objects';
import { shortcutLabel } from '@/lib/platform';
import { isDirty, useWorkspace, type Tab } from '@/stores/workspace';

import styles from './Workspace.module.css';

const CONTAINER_MARK = { diagram: 'ER', migrations: 'Mg', monitor: '≈' } as const;

function tabLabel(tab: Tab): string {
  return tab.kind === 'table' ? objectLabel(tab.object) : tab.title;
}

function tabTitle(tab: Tab): string {
  switch (tab.kind) {
    case 'query':
      return tab.filePath ?? tab.title;
    case 'table':
      return objectPath(tab.object);
    default:
      return tab.title;
  }
}

export function TabBar() {
  const { tabs, activeTabId, split, activate, closeTab, openQueryTab, moveToOtherPane, toggleSplit } = useWorkspace();
  const context = useContextMenu<Tab>();

  const menu = (tab: Tab): MenuEntry[] => [
    { label: t('action.moveToOtherPane'), onSelect: () => moveToOtherPane(tab.id) },
    { label: split ? t('action.closeSplit') : t('action.splitEditor'), onSelect: toggleSplit },
    { type: 'separator' },
    { label: t('action.close'), hint: shortcutLabel('mod+w'), onSelect: () => void closeTab(tab.id) },
  ];

  return (
    <div className={styles.tabbar} role="tablist" aria-label="Open tabs">
      {tabs.map((tab) => {
        const right = split?.rightTabId === tab.id;
        const selected = tab.id === activeTabId || right;
        return (
          <div
            key={tab.id}
            role="tab"
            tabIndex={selected ? 0 : -1}
            aria-selected={selected}
            className={styles.tab}
            title={tabTitle(tab)}
            onClick={() => activate(tab.id)}
            onAuxClick={(e) => e.button === 1 && void closeTab(tab.id)}
            onKeyDown={(e) => e.key === 'Enter' && activate(tab.id)}
            onContextMenu={(e) => context.open(e, tab)}
          >
            {tab.kind === 'table' && <span className={styles.tabKind}>{KIND_MARK[tab.object.kind] || 'T'}</span>}
            {tab.kind !== 'table' && tab.kind !== 'query' && <span className={styles.tabKind}>{CONTAINER_MARK[tab.kind]}</span>}
            <span className="ellipsis">{tabLabel(tab)}</span>
            {split && selected && (
              <span className={styles.tabPane} aria-label={right ? 'Right pane' : 'Left pane'}>
                {right ? '▐' : '▌'}
              </span>
            )}
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
      <button type="button" className={styles.newTab} aria-label={split ? t('action.closeSplit') : t('action.splitEditor')} title={split ? t('action.closeSplit') : t('action.splitEditor')} onClick={toggleSplit} style={{ marginLeft: 'auto' }}>
        <Icon name="split" />
      </button>
      {context.menu && <Menu x={context.menu.x} y={context.menu.y} items={menu(context.menu.target)} onClose={context.close} label="Tab actions" />}
    </div>
  );
}
