import { useEffect, useState } from 'react';

import type { Settings } from '@/bindings/Settings';
import { Alert, Button, Card, Chip, FileField, PageHead, TextField } from '@/components/ui';
import { formatRelative, plural } from '@/lib/format';
import { api } from '@/lib/ipc';
import { useFolderSync } from '@/stores/folderSync';
import { useSettings } from '@/stores/settings';
import { useUi } from '@/stores/ui';

import { Row } from './Row';
import styles from './Settings.module.css';

/** Runs a command that returns the saved settings, keeping the settings store in step. */
async function applySettings(run: () => Promise<Settings>): Promise<boolean> {
  try {
    useSettings.setState({ settings: await run() });
    return true;
  } catch (e) {
    useUi.getState().toastError(e);
    return false;
  }
}

export function SyncSettings() {
  useEffect(() => {
    void useFolderSync.getState().load().catch(useUi.getState().toastError);
  }, []);
  return (
    <>
      <PageHead eyebrow="Settings" title="Sync" />
      <ConnectionSync />
      <ScriptsFolder />
    </>
  );
}

/** Connections and folders through one encrypted file in a folder the user already syncs. */
function ConnectionSync() {
  const folder = useSettings((s) => s.settings.sync_folder);
  const status = useFolderSync((s) => s.sync);
  const [chosen, setChosen] = useState<string | null>(folder || null);
  const [passphrase, setPassphrase] = useState('');
  const [busy, setBusy] = useState(false);
  useEffect(() => setChosen(folder || null), [folder]);

  const on = Boolean(folder);
  const changed = Boolean(chosen) && chosen !== folder;
  const run = async (action: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await action();
    } finally {
      setBusy(false);
    }
  };
  const configure = () =>
    run(async () => {
      if (chosen && (await applySettings(() => api.syncConfigure(chosen, passphrase || null)))) setPassphrase('');
    });
  const disable = async () => {
    const ok = await useUi.getState().ask({
      title: 'Turn off connection sync?',
      body: 'This device stops syncing and forgets the sync passphrase. The sync file and your connections stay as they are.',
      confirmLabel: 'Turn off',
    });
    if (ok) await run(() => applySettings(api.syncDisable));
  };

  return (
    <Card
      title="Connection sync"
      flush
      footer="Passwords, SSH secrets, tokens and AI keys are never written to the sync file: enter them once on each device. There is no DataQuery server; the file is encrypted with your passphrase (Argon2id and AES-256-GCM)."
    >
      <Row
        title="How it works"
        help="Pick a folder your devices already sync — iCloud Drive, Dropbox, OneDrive or a git repository — and the same passphrase on each device. DataQuery keeps connections, folders and their settings in one encrypted file there, DataQuery.connections.dqsync, and syncs at launch, after you change a connection, when the window comes to the front and when the file changes."
      />
      <div className={styles.fields}>
        <FileField className={styles.wide} label="Sync folder" directory value={chosen} onChange={setChosen} />
        <TextField
          className={styles.wide}
          label="Sync passphrase"
          type="password"
          autoComplete="off"
          placeholder={on && status?.has_passphrase ? '•••••••• (saved in the keychain)' : 'At least 8 characters'}
          hint="The same on every device. Saved in this device's keychain. A forgotten passphrase cannot be recovered: delete the sync file and set a new one."
          value={passphrase}
          onChange={(e) => setPassphrase(e.target.value)}
          onKeyDown={(e) => e.key === 'Enter' && chosen && void configure()}
          action={
            <Button variant="primary" busy={busy} disabled={!chosen || (!on && passphrase.length < 8) || (on && !changed && !passphrase)} onClick={() => void configure()}>
              {on ? 'Save' : 'Turn on sync'}
            </Button>
          }
        />
      </div>
      {on && (
        <Row title="Status" help={<SyncStatusLine />}>
          <span style={{ display: 'flex', gap: 8 }}>
            <Button size="sm" busy={status?.running} onClick={() => void useFolderSync.getState().syncNow().catch(useUi.getState().toastError)}>
              Sync now
            </Button>
            <Button size="sm" variant="plain" disabled={busy} onClick={() => void disable()}>
              Turn off
            </Button>
          </span>
        </Row>
      )}
      {on && status?.error && (
        <div className={styles.check}>
          <Alert tone="critical">{status.error}</Alert>
        </div>
      )}
      {on && Boolean(status?.warnings.length) && (
        <div className={styles.check}>
          <Alert tone="caution" title="Not synced on this device">
            {status?.warnings.map((w) => <div key={w}>{w}</div>)}
          </Alert>
        </div>
      )}
    </Card>
  );
}

function SyncStatusLine() {
  const status = useFolderSync((s) => s.sync);
  if (!status) return <>Checking…</>;
  if (status.running) return <>Syncing…</>;
  return (
    <>
      Last synced: {formatRelative(status.last_synced_at)}
      {status.last_synced_at ? ` · ${plural(status.connections, 'connection')}` : ''}{' '}
      {status.error && (
        <Chip tone="critical" dot>
          Error
        </Chip>
      )}
    </>
  );
}

/** Saved scripts mirrored as plain `.sql` files, e.g. in a team git repository. */
function ScriptsFolder() {
  const folder = useSettings((s) => s.settings.scripts_folder);
  const status = useFolderSync((s) => s.scripts);
  const report = status?.report;
  const [busy, setBusy] = useState(false);

  const choose = async (path: string | null) => {
    if (!path) {
      const ok = await useUi.getState().ask({
        title: 'Stop using the scripts folder?',
        body: 'Saved scripts stay in DataQuery and the .sql files stay in the folder; they are no longer kept in step.',
        confirmLabel: 'Stop',
      });
      if (!ok) return;
    }
    setBusy(true);
    await applySettings(() => api.setScriptsFolder(path));
    setBusy(false);
  };

  return (
    <Card
      title="Scripts folder"
      flush
      footer='Tags, pin and linked connection (by name) go in an optional first line: -- dataquery: {"tags":["billing"],"pinned":true,"connection":"prod"}. Dotfiles, non-.sql files, symbolic links and files over 1 MB are ignored.'
    >
      <Row
        title="Saved scripts as .sql files"
        help="Each saved script is a plain .sql file; script folders are sub-folders. Files added, edited, renamed or deleted in the folder (for example after a git pull) show up in DataQuery, and edits in DataQuery are written back. If a file changes while its tab has unsaved edits, DataQuery asks before replacing anything."
      />
      <div className={styles.fields}>
        <FileField className={styles.wide} label="Folder" directory value={folder || null} onChange={(p) => void choose(p)} hint={busy ? 'Matching scripts and files…' : undefined} />
      </div>
      {folder && (
        <Row title="Status" help={report ? `${plural(report.files, 'script file')} · checked ${formatRelative(report.at)}` : 'Checking…'}>
          <Button size="sm" busy={busy} onClick={() => void useFolderSync.getState().syncScripts().catch(useUi.getState().toastError)}>
            Sync now
          </Button>
        </Row>
      )}
      {folder && status?.error && (
        <div className={styles.check}>
          <Alert tone="critical">{status.error}</Alert>
        </div>
      )}
      {folder && report && report.conflicts.length > 0 && (
        <div className={styles.check}>
          <Alert tone="caution" title="Changed in both places">
            The file and DataQuery both changed; the file won and DataQuery's version was kept as: {report.conflicts.join(', ')}.
          </Alert>
        </div>
      )}
      {folder && report && report.skipped.length > 0 && (
        <div className={styles.check}>
          <Alert tone="info" title="Not read">
            {report.skipped.map((s) => (
              <div key={s}>{s}</div>
            ))}
          </Alert>
        </div>
      )}
    </Card>
  );
}
