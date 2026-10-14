import { snippetCompletion, type Completion, type CompletionSource } from '@codemirror/autocomplete';
import { MSSQL, MySQL, PLSQL, PostgreSQL, StandardSQL, sql, type SQLDialect, type SQLNamespace } from '@codemirror/lang-sql';
import { EditorState } from '@codemirror/state';

import type { SchemaModel } from '@/bindings/SchemaModel';
import type { Snippet } from '@/bindings/Snippet';
import type { SqlFlavor } from '@/bindings/SqlFlavor';
import { defaultSchema } from '@/lib/engines';

const DIALECTS: Record<SqlFlavor, SQLDialect> = {
  postgres: PostgreSQL,
  mysql: MySQL,
  sqlserver: MSSQL,
  oracle: PLSQL,
  // Backtick identifiers, like MySQL.
  clickhouse: MySQL,
  duckdb: PostgreSQL,
  snowflake: StandardSQL,
};

/**
 * The completion namespace: `schema.table` (and bare `table` for the default
 * schema) → columns with their types. CodeMirror resolves aliases from FROM.
 */
export function completionSchema(model: SchemaModel | null): SQLNamespace | undefined {
  if (!model) return undefined;
  const ns: Record<string, Record<string, Completion[]> | Completion[]> = {};
  for (const t of model.tables) {
    const columns = t.columns.map((c) => ({ label: c.name, type: 'property', detail: c.data_type + (c.primary_key ? ' · pk' : '') }));
    const schema = t.object.schema;
    if (schema) {
      const tables = (ns[schema] ??= {}) as Record<string, Completion[]>;
      tables[t.object.name] = columns;
    } else {
      ns[t.object.name] = columns;
    }
  }
  return ns;
}

/** Snippets complete by their prefix; `${1:name}` fields are tab stops. */
export function snippetSource(snippets: Snippet[]): CompletionSource {
  const options = snippets.map((s) => snippetCompletion(s.body, { label: s.prefix, detail: s.name, type: 'text', boost: -1 }));
  return (context) => {
    const word = context.matchBefore(/[\w-]+/);
    if (!word || (word.from === word.to && !context.explicit)) return null;
    return { from: word.from, options, validFor: /^[\w-]*$/ };
  };
}

/** CodeMirror SQL language for a dialect, with schema-aware and snippet completion. */
export function sqlLanguage(flavor: SqlFlavor, model: SchemaModel | null = null, snippets: Snippet[] = []) {
  const language = sql({
    dialect: DIALECTS[flavor],
    schema: completionSchema(model),
    defaultSchema: defaultSchema(flavor) ?? undefined,
    upperCaseKeywords: true,
  });
  return [language, EditorState.languageData.of(() => [{ autocomplete: snippetSource(snippets) }])];
}
