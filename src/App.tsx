import { listen } from '@tauri-apps/api/event';
import { useEffect, useState } from 'react';
import { useShallow } from 'zustand/react/shallow';

import { GlobalDialogs } from '@/components/shell/GlobalDialogs';
import styles from '@/components/shell/Shell.module.css';
import { StatusBar, StatusIndicator } from '@/components/shell/StatusBar';
import { Toasts } from '@/components/shell/Toasts';
import { Crumbs, TitleBar, TitleBarButton, TitleDivider } from '@/components/shell/TitleBar';
import { Button, Chip, EngineMark, EnvironmentChip, Icon, IconButton, Menu } from '@/components/ui';
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
import { CommandPalette } from '@/features/commands/CommandPalette';
import { appCommands, commandKeys, openNewWindow } from '@/features/commands/commands';
import { ProGate } from '@/features/license/ProGate';
import { SchemaDiffPage } from '@/features/schema/SchemaDiffPage';
import { setLanguage, t } from '@/i18n';
import { matchShortcut, shortcutLabel } from '@/lib/platform';
import { useConnections, useEngine } from '@/stores/connections';
import { useCompletion } from '@/stores/completion';
import { startFolderSync } from '@/stores/folderSync';
import { useLayout } from '@/stores/layout';
import { useLicense } from '@/stores/license';
import { applyTheme, useSettings } from '@/stores/settings';
import { useUi, type View } from '@/stores/ui';
import { startUpdateChecks } from '@/stores/updates';
import { noteSessionClosed, useWorkspace } from '@/stores/workspace';

export function App() {
  const view = useUi((s) => s.view);
  const paletteOpen = useUi((s) => s.paletteOpen);
  const commandsOpen = useUi((s) => s.commandsOpen);
  const session = useWorkspace((s) => s.session);
  const theme = useSettings((s) => s.settings.theme);

  useEffect(() => {
    let stopUpdateChecks: (() => void) | undefined;
    void useSettings
      .getState()
      .load()
      .then(() => {
        stopUpdateChecks = startUpdateChecks();
      });
    void useConnections.getState().load();
    void useLicense.getState().load();
    void useLayout.getState().load();
    void useCompletion.getState().loadSnippets();
    const unlisten = listen<{ session_id: string; reason: string }>('session-closed', (e) => noteSessionClosed(e.payload.session_id, e.payload.reason));
    const stopFolderSync = startFolderSync();
    return () => {
      stopUpdateChecks?.();
      stopFolderSync();
      void unlisten.then((fn) => fn());
    };
  }, []);

  useEffect(() => {
    applyTheme(theme);
    if (theme !== 'system') return;
    const media = window.matchMedia('(prefers-color-scheme: dark)');
    const onChange = () => applyTheme('system');
    media.addEventListener('change', onChange);
    return () => media.removeEventListener('change', onChange);
  }, [theme]);

  useGlobalShortcuts();
  const language = useSettings((s) => s.settings.language);
  setLanguage(language);

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
        {view.name === 'diff' && (
          <ProGate feature="Schema diff" summary="Compare the schemas of two connections and get the migration SQL that makes the target match, with a preview before anything runs.">
            <SchemaDiffPage />
          </ProGate>
        )}
        {view.name === 'settings' && <SettingsPage section={view.section} />}
      </div>
      {view.name === 'workspace' && session && <WorkspaceStatus />}
      <Toasts />
      <GlobalDialogs />
      {paletteOpen && <SearchPalette />}
      {commandsOpen && <CommandPalette />}
    </div>
  );
}

function AppTitleBar({ view, production }: { view: View; production: boolean }) {
  const { navigate, back, setPaletteOpen } = useUi(useShallow((s) => ({ navigate: s.navigate, back: s.back, setPaletteOpen: s.setPaletteOpen })));
  const session = useWorkspace((s) => s.session);
  const link = useWorkspace((s) => s.link);
  const { disconnect, reconnect } = useWorkspace.getState();
  const engine = useEngine(session?.connection.config.engine);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const home = () => navigate({ name: 'home' });

  const navButtons = (
    <>
      <IconButton label={`${t('nav.commandPalette')} (${shortcutLabel(commandKeys('palette.open') ?? 'shift+mod+p')})`} onClick={() => useUi.getState().setCommandsOpen(true)}>
        <Icon name="command" />
      </IconButton>
      {session?.capabilities.monitoring && view.name === 'workspace' && (
        <TitleBarButton icon="activity" label={t('nav.monitoring')} onClick={() => useWorkspace.getState().openMonitorTab()} />
      )}
      <TitleBarButton icon="history" label={t('nav.history')} onClick={() => navigate({ name: 'history' })} />
      <TitleBarButton icon="settings" label={t('nav.settings')} onClick={() => navigate({ name: 'settings', section: 'general' })} />
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
              { label: t('action.newWindow'), hint: shortcutLabel(commandKeys('window.new') ?? ''), onSelect: openNewWindow },
              { label: t('nav.schemaDiff'), onSelect: () => navigate({ name: 'diff' }) },
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

  const backToWorkspace = session && (view.name === 'history' || view.name === 'settings' || view.name === 'diff') && (
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
    case 'diff':
      return <TitleBar onBrand={home} crumbs={<Crumbs items={[{ label: t('nav.schemaDiff') }]} />} actions={backToWorkspace || navButtons} />;
    default:
      return <TitleBar onBrand={home} actions={navButtons} />;
  }
}

/** Auto-commit / manual commit / open or failed transaction, with Commit and Rollback. */
function TransactionStatus() {
  const tx = useWorkspace((s) => s.tx);
  const manual = useSettings((s) => s.settings.manual_commit);
  const caps = useWorkspace((s) => s.session?.capabilities);
  const { transaction } = useWorkspace.getState();
  if (!caps?.transactions) return <span>{t('tx.auto')}</span>;
  const label = tx === 'open' ? t('tx.open') : tx === 'failed' ? t('tx.failed') : manual ? t('tx.manual') : t('tx.auto');
  return (
    <>
      <button
        type="button"
        className="dq-status-link"
        style={{ color: tx === 'failed' ? 'var(--critical)' : tx === 'open' ? 'var(--caution)' : undefined }}
        title={manual ? 'Statements open a transaction that stays open until Commit or Rollback. Click to switch to auto-commit.' : 'Each statement commits on its own. Click to switch to manual commit.'}
        onClick={() => void useSettings.getState().update({ manual_commit: !manual })}
      >
        {tx !== 'auto' && '● '}
        {label}
      </button>
      {tx === 'open' && (
        <Button size="xs" onClick={() => void transaction('commit')}>
          {t('action.commit')}
        </Button>
      )}
      {tx !== 'auto' && (
        <Button size="xs" onClick={() => void transaction('rollback')}>
          {t('action.rollback')}
        </Button>
      )}
    </>
  );
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
          {session.server.read_only && <span>{session.connection.config.read_only ? 'Read-only connection' : 'Read-only server'}</span>}
          <TransactionStatus />
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

/** Shortcuts for every command in the registry (customizable in Settings → Keyboard). */
function useGlobalShortcuts() {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const ui = useUi.getState();
      if (ui.confirmation || ui.question || e.defaultPrevented) return;
      if (e.ctrlKey && e.key === 'Tab' && ui.view.name === 'workspace') {
        e.preventDefault();
        useWorkspace.getState().cycleTab(e.shiftKey ? -1 : 1);
        return;
      }
      const inEditor = Boolean((e.target as HTMLElement | null)?.closest?.('.cm-editor'));
      for (const command of appCommands()) {
        const keys = commandKeys(command.id);
        if (!keys || !matchShortcut(e, keys)) continue;
        // The SQL editor binds the query shortcuts itself.
        if (inEditor && command.id.startsWith('query.')) return;
        if ((ui.paletteOpen || ui.commandsOpen) && !['palette.open', 'search.open'].includes(command.id)) return;
        if (!command.available) continue;
        e.preventDefault();
        if (command.id === 'search.open' && ui.paletteOpen) ui.setPaletteOpen(false);
        else if (command.id === 'palette.open' && ui.commandsOpen) ui.setCommandsOpen(false);
        else command.run();
        return;
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);
}
