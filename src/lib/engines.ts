import type { Engine } from '@/bindings/Engine';
import type { SqlFlavor } from '@/bindings/SqlFlavor';

/** SQL dialect used for highlighting and analysis. */
export function flavorOf(engine: Engine | undefined): SqlFlavor {
  return engine === 'mysql' || engine === 'mariadb' ? 'mysql' : 'postgres';
}
