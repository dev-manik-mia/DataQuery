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
