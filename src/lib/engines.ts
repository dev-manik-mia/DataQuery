import type { ConnConfig } from '@/bindings/ConnConfig';
import type { Engine } from '@/bindings/Engine';
import type { SqlFlavor } from '@/bindings/SqlFlavor';
import type { SslMode } from '@/bindings/SslMode';

/** SQL dialect used for highlighting and analysis. CockroachDB, Redshift and Greenplum speak PostgreSQL. */
export function flavorOf(engine: Engine | undefined): SqlFlavor {
  switch (engine) {
    case 'mysql':
    case 'mariadb':
      return 'mysql';
    case 'sqlserver':
      return 'sqlserver';
    case 'oracle':
      return 'oracle';
    case 'clickhouse':
      return 'clickhouse';
    case 'duckdb':
      return 'duckdb';
    case 'sqlite':
    case 'libsql':
    case 'd1':
      return 'sqlite';
    case 'snowflake':
      return 'snowflake';
    case 'bigquery':
      return 'bigquery';
    case 'mongodb':
      return 'mongodb';
    case 'dynamodb':
      return 'partiql';
    case 'redis':
    case 'valkey':
    case 'dragonfly':
      return 'redis';
    case 'cassandra':
    case 'scylladb':
      return 'cql';
    case 'elasticsearch':
      return 'query_dsl';
    case 'neo4j':
      return 'cypher';
    default:
      return 'postgres';
  }
}

/** Databases contain schemas (not MySQL / MariaDB, where the database is the schema, nor SQLite, whose attached databases are its schemas). */
export function hasSchemas(flavor: SqlFlavor): boolean {
  return !['mysql', 'sqlite', 'clickhouse', 'mongodb', 'partiql', 'redis', 'cql', 'query_dsl', 'cypher'].includes(flavor);
}

/** The schema new objects land in when none is named; Oracle uses the user's own schema. */
export function defaultSchema(flavor: SqlFlavor, user?: string): string | null {
  switch (flavor) {
    case 'postgres':
      return 'public';
    case 'sqlserver':
      return 'dbo';
    case 'duckdb':
      return 'main';
    case 'snowflake':
      return 'PUBLIC';
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
    case 'snowflake':
      return `:${n}`;
    case 'duckdb':
      return `$${n}`;
    case 'clickhouse':
      // Named `{name:Type}` parameters take values in order of first appearance.
      return `{…} #${n}`;
    case 'bigquery':
      // `@name` parameters in order of first appearance, or positional `?`.
      return `@… #${n}`;
    default:
      return `?${n}`;
  }
}

/** Label of the database field; Oracle connects to a service (pluggable database). */
export function databaseLabel(engine: Engine): string {
  if (engine === 'oracle') return 'Service name';
  if (engine === 'duckdb' || engine === 'sqlite') return 'Database file';
  if (engine === 'mongodb') return 'Default database';
  if (engine === 'redis' || engine === 'valkey' || engine === 'dragonfly') return 'Database number';
  if (engine === 'cassandra' || engine === 'scylladb') return 'Keyspace';
  if (engine === 'd1') return 'Database ID or name';
  if (engine === 'bigquery') return 'Project ID';
  return 'Database';
}

/** Engines reached through a cloud API with their own credentials: no host, port, user, password, TLS files or SSH. */
const API_ENGINES: Engine[] = ['bigquery'];

export function connectsByApi(engine: Engine): boolean {
  return API_ENGINES.includes(engine);
}

/** `host:port/database`, or just the database (file, cloud project) where there is no host. */
export function connectionAddress(config: Pick<ConnConfig, 'host' | 'port' | 'database'>): string {
  return config.host ? `${config.host}:${config.port}${config.database ? `/${config.database}` : ''}` : config.database;
}

/** Label, hint and placeholder of the host field. */
export function hostField(engine: Engine): { label: string; hint?: string; placeholder?: string } {
  switch (engine) {
    case 'redshift':
      return {
        label: 'Endpoint',
        hint: 'Cluster or Serverless workgroup endpoint, without the port. Sign-in is by user name and password (IAM credentials are not supported).',
        placeholder: 'examplecluster.abc123xyz789.us-west-2.redshift.amazonaws.com',
      };
    case 'greenplum':
      return { label: 'Coordinator host', hint: 'The coordinator (master) host; segments are reached through it. Also Apache Cloudberry.' };
    case 'snowflake':
      return { label: 'Account', hint: 'Account identifier (myorg-myaccount or xy12345.eu-west-1) or the full host name.' };
    case 'mongodb':
      return { label: 'Host', hint: 'One host, or host1:27017,host2:27017 for a replica set. With DNS seed list, the cluster host name.' };
    case 'cassandra':
    case 'scylladb':
      return { label: 'Contact points', hint: 'One node, or node1,node2:9043 — the driver discovers the rest of the cluster.' };
    case 'dynamodb':
      return { label: 'Endpoint', hint: 'Leave empty for AWS in the region below, or e.g. localhost for DynamoDB Local.' };
    case 'libsql':
      return { label: 'Host', hint: 'The database host (my-db-myorg.turso.io), or paste its libsql:// or https:// URL.' };
    default:
      return { label: 'Host' };
  }
}

/** What the username and password fields hold. DynamoDB signs requests with an access key pair. */
export function credentialLabels(engine: Engine): { username: string; password: string; usernameRequired: boolean } {
  switch (engine) {
    case 'dynamodb':
      return { username: 'Access key ID', password: 'Secret access key', usernameRequired: true };
    case 'mongodb':
      return { username: 'Username', password: 'Password', usernameRequired: false };
    case 'redis':
    case 'valkey':
    case 'dragonfly':
      return { username: 'Username (ACL)', password: 'Password', usernameRequired: false };
    case 'cassandra':
    case 'scylladb':
      return { username: 'Username', password: 'Password', usernameRequired: false };
    case 'elasticsearch':
      return { username: 'Username', password: 'Password, or an API key with no username', usernameRequired: false };
    case 'libsql':
      return { username: 'Username', password: 'Auth token', usernameRequired: false };
    case 'd1':
      return { username: 'Username', password: 'API token', usernameRequired: false };
    default:
      return { username: 'Username', password: 'Password', usernameRequired: true };
  }
}

/** Database and user a new connection form starts with. */
/**
 * SSL mode for a new connection. TLS is off by default so local and internal
 * servers without certificates connect first time; Snowflake, Redshift, Turso (libSQL)
 * and Cloudflare D1 are cloud services with public certificates, so they keep full verification.
 */
export function defaultSslMode(engine: Engine): SslMode {
  return ['snowflake', 'redshift', 'libsql', 'd1'].includes(engine) || connectsByApi(engine) ? 'verify-full' : 'disable';
}

/** Engines that offer cloud IAM sign-in (provider CLI tokens) besides a password. */
export function offersCloudAuth(engine: Engine): boolean {
  return !['mongodb', 'redshift', 'greenplum', 'libsql', 'd1'].includes(engine);
}

export function connectionDefaults(engine: Engine): { database: string; username: string } {
  switch (engine) {
    case 'postgres':
      return { database: 'postgres', username: 'postgres' };
    case 'mysql':
    case 'mariadb':
      return { database: '', username: 'root' };
    case 'cockroachdb':
      return { database: 'defaultdb', username: 'root' };
    case 'redshift':
      return { database: 'dev', username: 'awsuser' };
    case 'greenplum':
      return { database: 'postgres', username: 'gpadmin' };
    case 'sqlserver':
      return { database: 'master', username: 'sa' };
    case 'oracle':
      return { database: 'FREEPDB1', username: 'system' };
    case 'clickhouse':
      return { database: 'default', username: 'default' };
    case 'mongodb':
      return { database: 'test', username: '' };
    case 'redis':
    case 'valkey':
    case 'dragonfly':
      return { database: '0', username: '' };
    case 'cassandra':
    case 'scylladb':
      return { database: '', username: '' };
    case 'elasticsearch':
      return { database: '', username: 'elastic' };
    case 'neo4j':
      return { database: 'neo4j', username: 'neo4j' };
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

const MONGO_SSL_LABELS: Record<SslMode, string> = {
  disable: 'tls=false',
  prefer: 'tls, tlsInsecure',
  require: 'tls, tlsInsecure',
  'verify-ca': 'tls, tlsAllowInvalidHostnames',
  'verify-full': 'tls=true',
};

const REDIS_SSL_LABELS: Record<SslMode, string> = {
  disable: 'redis:// (no TLS)',
  prefer: 'rediss:// (no verification)',
  require: 'rediss:// (no verification)',
  'verify-ca': 'rediss:// (verify certificate)',
  'verify-full': 'rediss:// (verify certificate and host)',
};

const NEO4J_SSL_LABELS: Record<SslMode, string> = {
  disable: 'HTTP :7474 (no TLS)',
  prefer: 'HTTPS :7473 (no verification)',
  require: 'HTTPS :7473 (no verification)',
  'verify-ca': 'HTTPS :7473 (verify certificate)',
  'verify-full': 'HTTPS :7473 (verify certificate and host)',
};

const HTTPS_SSL_LABELS: Record<SslMode, string> = {
  disable: 'HTTP (no TLS)',
  prefer: 'HTTPS (no verification)',
  require: 'HTTPS (no verification)',
  'verify-ca': 'HTTPS (verify certificate)',
  'verify-full': 'HTTPS (verify certificate and host)',
};

/** SSL mode as the engine's own client spells it (`sslmode`, `--ssl-mode`, `encrypt`, TCPS). */
export function sslModeLabel(engine: Engine, mode: SslMode): string {
  if (engine === 'libsql') return HTTPS_SSL_LABELS[mode];
  switch (flavorOf(engine)) {
    case 'mysql':
      return MYSQL_SSL_LABELS[mode];
    case 'sqlserver':
      return SQLSERVER_SSL_LABELS[mode];
    case 'oracle':
      return ORACLE_SSL_LABELS[mode];
    case 'clickhouse':
    case 'snowflake':
    case 'bigquery':
    case 'partiql':
    case 'query_dsl':
      return HTTPS_SSL_LABELS[mode];
    case 'cypher':
      return NEO4J_SSL_LABELS[mode];
    case 'mongodb':
      return MONGO_SSL_LABELS[mode];
    case 'redis':
      return REDIS_SSL_LABELS[mode];
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

/**
 * A libSQL URL pasted into the host field: `libsql://db-org.turso.io` (TLS
 * unless `?tls=0`), `https://…`, `http://localhost:8080` or `ws(s)://…`, with an
 * optional `?authToken=`. `null` when the input is a plain host name.
 */
export function libsqlEndpoint(input: string): { host: string; port: number; sslMode: SslMode; token: string | null } | null {
  const [, scheme = '', rest = ''] = /^(libsql|https?|wss?):\/\/(.+)$/i.exec(input.trim()) ?? [];
  if (!rest) return null;
  let url: URL;
  try {
    url = new URL(`http://${rest}`);
  } catch {
    return null;
  }
  const tlsParam = url.searchParams.get('tls');
  const kind = scheme.toLowerCase();
  const tls = kind === 'libsql' ? tlsParam !== '0' && tlsParam !== 'false' : kind === 'https' || kind === 'wss';
  return {
    host: url.hostname.replace(/^\[(.*)\]$/, '$1'),
    port: url.port ? Number(url.port) : tls ? 443 : 8080,
    sslMode: tls ? 'verify-full' : 'disable',
    token: url.searchParams.get('authToken'),
  };
}
