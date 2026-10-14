import { useMemo, useState } from 'react';

import type { ConnConfig } from '@/bindings/ConnConfig';
import type { ConnectionDraft } from '@/bindings/ConnectionDraft';
import type { DbError } from '@/bindings/DbError';
import type { Engine } from '@/bindings/Engine';
import type { Environment } from '@/bindings/Environment';
import type { ParsedUrl } from '@/bindings/ParsedUrl';
import type { PasswordChange } from '@/bindings/PasswordChange';
import type { ServerInfo } from '@/bindings/ServerInfo';
import type { SslMode } from '@/bindings/SslMode';
import { Alert, Button, EngineMark, ErrorAlert, FieldShell, PageHead, PlannedChip, Segmented, SelectField, TextField } from '@/components/ui';
import { toAppError } from '@/lib/errors';
import { formatDuration } from '@/lib/format';
import { api } from '@/lib/ipc';
import { isMac } from '@/lib/platform';
import { useConnections, useEngine } from '@/stores/connections';
import { useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import styles from './Connections.module.css';

const ENVIRONMENTS: Environment[] = ['Local', 'Development', 'Staging', 'Production'];

const SSL_MODES: { value: SslMode; label: string }[] = [
  { value: 'verify-full', label: 'verify-full' },
  { value: 'verify-ca', label: 'verify-ca' },
  { value: 'require', label: 'require' },
  { value: 'prefer', label: 'prefer' },
  { value: 'disable', label: 'disable' },
];

const keychainName = isMac ? 'macOS Keychain' : navigator.userAgent.includes('Windows') ? 'Windows Credential Manager' : 'the system keychain';

interface FormState {
  name: string;
  folderId: string;
  environment: Environment;
  host: string;
  port: string;
  database: string;
  username: string;
  password: string;
  clearPassword: boolean;
  sslMode: SslMode;
  timeout: string;
}

function initialState(engine: Engine, defaultPort: number | null, existing?: { name: string; folder_id: string | null; environment: Environment; config: ConnConfig }, prefill?: ParsedUrl): FormState {
  const config = existing?.config ?? prefill?.config;
  return {
    name: existing?.name ?? prefill?.name_suggestion ?? '',
    folderId: existing?.folder_id ?? '',
    environment: existing?.environment ?? 'Development',
    host: config?.host ?? 'localhost',
    port: String(config?.port ?? defaultPort ?? ''),
    database: config?.database ?? (engine === 'postgres' ? 'postgres' : ''),
    username: config?.username ?? (engine === 'postgres' ? 'postgres' : 'root'),
    password: prefill?.password ?? '',
    clearPassword: false,
    sslMode: config?.ssl_mode ?? 'verify-full',
    timeout: String(config?.connect_timeout_secs ?? 10),
  };
}

type Errors = Partial<Record<keyof FormState, string>>;

function validate(s: FormState): Errors {
  const errors: Errors = {};
  if (!s.name.trim()) errors.name = 'Give the connection a name.';
  if (!s.host.trim()) errors.host = 'Host is required.';
  const port = Number(s.port);
  if (!Number.isInteger(port) || port < 1 || port > 65535) errors.port = '1–65535';
  if (!s.username.trim()) errors.username = 'Username is required.';
  const timeout = Number(s.timeout);
  if (!Number.isInteger(timeout) || timeout < 1 || timeout > 300) errors.timeout = '1–300 seconds';
  return errors;
}

/** Step 2: connection details, test, save. */
export function ConnectionForm({ engine, connectionId, prefill }: { engine: Engine; connectionId?: string; prefill?: ParsedUrl }) {
  const info = useEngine(engine);
  const { connections, folders, save } = useConnections();
  const navigate = useUi((s) => s.navigate);
  const connect = useWorkspace((s) => s.connect);
  const existing = connections.find((c) => c.id === connectionId);
  const [state, setState] = useState(() => initialState(engine, info?.default_port ?? null, existing, prefill));
  const [touched, setTouched] = useState(false);
  const [testing, setTesting] = useState(false);
  const [saving, setSaving] = useState<'save' | 'connect' | null>(null);
  const [result, setResult] = useState<{ ok: true; info: ServerInfo } | { ok: false; error: DbError } | null>(null);

  const errors = useMemo(() => validate(state), [state]);
  const set = <K extends keyof FormState>(key: K, value: FormState[K]) => {
    setState((s) => ({ ...s, [key]: value }));
    setResult(null);
  };

  const passwordChange = (): PasswordChange => {
    if (state.password) return { action: 'set', value: state.password };
    if (existing?.has_password && !state.clearPassword) return { action: 'keep' };
    return { action: 'clear' };
  };

  const draft = (): ConnectionDraft => ({
    id: existing?.id ?? null,
    name: state.name.trim(),
    folder_id: state.folderId || null,
    environment: state.environment,
    favorite: existing?.favorite ?? false,
    config: {
      engine,
      host: state.host.trim(),
      port: Number(state.port),
      database: state.database.trim(),
      username: state.username.trim(),
      ssl_mode: state.sslMode,
      connect_timeout_secs: Number(state.timeout),
    },
    password: passwordChange(),
  });

  const invalid = Object.keys(errors).length > 0;

  const test = async () => {
    setTouched(true);
    if (invalid) return;
    setTesting(true);
    setResult(null);
    try {
      setResult({ ok: true, info: await api.testConnection(draft()) });
    } catch (e) {
      setResult({ ok: false, error: toAppError(e).info });
    } finally {
      setTesting(false);
    }
  };

  const submit = async (andConnect: boolean) => {
    setTouched(true);
    if (invalid) return;
    setSaving(andConnect ? 'connect' : 'save');
    try {
      const saved = await save(draft());
      if (andConnect) {
        if (!(await connect(saved.id))) {
          const error = useWorkspace.getState().connectError;
          if (error) setResult({ ok: false, error });
          navigate({ name: 'connection-form', engine, connectionId: saved.id });
        }
      } else {
        navigate({ name: 'home' });
      }
    } catch (e) {
      setResult({ ok: false, error: toAppError(e).info });
    } finally {
      setSaving(null);
    }
  };

  const err = (key: keyof FormState) => (touched ? errors[key] : undefined);
  const passwordHint = existing?.has_password
    ? state.clearPassword
      ? 'The saved password will be removed when you save.'
      : `A password is saved in ${keychainName}. Leave blank to keep it.`
    : `Stored in ${keychainName}. Never written to the settings file or logs.`;

  return (
    <div className={styles.formPage}>
      <form
        className={styles.form}
        onSubmit={(e) => {
          e.preventDefault();
          void submit(true);
        }}
        noValidate
      >
        <PageHead eyebrow={existing ? 'Edit connection' : 'New connection · step 2 of 2'} title={state.name || 'Untitled connection'} />
        <div className={styles.formCard}>
          <div className={styles.engineRow}>
            {info && <EngineMark engine={info} />}
            <span style={{ display: 'flex', flexDirection: 'column' }}>
              <span className={styles.name}>{info?.name}</span>
              <span className="muted" style={{ fontSize: 12 }}>
                {info?.versions}
              </span>
            </span>
            <span style={{ flex: 1 }} />
            {!existing && (
              <Button size="sm" onClick={() => navigate({ name: 'engines' })}>
                Change engine
              </Button>
            )}
          </div>
          <TextField label="Name" value={state.name} error={err('name')} onChange={(e) => set('name', e.target.value)} autoFocus={!existing} />
          <SelectField
            label="Folder"
            value={state.folderId}
            onChange={(e) => set('folderId', e.target.value)}
            options={[{ value: '', label: 'No folder' }, ...folders.map((f) => ({ value: f.id, label: f.name }))]}
          />
          <div className={styles.full}>
            <FieldShell label="Environment" hint={state.environment === 'Production' ? 'Production shows a red rule, asks before every write and requires typing the name for destructive statements.' : undefined}>
              {() => (
                <Segmented<Environment>
                  label="Environment"
                  size="md"
                  value={state.environment}
                  onChange={(v) => set('environment', v)}
                  options={ENVIRONMENTS.map((env) => ({ value: env, label: env, selectedClassName: styles[`env${env}`] }))}
                />
              )}
            </FieldShell>
          </div>
          <div className={styles.hostRow}>
            <TextField label="Host" mono value={state.host} error={err('host')} onChange={(e) => set('host', e.target.value)} />
            <TextField label="Port" mono inputMode="numeric" value={state.port} error={err('port')} onChange={(e) => set('port', e.target.value.replace(/\D/g, ''))} />
          </div>
          <TextField label="Database" mono value={state.database} onChange={(e) => set('database', e.target.value)} />
          <TextField label="Username" mono value={state.username} error={err('username')} onChange={(e) => set('username', e.target.value)} />
          <div className={styles.full} style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
            <TextField
              label="Password"
              type="password"
              mono
              autoComplete="off"
              placeholder={existing?.has_password && !state.clearPassword ? '•••••••• (saved)' : ''}
              hint={passwordHint}
              value={state.password}
              onChange={(e) => set('password', e.target.value)}
            />
            {existing?.has_password && !state.password && (
              <button type="button" className={styles.linkButton} onClick={() => set('clearPassword', !state.clearPassword)}>
                {state.clearPassword ? 'Keep the saved password' : 'Remove the saved password'}
              </button>
            )}
          </div>
          <SelectField
            label="SSL mode"
            mono
            value={state.sslMode}
            onChange={(e) => set('sslMode', e.target.value as SslMode)}
            options={SSL_MODES}
            hint={
              state.sslMode === 'verify-full'
                ? 'Certificates and host name are verified.'
                : state.sslMode === 'verify-ca'
                  ? 'The certificate chain is verified; the host name is not.'
                  : state.sslMode === 'disable'
                    ? 'Traffic is not encrypted.'
                    : 'Encrypted, but the certificate is not verified.'
            }
          />
          <TextField label="Connect timeout" mono inputMode="numeric" suffix="seconds" value={state.timeout} error={err('timeout')} onChange={(e) => set('timeout', e.target.value.replace(/\D/g, ''))} />
          <div className={styles.planned}>
            <span className={styles.plannedLabel}>SSH tunnel</span>
            <PlannedChip priority="P1" />
          </div>
        </div>

        {result?.ok && (
          <Alert tone="positive" title={`✓ Connected to ${result.info.version} in ${formatDuration(result.info.latency_ms)}.`}>
            {result.info.tls
              ? `TLS${result.info.tls.version ? ` ${result.info.tls.version.replace('TLSv', '')}` : ''} · ${result.info.tls.verified ? `certificate verified for ${state.host}` : 'certificate not verified'}`
              : 'Not encrypted (SSL disabled or not offered by the server).'}
            {result.info.read_only && ' · Server is read-only (standby).'}
          </Alert>
        )}
        {result && !result.ok && <ErrorAlert error={result.error} title={`Could not connect to ${state.host}:${state.port}`} />}

        <div className={styles.formActions}>
          <Button busy={testing} onClick={() => void test()}>
            Test connection
          </Button>
          <div style={{ display: 'flex', gap: 8 }}>
            <Button onClick={() => navigate({ name: 'home' })}>Cancel</Button>
            <Button busy={saving === 'save'} onClick={() => void submit(false)}>
              Save
            </Button>
            <Button type="submit" variant="primary" busy={saving === 'connect'}>
              Save and connect
            </Button>
          </div>
        </div>
      </form>
    </div>
  );
}
