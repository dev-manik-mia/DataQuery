import { MySQL, PostgreSQL, sql } from '@codemirror/lang-sql';

import type { SqlFlavor } from '@/bindings/SqlFlavor';

/** CodeMirror SQL language for a dialect. */
export function sqlLanguage(flavor: SqlFlavor) {
  return sql({ dialect: flavor === 'mysql' ? MySQL : PostgreSQL });
}
