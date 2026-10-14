import { useMemo, useState } from 'react';

import type { AuthMethod } from '@/bindings/AuthMethod';
import type { ConnConfig } from '@/bindings/ConnConfig';
import type { ConnOptions } from '@/bindings/ConnOptions';
import type { ConnectionDraft } from '@/bindings/ConnectionDraft';
import type { DbError } from '@/bindings/DbError';
import type { Engine } from '@/bindings/Engine';
import type { Environment } from '@/bindings/Environment';
import type { ParsedUrl } from '@/bindings/ParsedUrl';
import type { PasswordChange } from '@/bindings/PasswordChange';
import type { ServerInfo } from '@/bindings/ServerInfo';
import type { SshTunnel } from '@/bindings/SshTunnel';
import type { SslMode } from '@/bindings/SslMode';
import type { TlsFiles } from '@/bindings/TlsFiles';
import { Alert, Button, EngineMark, ErrorAlert, FieldShell, PageHead, Segmented, SelectField, TextField } from '@/components/ui';
import { toAppError } from '@/lib/errors';
import { connectionDefaults, insecureReason, sslModeLabel } from '@/lib/engines';
import { formatDuration } from '@/lib/format';
import { api } from '@/lib/ipc';
import { isMac } from '@/lib/platform';
import { useConnections, useEngine } from '@/stores/connections';
import { useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import {
  AuthFields,
  authSummary,
  FormSection,
  type HopSecret,
  OptionFields,
  type OptionDrafts,
  optionDrafts,
  optionsSummary,
  SshFields,
  sshSummary,
  TlsFields,
  tlsSummary,
  validateOptions,
} from './AdvancedSections';
import styles from './Connections.module.css';
import { TrustHostKey } from './TrustHostKey';

const ENVIRONMENTS: Environment[] = ['Local', 'Development', 'Staging', 'Production'];

const SSL_MODES: SslMode[] = ['verify-full', 'verify-ca', 'require', 'prefer', 'disable'];

const keychainName = isMac ? 'macOS Keychain' : navigator.userAgent.includes('Windows') ? 'Windows Credential Manager' : 'the system keychain';

interface FormState {
  auth: AuthMethod;
  tlsFiles: TlsFiles;
  ssh: SshTunnel | null;
  sshSecrets: HopSecret[];
  readOnly: boolean;
  options: OptionDrafts;
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
  const defaults = connectionDefaults(engine);
  const hops = config?.ssh ? config.ssh.jumps.length + 1 : 0;
  return {
    auth: config?.auth ?? { method: 'password' },
    tlsFiles: config?.tls_files ?? { ca_file: null, client_cert_file: null, client_key_file: null },
    ssh: config?.ssh ?? null,
    sshSecrets: Array.from({ length: hops }, () => ({ value: '', clear: false })),
    readOnly: config?.read_only ?? false,
    options: optionDrafts(config?.options ?? DEFAULT_OPTIONS),
    name: existing?.name ?? prefill?.name_suggestion ?? '',
    folderId: existing?.folder_id ?? '',
    environment: existing?.environment ?? 'Development',
    host: config?.host ?? 'localhost',
    port: String(config?.port ?? defaultPort ?? ''),
    database: config?.database ?? defaults.database,
    username: config?.username ?? defaults.username,
    password: prefill?.password ?? '',
    clearPassword: false,
    sslMode: config?.ssl_mode ?? 'verify-full',
    timeout: String(config?.connect_timeout_secs ?? 10),
  };
}

type Errors = Partial<Record<keyof FormState, string>>;

type SectionId = 'auth' | 'tls' | 'ssh' | 'options';

const DEFAULT_OPTIONS: ConnOptions = { keepalive_secs: 60, connect_retries: 0, idle_timeout_mins: 0, pool_size: 4 };

function validate(s: FormState): Errors {
  const errors: Errors = {};
  if (Object.keys(validateOptions(s.options)).length) errors.options = 'Check the connection options.';
  if (s.ssh && [s.ssh.host, ...s.ssh.jumps].some((h) => !h.host || !h.username || h.port < 1 || h.port > 65535 || (h.auth.method === 'private_key' && !h.auth.path))) {
    errors.ssh = 'Every SSH host needs a host, port, user and (for key auth) a key file.';
  }
  if (s.auth.method === 'aws_iam' && !s.auth.region) errors.auth = 'Enter the AWS region.';
  if (s.auth.method !== 'password' && s.sslMode === 'disable') errors.auth = 'Cloud IAM authentication requires TLS.';
  if (s.tlsFiles.client_cert_file !== null && s.tlsFiles.client_key_file === null) errors.tlsFiles = 'Choose the client key too.';
  if (s.tlsFiles.client_key_file !== null && s.tlsFiles.client_cert_file === null) errors.tlsFiles = 'Choose the client certificate too.';
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
  const [openSections, setOpenSections] = useState<Record<SectionId, boolean>>(() => ({
    auth: Boolean(existing?.config.auth.method !== 'password' && existing),
    tls: Boolean(existing && (existing.config.tls_files.ca_file || existing.config.tls_files.client_cert_file)),
    ssh: Boolean(existing?.config.ssh),
    options: false,
  }));
  const toggle = (k: SectionId) => setOpenSections((o) => ({ ...o, [k]: !o[k] }));

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

  const hopCount = state.ssh ? state.ssh.jumps.length + 1 : 0;
  // Secrets are stored per hop index; after adding or removing hops they must be entered again.
  const savedSecrets = existing && existing.ssh_secrets.length === hopCount ? existing.ssh_secrets : [];
  const hopChanges = (): PasswordChange[] =>
    Array.from({ length: hopCount }, (_, i) => {
      const secret = state.sshSecrets[i] ?? { value: '', clear: false };
      if (secret.value) return { action: 'set', value: secret.value };
      if (savedSecrets[i] && !secret.clear) return { action: 'keep' };
      return { action: 'clear' };
    });

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
      tls_files: state.tlsFiles,
      ssh: state.ssh,
      read_only: state.readOnly,
      options: {
        keepalive_secs: Number(state.options.keepalive_secs),
        connect_retries: Number(state.options.connect_retries),
        idle_timeout_mins: Number(state.options.idle_timeout_mins),
        pool_size: Number(state.options.pool_size),
      },
      auth: state.auth,
    },
    password: state.auth.method === 'password' ? passwordChange() : { action: 'clear' },
    ssh_secrets: hopChanges(),
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
  const insecure = insecureReason({ host: state.host.trim(), ssl_mode: state.sslMode, ssh: state.ssh });
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
          {state.auth.method === 'password' && (
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
          )}
          <SelectField
            label="SSL mode"
            mono
            value={state.sslMode}
            onChange={(e) => set('sslMode', e.target.value as SslMode)}
            options={SSL_MODES.map((value) => ({ value, label: sslModeLabel(engine, value) }))}
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
          <FormSection title="Authentication" summary={authSummary(state.auth)} open={openSections.auth} onToggle={() => toggle('auth')}>
            <AuthFields auth={state.auth} onChange={(a) => set('auth', a)} />
            {err('auth') && <span className={`dq-field__error ${styles.full}`}>{err('auth')}</span>}
          </FormSection>
          <FormSection title="Advanced TLS" summary={tlsSummary(state.tlsFiles)} open={openSections.tls} onToggle={() => toggle('tls')}>
            <TlsFields files={state.tlsFiles} onChange={(f) => set('tlsFiles', f)} />
            {err('tlsFiles') && <span className={`dq-field__error ${styles.full}`}>{err('tlsFiles')}</span>}
          </FormSection>
          <FormSection title="SSH tunnel" summary={sshSummary(state.ssh)} open={openSections.ssh} onToggle={() => toggle('ssh')}>
            <SshFields tunnel={state.ssh} secrets={state.sshSecrets} savedSecrets={savedSecrets} onChange={(t) => set('ssh', t)} onSecrets={(v) => set('sshSecrets', v)} />
            {existing && existing.ssh_secrets.length > 0 && existing.ssh_secrets.length !== hopCount && (
              <span className={`dq-field__hint ${styles.full}`}>The SSH hosts changed, so saved SSH secrets must be entered again.</span>
            )}
            {err('ssh') && <span className={`dq-field__error ${styles.full}`}>{err('ssh')}</span>}
          </FormSection>
          <FormSection title="Connection options" summary={optionsSummary(state.readOnly, state.options)} open={openSections.options} onToggle={() => toggle('options')}>
            <OptionFields
              readOnly={state.readOnly}
              options={state.options}
              errors={touched ? validateOptions(state.options) : {}}
              onReadOnly={(v) => set('readOnly', v)}
              onOption={(k, v) => set('options', { ...state.options, [k]: v })}
            />
          </FormSection>
        </div>

        {insecure && (
          <Alert tone="caution" title="This connection is not fully secure">
            {insecure} Choose verify-full{state.ssh ? '' : ' or an SSH tunnel'} for networks you do not control.
          </Alert>
        )}

        {result?.ok && (
          <Alert tone="positive" title={`✓ Connected to ${result.info.version} in ${formatDuration(result.info.latency_ms)}.`}>
            {result.info.tls
              ? `TLS${result.info.tls.version ? ` ${result.info.tls.version.replace('TLSv', '')}` : ''} · ${result.info.tls.verified ? `certificate verified for ${state.host}` : 'certificate not verified'}`
              : 'Not encrypted (SSL disabled or not offered by the server).'}
            {result.info.read_only && ' · Server is read-only (standby).'}
          </Alert>
        )}
        {result && !result.ok && <ErrorAlert error={result.error} title={`Could not connect to ${state.host}:${state.port}`} />}
        {result && !result.ok && <TrustHostKey error={result.error} onTrusted={() => void test()} />}

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
