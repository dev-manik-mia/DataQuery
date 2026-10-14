import { WebviewWindow } from '@tauri-apps/api/webviewWindow';

import { t } from '@/i18n';
import { uid } from '@/lib/id';
import { useSettings } from '@/stores/settings';
import { useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

/** Actions on the active query tab, handled by `QueryTabView`. */
export type QueryAction = 'statement' | 'script' | 'explain' | 'explain-analyze' | 'format' | 'save' | 'ai' | 'bookmark' | 'next-bookmark' | 'prev-bookmark';
export const QUERY_EVENT = 'dq:query';

export function queryAction(action: QueryAction) {
  window.dispatchEvent(new CustomEvent<QueryAction>(QUERY_EVENT, { detail: action }));
}

export interface AppCommand {
  id: string;
  title: string;
  group: string;
  /** Shown and bound only when available (e.g. needs a session, or an engine capability). */
  available: boolean;
  run: () => void;
}

/** Default shortcuts; users override them in Settings → Keyboard. */
export const DEFAULT_KEYS: Record<string, string> = {
  'query.runStatement': 'mod+enter',
  'query.runScript': 'shift+mod+enter',
  'query.explain': 'mod+e',
  'query.format': 'shift+alt+f',
  'query.save': 'mod+s',
  'query.bookmark': 'mod+f2',
  'query.nextBookmark': 'f2',
  'query.prevBookmark': 'shift+f2',
  'tab.new': 'mod+t',
  'tab.close': 'mod+w',
  'search.open': 'mod+k',
  'palette.open': 'shift+mod+p',
  'window.new': 'shift+mod+n',
  'editor.split': 'mod+\\',
};

/** The shortcut bound to a command: the user's override, else the default. Empty means unbound. */
export function commandKeys(id: string): string | undefined {
  const custom = useSettings.getState().settings.keymap[id];
  if (custom !== undefined) return custom || undefined;
  return DEFAULT_KEYS[id];
}

let windows = 0;

/** Opens another app window with its own workspace. */
export function openNewWindow() {
  windows += 1;
  const label = `window-${uid('w').replace(/[^a-zA-Z0-9-]/g, '')}`;
  const w = new WebviewWindow(label, {
    url: '/',
    title: 'DataQuery',
    width: 1280,
    height: 820,
    minWidth: 640,
    minHeight: 620,
    x: 80 + windows * 24,
    y: 80 + windows * 24,
    titleBarStyle: 'overlay',
    hiddenTitle: true,
  });
  void w.once('tauri://error', (e) => useUi.getState().toast(`Could not open a window: ${String(e.payload)}`, 'critical'));
}

/** Every app action, for the command palette and keyboard shortcuts. */
export function appCommands(): AppCommand[] {
  const ui = useUi.getState();
  const ws = useWorkspace.getState();
  const session = ws.session;
  const inWorkspace = Boolean(session) && ui.view.name === 'workspace';
  const activeQuery = inWorkspace && ws.tabs.find((tab) => tab.id === (ws.split?.focus === 'right' ? ws.split.rightTabId : ws.activeTabId))?.kind === 'query';
  const caps = session?.capabilities;
  const settings = useSettings.getState();
  return [
    { id: 'query.runStatement', title: t('action.runStatement'), group: 'Query', available: activeQuery, run: () => queryAction('statement') },
    { id: 'query.runScript', title: t('action.runScript'), group: 'Query', available: activeQuery, run: () => queryAction('script') },
    { id: 'query.explain', title: t('action.explain'), group: 'Query', available: activeQuery && Boolean(caps?.explain), run: () => queryAction('explain') },
    { id: 'query.explainAnalyze', title: t('action.explainAnalyze'), group: 'Query', available: activeQuery && Boolean(caps?.explain), run: () => queryAction('explain-analyze') },
    { id: 'query.format', title: t('action.format'), group: 'Query', available: activeQuery, run: () => queryAction('format') },
    { id: 'query.save', title: 'Save script or file', group: 'Query', available: activeQuery, run: () => queryAction('save') },
    { id: 'query.ai', title: 'Ask the AI assistant', group: 'Query', available: activeQuery && settings.settings.ai_enabled, run: () => queryAction('ai') },
    { id: 'query.bookmark', title: 'Toggle bookmark', group: 'Query', available: activeQuery, run: () => queryAction('bookmark') },
    { id: 'query.nextBookmark', title: 'Next bookmark', group: 'Query', available: activeQuery, run: () => queryAction('next-bookmark') },
    { id: 'query.prevBookmark', title: 'Previous bookmark', group: 'Query', available: activeQuery, run: () => queryAction('prev-bookmark') },
    { id: 'tx.begin', title: `${t('action.begin')} transaction`, group: 'Transaction', available: inWorkspace && Boolean(caps?.transactions) && ws.tx === 'auto', run: () => void ws.transaction('begin') },
    { id: 'tx.commit', title: t('action.commit'), group: 'Transaction', available: inWorkspace && ws.tx === 'open', run: () => void ws.transaction('commit') },
    { id: 'tx.rollback', title: t('action.rollback'), group: 'Transaction', available: inWorkspace && ws.tx !== 'auto', run: () => void ws.transaction('rollback') },
    {
      id: 'tx.manual',
      title: settings.settings.manual_commit ? 'Switch to auto-commit' : 'Switch to manual commit',
      group: 'Transaction',
      available: Boolean(caps?.transactions) || !session,
      run: () => void settings.update({ manual_commit: !settings.settings.manual_commit }),
    },
    { id: 'tab.new', title: t('action.newQuery'), group: 'Tabs', available: inWorkspace, run: () => ws.openQueryTab() },
    { id: 'tab.close', title: 'Close tab', group: 'Tabs', available: inWorkspace && Boolean(ws.activeTabId), run: () => ws.activeTabId && void ws.closeTab(ws.split?.focus === 'right' && ws.split.rightTabId ? ws.split.rightTabId : ws.activeTabId) },
    { id: 'editor.split', title: ws.split ? t('action.closeSplit') : t('action.splitEditor'), group: 'Tabs', available: inWorkspace, run: () => ws.toggleSplit() },
    { id: 'window.new', title: t('action.newWindow'), group: 'Window', available: true, run: openNewWindow },
    { id: 'search.open', title: t('nav.search'), group: 'Navigate', available: true, run: () => ui.setPaletteOpen(true) },
    { id: 'palette.open', title: t('nav.commandPalette'), group: 'Navigate', available: true, run: () => ui.setCommandsOpen(true) },
    { id: 'nav.connections', title: t('nav.connections'), group: 'Navigate', available: true, run: () => ui.navigate({ name: 'home' }) },
    { id: 'nav.history', title: t('nav.history'), group: 'Navigate', available: true, run: () => ui.navigate({ name: 'history' }) },
    { id: 'nav.schemaDiff', title: t('nav.schemaDiff'), group: 'Navigate', available: true, run: () => ui.navigate({ name: 'diff' }) },
    { id: 'nav.monitor', title: t('nav.monitoring'), group: 'Navigate', available: Boolean(caps?.monitoring), run: () => (ui.navigate({ name: 'workspace' }), ws.openMonitorTab()) },
    { id: 'nav.workspace', title: `Back to ${session?.connection.name ?? 'workspace'}`, group: 'Navigate', available: Boolean(session) && !inWorkspace, run: () => ui.navigate({ name: 'workspace' }) },
    { id: 'conn.reconnect', title: t('action.reconnect'), group: 'Connection', available: Boolean(session), run: () => void ws.reconnect() },
    { id: 'conn.disconnect', title: t('action.disconnect'), group: 'Connection', available: Boolean(session), run: () => void ws.disconnect().then(() => ui.navigate({ name: 'home' })) },
    ...(['general', 'editor', 'keyboard', 'results', 'history', 'security', 'ai', 'engines', 'about'] as const).map((section) => ({
      id: `settings.${section}`,
      title: `Settings: ${t(`settings.${section}`)}`,
      group: 'Settings',
      available: true,
      run: () => ui.navigate({ name: 'settings', section }),
    })),
    ...(['dark', 'light', 'system'] as const).map((theme) => ({
      id: `theme.${theme}`,
      title: `Theme: ${theme[0]!.toUpperCase()}${theme.slice(1)}`,
      group: 'Appearance',
      available: settings.settings.theme !== theme,
      run: () => void settings.update({ theme }),
    })),
  ];
}
