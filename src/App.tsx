import { listen } from '@tauri-apps/api/event';
import { useEffect, useState } from 'react';

import { GlobalDialogs } from '@/components/shell/GlobalDialogs';
import styles from '@/components/shell/Shell.module.css';
import { StatusBar, StatusIndicator } from '@/components/shell/StatusBar';
import { Toasts } from '@/components/shell/Toasts';
import { Crumbs, TitleBar, TitleDivider } from '@/components/shell/TitleBar';
import { Button, Chip, EngineMark, EnvironmentChip, Icon, Menu } from '@/components/ui';
import { ConnectionForm } from '@/features/connections/ConnectionForm';
import { ConnectionsHome } from '@/features/connections/ConnectionsHome';
import { EnginePicker } from '@/features/connections/EnginePicker';
import { useEditorStatus } from '@/features/editor/editorStatus';
import { HistoryPage } from '@/features/history/HistoryPage';
import { SearchPalette } from '@/features/search/SearchPalette';
import { SettingsPage } from '@/features/settings/SettingsPage';
import { Workspace } from '@/features/workspace/Workspace';
import wsStyles from '@/features/workspace/Workspace.module.css';
import { formatCount, formatDuration } from '@/lib/format';
import { matchShortcut, shortcutLabel } from '@/lib/platform';
import { useConnections, useEngine } from '@/stores/connections';
import { useLayout } from '@/stores/layout';
import { applyTheme, useSettings } from '@/stores/settings';
import { useUi, type View } from '@/stores/ui';
import { noteSessionClosed, useWorkspace } from '@/stores/workspace';

export const RUN_EVENT = 'dq:run';
export type RunMode = 'statement' | 'script';

export function App() {
  const view = useUi((s) => s.view);
  const paletteOpen = useUi((s) => s.paletteOpen);
  const session = useWorkspace((s) => s.session);
  const theme = useSettings((s) => s.settings.theme);

  useEffect(() => {
    void useSettings.getState().load();
    void useConnections.getState().load();
    void useLayout.getState().load();
    const unlisten = listen<{ session_id: string; reason: string }>('session-closed', (e) => noteSessionClosed(e.payload.session_id, e.payload.reason));
    return () => void unlisten.then((fn) => fn());
  }, []);

  useEffect(() => {
    applyTheme(theme);
    if (theme !== 'system') return;
    const media = window.matchMedia('(prefers-color-scheme: dark)');
    const onChange = () => applyTheme('system');
    media.addEventListener('change', onChange);
    return () => media.removeEventListener('change', onChange);
  }, [theme]);

  useGlobalShortcuts(view);

  const production = Boolean(session && session.connection.environment === 'Production' && view.name === 'workspace');

  return (
    <div className={styles.app}>
      {production && <div className={styles.productionRule} aria-hidden />}
      <AppTitleBar view={view} production={production} />
      <div style={{ flex: 1, display: 'flex', flexDirection: 'column', minHeight: 0, minWidth: 0 }}>
        {session && (
          <div style={{ flex: 1, display: view.name === 'workspace' ? 'flex' : 'none', minHeight: 0, minWidth: 0 }}>
            <Workspace />
          </div>
        )}
        {view.name === 'home' && <ConnectionsHome />}
        {view.name === 'engines' && <EnginePicker />}
        {view.name === 'connection-form' && <ConnectionForm key={`${view.engine}:${view.connectionId ?? 'new'}`} engine={view.engine} connectionId={view.connectionId} prefill={view.prefill} />}
        {view.name === 'history' && <HistoryPage />}
        {view.name === 'settings' && <SettingsPage section={view.section} />}
      </div>
      {view.name === 'workspace' && session && <WorkspaceStatus />}
      <Toasts />
      <GlobalDialogs />
      {paletteOpen && <SearchPalette />}
    </div>
  );
}

function AppTitleBar({ view, production }: { view: View; production: boolean }) {
  const { navigate, back, setPaletteOpen } = useUi();
  const session = useWorkspace((s) => s.session);
  const link = useWorkspace((s) => s.link);
  const { disconnect, reconnect } = useWorkspace.getState();
  const engine = useEngine(session?.connection.config.engine);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const home = () => navigate({ name: 'home' });

  const navButtons = (
    <>
      <Button size="sm" onClick={() => navigate({ name: 'history' })}>
        History
      </Button>
      <Button size="sm" onClick={() => navigate({ name: 'settings', section: 'general' })}>
        Settings
      </Button>
    </>
  );

  if (view.name === 'workspace' && session) {
    const c = session.connection;
    return (
      <>
        <TitleBar
          production={production}
          onBrand={home}
          crumbs={
            <>
              <TitleDivider />
              {engine && <EngineMark engine={engine} size={18} />}
              <button
                type="button"
                className={wsStyles.connMenu}
                aria-haspopup="menu"
                onClick={(e) => {
                  const r = e.currentTarget.getBoundingClientRect();
                  setMenu({ x: r.left, y: r.bottom + 4 });
                }}
              >
                {c.name} <Icon name="chevronDown" size={10} />
              </button>
              <EnvironmentChip environment={c.environment} />
              {session.server.read_only && (
                <Chip tone="mute" title={c.config.read_only ? 'Read-only connection profile' : 'The server is read-only (standby)'}>
                  <Icon name="lock" size={11} /> Read-only
                </Chip>
              )}
              {session.tunneled && (
                <Chip tone="mute" title={`SSH tunnel via ${c.config.ssh?.host.host ?? ''}`}>
                  SSH
                </Chip>
              )}
              {session.insecure && (
                <Chip tone="caution" title={session.insecure}>
                  <Icon name="warning" size={11} /> Not secure
                </Chip>
              )}
              <StatusIndicator status={link} live={false} />
            </>
          }
          center={
            <button type="button" className={wsStyles.search} onClick={() => setPaletteOpen(true)}>
              Search objects and scripts
              <span className="mono">{shortcutLabel('mod+k')}</span>
            </button>
          }
          actions={navButtons}
        />
        {menu && (
          <Menu
            x={menu.x}
            y={menu.y}
            onClose={() => setMenu(null)}
            label="Connection"
            items={[
              { label: 'All connections', onSelect: home },
              { label: 'Edit connection', onSelect: () => navigate({ name: 'connection-form', engine: c.config.engine, connectionId: c.id }) },
              { type: 'separator' },
              { label: 'Reconnect', onSelect: () => void reconnect() },
              {
                label: 'Disconnect',
                onSelect: async () => {
                  await disconnect();
                  home();
                },
              },
            ]}
          />
        )}
      </>
    );
  }

  const backToWorkspace = session && (view.name === 'history' || view.name === 'settings') && (
    <Button size="sm" onClick={back}>
      ← Back to {session.connection.name}
    </Button>
  );

  switch (view.name) {
    case 'home':
      return <TitleBar onBrand={home} actions={navButtons} />;
    case 'engines':
      return <TitleBar onBrand={home} crumbs={<Crumbs items={[{ label: 'Connections', onClick: home }, { label: 'New connection' }]} />} />;
    case 'connection-form':
      return <TitleBar onBrand={home} crumbs={<Crumbs items={[{ label: 'Connections', onClick: home }, { label: view.connectionId ? 'Edit connection' : 'New connection' }]} />} />;
    case 'history':
      return <TitleBar onBrand={home} crumbs={<Crumbs items={[{ label: 'History' }]} />} actions={backToWorkspace || undefined} />;
    case 'settings':
      return <TitleBar onBrand={home} crumbs={<Crumbs items={[{ label: 'Settings' }]} />} actions={backToWorkspace || undefined} />;
    default:
      return <TitleBar onBrand={home} actions={navButtons} />;
  }
}

function WorkspaceStatus() {
  const session = useWorkspace((s) => s.session)!;
  const link = useWorkspace((s) => s.link);
  const active = useWorkspace((s) => s.tabs.find((t) => t.id === s.activeTabId));
  const { line, column } = useEditorStatus();
  const run = active?.kind === 'query' ? active.run : null;
  const result = run && typeof run.active === 'number' ? run.results[run.active] : undefined;
  return (
    <StatusBar
      status={link}
      left={
        <>
          {(link === 'failed' || link === 'disconnected') && (
            <Button size="sm" onClick={() => void useWorkspace.getState().reconnect()}>
              Reconnect
            </Button>
          )}
          <span>{session.server.version}</span>
          <span className="mono">
            {session.server.user}@{session.server.database}
          </span>
          <span>{session.server.read_only ? 'Read-only server' : 'Auto-commit'}</span>
          {session.server.tls && <span>TLS{session.server.tls.verified ? ' · verified' : ''}</span>}
        </>
      }
      right={
        <>
          {run?.status === 'running' && <span>Running…</span>}
          {result && (
            <span className="tabular">
              {formatCount(result.rows.length)}
              {result.hasMore ? '+' : ''} rows · {formatDuration(result.durationMs)}
            </span>
          )}
          {active?.kind === 'query' && (
            <span>
              Ln {line}, Col {column}
            </span>
          )}
        </>
      }
    />
  );
}

/** ⌘K anywhere; ⌘T, ⌘W, Ctrl+Tab and run shortcuts in the workspace. */
function useGlobalShortcuts(view: View) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const ui = useUi.getState();
      if (ui.confirmation || ui.question) return;
      if (matchShortcut(e, 'mod+k')) {
        e.preventDefault();
        ui.setPaletteOpen(!ui.paletteOpen);
        return;
      }
      const ws = useWorkspace.getState();
      if (view.name !== 'workspace' || !ws.session || ui.paletteOpen) return;
      if (matchShortcut(e, 'mod+t')) {
        e.preventDefault();
        ws.openQueryTab();
      } else if (matchShortcut(e, 'mod+w')) {
        e.preventDefault();
        if (ws.activeTabId) void ws.closeTab(ws.activeTabId);
      } else if (e.ctrlKey && e.key === 'Tab') {
        e.preventDefault();
        ws.cycleTab(e.shiftKey ? -1 : 1);
      } else if ((matchShortcut(e, 'mod+enter') || matchShortcut(e, 'shift+mod+enter')) && !(e.target as HTMLElement).closest('.cm-editor')) {
        e.preventDefault();
        window.dispatchEvent(new CustomEvent<RunMode>(RUN_EVENT, { detail: e.shiftKey ? 'script' : 'statement' }));
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [view.name]);
}
