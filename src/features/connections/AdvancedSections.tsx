import type { AuthMethod } from '@/bindings/AuthMethod';
import type { ConnOptions } from '@/bindings/ConnOptions';
import type { Engine } from '@/bindings/Engine';
import type { SshHop } from '@/bindings/SshHop';
import type { SshTunnel } from '@/bindings/SshTunnel';
import type { TlsFiles } from '@/bindings/TlsFiles';
import { Button, Checkbox, FileField, SelectField, TextField } from '@/components/ui';

import styles from './Connections.module.css';

const PEM_FILTER = [{ name: 'PEM', extensions: ['pem', 'crt', 'cer', 'key'] }, { name: 'All files', extensions: ['*'] }];
/** OpenSSH keys usually have no extension (id_ed25519), so every file stays selectable. */
const SSH_KEY_FILTER = [{ name: 'Private key (.pem, OpenSSH)', extensions: ['pem', 'key'] }, { name: 'All files', extensions: ['*'] }];

/** One section of the connection form: a titled, collapsible group. */
export function FormSection({ title, summary, open, onToggle, children }: { title: string; summary?: string; open: boolean; onToggle: () => void; children: React.ReactNode }) {
  return (
    <div className={styles.section}>
      <button type="button" className={styles.sectionHead} aria-expanded={open} onClick={onToggle}>
        <span className={styles.sectionChevron} aria-hidden>
          {open ? '▾' : '▸'}
        </span>
        <span className={styles.sectionTitle}>{title}</span>
        {summary && <span className={styles.sectionSummary}>{summary}</span>}
      </button>
      {open && <div className={styles.sectionBody}>{children}</div>}
    </div>
  );
}

// ---------------------------------------------------------------- auth

type AuthKind = AuthMethod['method'];

const AUTH_OPTIONS: { value: AuthKind; label: string }[] = [
  { value: 'password', label: 'Password (saved in the keychain)' },
  { value: 'aws_iam', label: 'AWS RDS IAM token (AWS CLI)' },
  { value: 'azure_ad', label: 'Azure AD token (Azure CLI)' },
  { value: 'gcp_iam', label: 'Google Cloud SQL IAM token (gcloud CLI)' },
];

/** DynamoDB signs requests itself: access keys, or the credentials of an AWS CLI profile. */
const DYNAMODB_AUTH_OPTIONS: { value: AuthKind; label: string }[] = [
  { value: 'password', label: 'Access keys (secret saved in the keychain)' },
  { value: 'aws_iam', label: 'AWS CLI profile (SSO, assumed roles)' },
];

/** BigQuery takes OAuth tokens: from a key file, or from the Google Cloud CLI (renewed while connected). */
const BIGQUERY_AUTH_OPTIONS: { value: AuthKind; label: string }[] = [
  { value: 'password', label: 'Service account key (JSON file)' },
  { value: 'gcp_iam', label: 'Google Cloud CLI (gcloud auth login)' },
];

/** Engines whose credentials are not a password, with how each option works. */
const ENGINE_AUTH: Partial<Record<Engine, { options: { value: AuthKind; label: string }[]; cliHint: string }>> = {
  dynamodb: { options: DYNAMODB_AUTH_OPTIONS, cliHint: 'Credentials come from `aws configure export-credentials` on every connect. They are never stored.' },
  bigquery: { options: BIGQUERY_AUTH_OPTIONS, cliHint: 'Tokens come from `gcloud auth print-access-token` and are renewed while connected. They are never stored.' },
};

const CLI_TOKEN_HINT = 'A short-lived token is requested from the provider CLI on every connect and used as the password. It is never stored. TLS is required.';

function authOptions(engine: Engine) {
  return ENGINE_AUTH[engine]?.options ?? AUTH_OPTIONS;
}

export function authSummary(auth: AuthMethod, engine: Engine): string {
  return authOptions(engine).find((o) => o.value === auth.method)?.label.split(' (')[0] ?? 'Password';
}

export function AuthFields({ auth, engine, onChange }: { auth: AuthMethod; engine: Engine; onChange: (a: AuthMethod) => void }) {
  const dynamodb = engine === 'dynamodb';
  const select = (method: AuthKind) => {
    if (method === 'aws_iam') onChange({ method, region: '', profile: null });
    else onChange({ method } as AuthMethod);
  };
  return (
    <>
      <SelectField
        label="Authentication"
        className={styles.full}
        value={auth.method}
        onChange={(e) => select(e.target.value as AuthKind)}
        options={authOptions(engine)}
        hint={auth.method === 'password' ? undefined : (ENGINE_AUTH[engine]?.cliHint ?? CLI_TOKEN_HINT)}
      />
      {auth.method === 'aws_iam' && (
        <>
          {!dynamodb && <TextField label="AWS region" mono placeholder="eu-west-1" value={auth.region} onChange={(e) => onChange({ ...auth, region: e.target.value.trim() })} />}
          <TextField
            label="AWS profile"
            mono
            placeholder="default"
            value={auth.profile ?? ''}
            onChange={(e) => onChange({ ...auth, profile: e.target.value.trim() || null })}
          />
        </>
      )}
    </>
  );
}

// ---------------------------------------------------------------- TLS files

export function tlsSummary(files: TlsFiles): string {
  const parts = [files.ca_file && 'CA file', files.client_cert_file && 'client certificate'].filter(Boolean);
  return parts.length ? parts.join(' · ') : 'System trust store';
}

export function TlsFields({ files, onChange }: { files: TlsFiles; onChange: (f: TlsFiles) => void }) {
  return (
    <>
      <FileField
        className={styles.full}
        label="CA certificate"
        hint="PEM bundle trusted instead of the system roots, for servers signed by a private CA."
        value={files.ca_file}
        filters={PEM_FILTER}
        onChange={(ca_file) => onChange({ ...files, ca_file })}
      />
      <FileField
        label="Client certificate"
        hint="A .pem with the certificate, or with the certificate and its key together."
        value={files.client_cert_file}
        filters={PEM_FILTER}
        onChange={(client_cert_file) => onChange({ ...files, client_cert_file })}
      />
      <FileField
        label="Client key"
        hint="Leave empty when the certificate .pem also holds the key. Keys must not be encrypted."
        value={files.client_key_file}
        filters={PEM_FILTER}
        onChange={(client_key_file) => onChange({ ...files, client_key_file })}
      />
    </>
  );
}

// ---------------------------------------------------------------- SSH

export interface HopSecret {
  value: string;
  /** Remove the saved secret on save. */
  clear: boolean;
}

export function newHop(): SshHop {
  return { host: '', port: 22, username: '', auth: { method: 'agent' } };
}

export function sshSummary(tunnel: SshTunnel | null): string {
  if (!tunnel) return 'Off';
  const jumps = tunnel.jumps.length ? ` via ${tunnel.jumps.length} jump host${tunnel.jumps.length > 1 ? 's' : ''}` : '';
  return `${tunnel.host.username || '…'}@${tunnel.host.host || '…'}${jumps}`;
}

function HopFields({
  hop,
  title,
  secret,
  saved,
  onChange,
  onSecret,
  onRemove,
}: {
  hop: SshHop;
  title: string;
  secret: HopSecret;
  saved: boolean;
  onChange: (h: SshHop) => void;
  onSecret: (s: HopSecret) => void;
  onRemove?: () => void;
}) {
  const method = hop.auth.method;
  const secretLabel = method === 'password' ? 'SSH password' : 'Key passphrase';
  return (
    <div className={styles.hop}>
      <div className={styles.hopHead}>
        <span className={styles.hopTitle}>{title}</span>
        {onRemove && (
          <Button size="xs" onClick={onRemove}>
            Remove
          </Button>
        )}
      </div>
      <div className={styles.hostRow}>
        <TextField label="SSH host" mono value={hop.host} onChange={(e) => onChange({ ...hop, host: e.target.value.trim() })} />
        <TextField label="Port" mono inputMode="numeric" value={String(hop.port)} onChange={(e) => onChange({ ...hop, port: Number(e.target.value.replace(/\D/g, '')) || 0 })} />
      </div>
      <TextField label="SSH user" mono value={hop.username} onChange={(e) => onChange({ ...hop, username: e.target.value.trim() })} />
      <SelectField
        label="SSH authentication"
        value={method}
        onChange={(e) => {
          const m = e.target.value as SshHop['auth']['method'];
          onChange({ ...hop, auth: m === 'private_key' ? { method: m, path: '' } : { method: m } });
        }}
        options={[
          { value: 'agent', label: 'SSH agent' },
          { value: 'private_key', label: 'Private key file' },
          { value: 'password', label: 'Password' },
        ]}
      />
      {hop.auth.method === 'private_key' && (
        <FileField
          className={styles.full}
          label="Private key"
          hint="OpenSSH or .pem keys (AWS EC2 and other clouds: RSA, EC, Ed25519, PKCS#8). PuTTY .ppk keys: export as OpenSSH in PuTTYgen first."
          filters={SSH_KEY_FILTER}
          value={hop.auth.path || null}
          onChange={(path) => onChange({ ...hop, auth: { method: 'private_key', path: path ?? '' } })}
        />
      )}
      {method !== 'agent' && (
        <div className={styles.full} style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
          <TextField
            label={secretLabel}
            type="password"
            mono
            autoComplete="off"
            placeholder={saved && !secret.clear ? '•••••••• (saved)' : method === 'private_key' ? 'Only for encrypted keys' : ''}
            hint="Stored in the keychain, never in the settings file."
            value={secret.value}
            onChange={(e) => onSecret({ value: e.target.value, clear: false })}
          />
          {saved && !secret.value && (
            <button type="button" className={styles.linkButton} onClick={() => onSecret({ value: '', clear: !secret.clear })}>
              {secret.clear ? `Keep the saved ${secretLabel.toLowerCase()}` : `Remove the saved ${secretLabel.toLowerCase()}`}
            </button>
          )}
        </div>
      )}
    </div>
  );
}

/**
 * SSH tunnel host and jump hosts. Secrets are indexed in connection order:
 * jump hosts first, the tunnel host last.
 */
export function SshFields({
  tunnel,
  secrets,
  savedSecrets,
  onChange,
  onSecrets,
}: {
  tunnel: SshTunnel | null;
  secrets: HopSecret[];
  /** Per hop: a secret is saved. Empty when hops changed since saving. */
  savedSecrets: boolean[];
  onChange: (t: SshTunnel | null) => void;
  onSecrets: (s: HopSecret[]) => void;
}) {
  if (!tunnel) {
    return (
      <div className={styles.full}>
        <Checkbox label="Connect through an SSH tunnel" help="Use a bastion host, optionally through jump hosts." checked={false} onChange={() => {
          onChange({ host: newHop(), jumps: [] });
          onSecrets([{ value: '', clear: false }]);
        }} />
      </div>
    );
  }
  const secretAt = (i: number) => secrets[i] ?? { value: '', clear: false };
  const setSecret = (i: number, s: HopSecret) => onSecrets(Array.from({ length: tunnel.jumps.length + 1 }, (_, j) => (j === i ? s : secretAt(j))));
  const hostIndex = tunnel.jumps.length;
  return (
    <>
      <div className={styles.full}>
        <Checkbox
          label="Connect through an SSH tunnel"
          help="The database host and port are reached from the SSH host. Host keys are checked against known_hosts."
          checked
          onChange={() => {
            onChange(null);
            onSecrets([]);
          }}
        />
      </div>
      <HopFields
        title="SSH host"
        hop={tunnel.host}
        secret={secretAt(hostIndex)}
        saved={savedSecrets[hostIndex] ?? false}
        onChange={(host) => onChange({ ...tunnel, host })}
        onSecret={(s) => setSecret(hostIndex, s)}
      />
      {tunnel.jumps.map((jump, i) => (
        <HopFields
          key={i}
          title={`Jump host ${i + 1} (visited ${i === 0 ? 'first' : `after jump host ${i}`})`}
          hop={jump}
          secret={secretAt(i)}
          saved={savedSecrets[i] ?? false}
          onChange={(h) => onChange({ ...tunnel, jumps: tunnel.jumps.map((j, k) => (k === i ? h : j)) })}
          onSecret={(s) => setSecret(i, s)}
          onRemove={() => {
            onChange({ ...tunnel, jumps: tunnel.jumps.filter((_, k) => k !== i) });
            onSecrets(secrets.filter((_, k) => k !== i));
          }}
        />
      ))}
      {tunnel.jumps.length < 4 && (
        <div className={styles.full}>
          <Button
            size="sm"
            onClick={() => {
              onChange({ ...tunnel, jumps: [...tunnel.jumps, newHop()] });
              onSecrets([...secrets.slice(0, tunnel.jumps.length), { value: '', clear: false }, ...secrets.slice(tunnel.jumps.length)]);
            }}
          >
            Add jump host
          </Button>
        </div>
      )}
    </>
  );
}

// ---------------------------------------------------------------- options

export type OptionDrafts = Record<keyof ConnOptions, string>;

export function optionDrafts(o: ConnOptions): OptionDrafts {
  return { keepalive_secs: String(o.keepalive_secs), connect_retries: String(o.connect_retries), idle_timeout_mins: String(o.idle_timeout_mins), pool_size: String(o.pool_size) };
}

export function optionsSummary(readOnly: boolean, o: OptionDrafts): string {
  return [readOnly ? 'Read-only' : 'Read-write', `pool ${o.pool_size}`, o.idle_timeout_mins !== '0' ? `idle ${o.idle_timeout_mins} min` : null].filter(Boolean).join(' · ');
}

export function OptionFields({ readOnly, options, errors, onReadOnly, onOption }: { readOnly: boolean; options: OptionDrafts; errors: Partial<OptionDrafts>; onReadOnly: (v: boolean) => void; onOption: (k: keyof ConnOptions, v: string) => void }) {
  const num = (k: keyof ConnOptions, label: string, suffix: string, hint: string) => (
    <TextField label={label} mono inputMode="numeric" suffix={suffix} hint={hint} error={errors[k]} value={options[k]} onChange={(e) => onOption(k, e.target.value.replace(/\D/g, ''))} />
  );
  return (
    <>
      <div className={styles.full}>
        <Checkbox
          label="Read-only connection"
          help="Every transaction is read-only on the server. Grid edits, imports and schema changes are refused."
          checked={readOnly}
          onChange={onReadOnly}
        />
      </div>
      {num('keepalive_secs', 'TCP keepalive', 'seconds', '0 turns keepalives off.')}
      {num('connect_retries', 'Connect retries', 'times', 'After network errors, with backoff. 0–10.')}
      {num('idle_timeout_mins', 'Idle timeout', 'minutes', 'Disconnect when unused. 0 never.')}
      {num('pool_size', 'Catalog pool size', 'connections', 'Explorer, table views and exports. 1–32.')}
    </>
  );
}

export function validateOptions(o: OptionDrafts): Partial<OptionDrafts> {
  const errors: Partial<OptionDrafts> = {};
  const range = (k: keyof ConnOptions, min: number, max: number) => {
    const n = Number(o[k]);
    if (o[k] === '' || !Number.isInteger(n) || n < min || n > max) errors[k] = `${min}–${max}`;
  };
  range('keepalive_secs', 0, 7200);
  range('connect_retries', 0, 10);
  range('idle_timeout_mins', 0, 1440);
  range('pool_size', 1, 32);
  return errors;
}
