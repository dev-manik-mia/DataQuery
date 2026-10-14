import type { ConnConfig } from '@/bindings/ConnConfig';
import type { Engine } from '@/bindings/Engine';
import type { SqlFlavor } from '@/bindings/SqlFlavor';
import type { SslMode } from '@/bindings/SslMode';

/** SQL dialect used for highlighting and analysis. CockroachDB speaks PostgreSQL. */
export function flavorOf(engine: Engine | undefined): SqlFlavor {
  switch (engine) {
    case 'mysql':
    case 'mariadb':
      return 'mysql';
    case 'sqlserver':
      return 'sqlserver';
    case 'oracle':
      return 'oracle';
    default:
      return 'postgres';
  }
}

/** Databases contain schemas (all but MySQL / MariaDB, where the database is the schema). */
export function hasSchemas(flavor: SqlFlavor): boolean {
  return flavor !== 'mysql';
}

/** The schema new objects land in when none is named; Oracle uses the user's own schema. */
export function defaultSchema(flavor: SqlFlavor, user?: string): string | null {
  switch (flavor) {
    case 'postgres':
      return 'public';
    case 'sqlserver':
      return 'dbo';
    case 'oracle':
      return user ? user.toUpperCase() : null;
    default:
      return null;
  }
}

/** How the editor shows the n-th (1-based) parameter placeholder. */
export function placeholderLabel(flavor: SqlFlavor, n: number): string {
  switch (flavor) {
    case 'postgres':
      return `$${n}`;
    case 'sqlserver':
      return `@P${n}`;
    case 'oracle':
      return `:${n}`;
    default:
      return `?${n}`;
  }
}

/** Label of the database field; Oracle connects to a service (pluggable database). */
export function databaseLabel(engine: Engine): string {
  return engine === 'oracle' ? 'Service name' : 'Database';
}

/** Database and user a new connection form starts with. */
export function connectionDefaults(engine: Engine): { database: string; username: string } {
  switch (engine) {
    case 'postgres':
      return { database: 'postgres', username: 'postgres' };
    case 'mysql':
    case 'mariadb':
      return { database: '', username: 'root' };
    case 'cockroachdb':
      return { database: 'defaultdb', username: 'root' };
    case 'sqlserver':
      return { database: 'master', username: 'sa' };
    case 'oracle':
      return { database: 'FREEPDB1', username: 'system' };
    default:
      return { database: '', username: '' };
  }
}

const MYSQL_SSL_LABELS: Record<SslMode, string> = {
  disable: 'DISABLED',
  prefer: 'PREFERRED',
  require: 'REQUIRED',
  'verify-ca': 'VERIFY_CA',
  'verify-full': 'VERIFY_IDENTITY',
};

const SQLSERVER_SSL_LABELS: Record<SslMode, string> = {
  disable: 'encrypt=false',
  prefer: 'encrypt=optional',
  require: 'encrypt, trustServerCertificate',
  'verify-ca': 'encrypt (verify certificate)',
  'verify-full': 'encrypt (verify certificate and host)',
};

const ORACLE_SSL_LABELS: Record<SslMode, string> = {
  disable: 'TCP',
  prefer: 'TCP',
  require: 'TCPS',
  'verify-ca': 'TCPS (verify certificate)',
  'verify-full': 'TCPS (verify certificate and DN)',
};

/** SSL mode as the engine's own client spells it (`sslmode`, `--ssl-mode`, `encrypt`, TCPS). */
export function sslModeLabel(engine: Engine, mode: SslMode): string {
  switch (flavorOf(engine)) {
    case 'mysql':
      return MYSQL_SSL_LABELS[mode];
    case 'sqlserver':
      return SQLSERVER_SSL_LABELS[mode];
    case 'oracle':
      return ORACLE_SSL_LABELS[mode];
    default:
      return mode;
  }
}

/**
 * Why a connection is not fully secure, mirroring `ConnConfig::insecure_reason`
 * in Rust so the form can warn while typing.
 */
export function insecureReason(config: Pick<ConnConfig, 'host' | 'ssl_mode' | 'ssh'>): string | null {
  const local = ['localhost', '127.0.0.1', '::1'].includes(config.host) && !config.ssh;
  if (local) return null;
  switch (config.ssl_mode) {
    case 'disable':
      return config.ssh ? null : 'TLS is off: passwords and data cross the network unencrypted.';
    case 'prefer':
      return 'TLS is optional: the connection may fall back to plain text.';
    case 'require':
      return "The server certificate is not verified, so the server's identity is not checked.";
    default:
      return null;
  }
}
