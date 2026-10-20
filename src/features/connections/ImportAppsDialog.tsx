import { open } from '@tauri-apps/plugin-dialog';
import { useEffect, useMemo, useState } from 'react';

import type { DbError } from '@/bindings/DbError';
import type { DetectedSource } from '@/bindings/DetectedSource';
import type { ForeignPreview } from '@/bindings/ForeignPreview';
import type { SourceApp } from '@/bindings/SourceApp';
import {
  Alert,
  Button,
  Checkbox,
  Chip,
  Dialog,
  EngineMark,
  EnvironmentChip,
  ErrorAlert,
  Eyebrow,
  Input,
  SelectField,
  Spinner,
} from '@/components/ui';
import { toAppError } from '@/lib/errors';
import { plural } from '@/lib/format';
import { api } from '@/lib/ipc';
import { useConnections } from '@/stores/connections';

import {
  SOURCE_APPS,
  appName,
  importable,
  initialSelection,
  needsUsername,
  selectedConnections,
  type Usernames,
} from './importApps';
import { reportConnectionImport } from './transfer';
import styles from './Connections.module.css';

interface Loaded {
  path: string;
  preview: ForeignPreview;
}

/**
 * Imports connections from TablePlus, DBeaver, Sequel Ace, Sequel Pro and
 * DataGrip: pick a detected file or choose one, review, then create the
 * selected connections. Passwords and other secrets are never imported.
 */
export function ImportAppsDialog({ onClose }: { onClose: () => void }) {
  const engines = useConnections((s) => s.engines);
  const [detected, setDetected] = useState<DetectedSource[] | null>(null);
  const [app, setApp] = useState<SourceApp>('tableplus');
  const [loaded, setLoaded] = useState<Loaded | null>(null);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [usernames, setUsernames] = useState<Usernames>({});
  const [error, setError] = useState<DbError | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api
      .detectConnectionSources()
      .then(setDetected)
      .catch(() => setDetected([]));
  }, []);

  const read = async (source: SourceApp, path: string) => {
    setBusy(true);
    setError(null);
    try {
      const preview = await api.readConnectionSource(source, path);
      setLoaded({ path, preview });
      setSelected(initialSelection(preview.connections));
      setUsernames({});
    } catch (e) {
      setError(toAppError(e).info);
    } finally {
      setBusy(false);
    }
  };

  const choose = async (folder: boolean) => {
    const info = SOURCE_APPS.find((a) => a.app === app)!;
    const path = await open({
      multiple: false,
      directory: folder,
      filters: folder ? undefined : [{ name: `${info.name} connections`, extensions: info.extensions }],
    });
    if (typeof path === 'string') await read(app, path);
  };

  const rows = useMemo(() => loaded?.preview.connections ?? [], [loaded]);
  const chosen = selectedConnections(rows, selected, usernames);
  const selectable = rows.flatMap((row, i) => (importable(row, usernames[i]) ? [i] : []));

  const toggle = (i: number, on: boolean) => {
    const next = new Set(selected);
    if (on) next.add(i);
    else next.delete(i);
    setSelected(next);
  };

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      await reportConnectionImport(await api.importConnectionList(chosen));
      onClose();
    } catch (e) {
      setError(toAppError(e).info);
      setBusy(false);
    }
  };

  const secretsNote = (
    <Alert tone="info">
      Passwords, SSH passwords and key passphrases are never imported. Enter them after the import, before you connect.
    </Alert>
  );

  if (loaded) {
    const { preview } = loaded;
    return (
      <Dialog
        wide
        title={`Import from ${appName(preview.app)}`}
        onClose={onClose}
        footer={
          <>
            <Button onClick={() => setLoaded(null)} style={{ marginRight: 'auto' }}>
              Back
            </Button>
            <Button onClick={onClose}>Cancel</Button>
            <Button variant="primary" busy={busy} disabled={chosen.length === 0} onClick={() => void submit()}>
              Import {plural(chosen.length, 'connection')}
            </Button>
          </>
        }
      >
        <div className="mono ellipsis ink-2" title={loaded.path} style={{ fontSize: 12 }}>
          {loaded.path}
        </div>
        {secretsNote}
        {rows.length === 0 ? (
          <Alert tone="caution">No connection in this file can be imported.</Alert>
        ) : (
          <>
            <Checkbox
              checked={selectable.length > 0 && selectable.every((i) => selected.has(i))}
              onChange={(on) => setSelected(new Set(on ? selectable : []))}
              label={`Select all that can be imported (${selectable.length} of ${rows.length})`}
            />
            <div className={styles.importList}>
              {rows.map((row, i) => {
                const c = row.connection;
                const engine = engines.find((e) => e.engine === c.config.engine);
                const address = c.config.host
                  ? `${c.config.host}:${c.config.port}${c.config.database ? `/${c.config.database}` : ''}`
                  : c.config.database;
                return (
                  <div key={i} className={styles.importRow}>
                    <Checkbox
                      checked={selected.has(i)}
                      disabled={!importable(row, usernames[i])}
                      onChange={(on) => toggle(i, on)}
                      label={c.name}
                      help={<span className="mono">{address}</span>}
                    />
                    {engine ? <EngineMark engine={engine} size={22} /> : <span />}
                    <span className="ellipsis ink-2" title={c.folder ?? undefined}>
                      {c.folder ?? '—'}
                    </span>
                    <span className={styles.importStatus}>
                      <EnvironmentChip environment={c.environment} small />
                      {row.duplicate_of !== null && (
                        <Chip
                          tone="caution"
                          small
                          title={`Same engine, host, port, user and database as “${row.duplicate_of}”`}
                        >
                          Already saved
                        </Chip>
                      )}
                      {needsUsername(row) ? (
                        <Input
                          label={`Username for ${c.name}`}
                          placeholder="Username"
                          value={usernames[i] ?? ''}
                          onChange={(e) => {
                            setUsernames({ ...usernames, [i]: e.target.value });
                            if (!selected.has(i) && e.target.value.trim()) toggle(i, true);
                          }}
                        />
                      ) : (
                        row.problem && <span className={styles.importProblem}>{row.problem}</span>
                      )}
                    </span>
                  </div>
                );
              })}
            </div>
          </>
        )}
        {preview.skipped.length > 0 && (
          <details>
            <summary className="ink-2">Not imported ({preview.skipped.length})</summary>
            <ul className={styles.importSkipped}>
              {preview.skipped.map((s, i) => (
                <li key={i}>{s}</li>
              ))}
            </ul>
          </details>
        )}
        {error && <ErrorAlert error={error} />}
      </Dialog>
    );
  }

  const info = SOURCE_APPS.find((a) => a.app === app)!;
  return (
    <Dialog wide title="Import from another app" onClose={onClose} footer={<Button onClick={onClose}>Cancel</Button>}>
      {secretsNote}
      <Eyebrow>Found on this computer</Eyebrow>
      {detected === null ? (
        <div className="ink-2">
          <Spinner /> Looking for TablePlus, DBeaver, Sequel Ace, Sequel Pro and DataGrip…
        </div>
      ) : detected.length === 0 ? (
        <div className="ink-2">No connection files were found in the usual places. Choose a file below.</div>
      ) : (
        <div className={styles.importList}>
          {detected.map((d) => (
            <button
              key={d.path}
              type="button"
              className={styles.sourceRow}
              disabled={busy}
              onClick={() => void read(d.app, d.path)}
            >
              <span className={styles.name}>{appName(d.app)}</span>
              <span className="ink-2">{d.label}</span>
              <span className={`${styles.address} ellipsis`} title={d.path}>
                {d.path}
              </span>
            </button>
          ))}
        </div>
      )}
      <Eyebrow>Choose a file</Eyebrow>
      <div className={styles.sourcePick}>
        <SelectField
          label="App"
          value={app}
          onChange={(e) => setApp(e.target.value as SourceApp)}
          options={SOURCE_APPS.map((a) => ({ value: a.app, label: a.name }))}
          hint={info.file}
        />
        <Button busy={busy} onClick={() => void choose(false)}>
          Choose file…
        </Button>
        {app === 'datagrip' && (
          <Button disabled={busy} onClick={() => void choose(true)}>
            Choose project folder…
          </Button>
        )}
      </div>
      {error && <ErrorAlert error={error} />}
    </Dialog>
  );
}
