import type { ConnConfig } from '@/bindings/ConnConfig';
import type { Engine } from '@/bindings/Engine';
import type { SqlFlavor } from '@/bindings/SqlFlavor';
import type { SslMode } from '@/bindings/SslMode';

/** SQL dialect used for highlighting and analysis. */
export function flavorOf(engine: Engine | undefined): SqlFlavor {
  return engine === 'mysql' || engine === 'mariadb' ? 'mysql' : 'postgres';
}

/** Database and user a new connection form starts with. */
export function connectionDefaults(engine: Engine): { database: string; username: string } {
  switch (engine) {
    case 'postgres':
      return { database: 'postgres', username: 'postgres' };
    case 'mysql':
    case 'mariadb':
      return { database: '', username: 'root' };
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

/** SSL mode as the engine's own client spells it (`sslmode` vs `--ssl-mode`). */
export function sslModeLabel(engine: Engine, mode: SslMode): string {
  return flavorOf(engine) === 'mysql' ? MYSQL_SSL_LABELS[mode] : mode;
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
