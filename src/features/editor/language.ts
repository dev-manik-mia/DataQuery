import { snippetCompletion, type Completion, type CompletionSource } from '@codemirror/autocomplete';
import { javascript } from '@codemirror/lang-javascript';
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
  // Not SQL: MongoDB uses the JavaScript mode below.
  mongodb: StandardSQL,
  partiql: StandardSQL,
};

/** Shell methods offered after `db.<collection>.`. */
const MONGO_METHODS = [
  'find', 'findOne', 'aggregate', 'countDocuments', 'estimatedDocumentCount', 'distinct', 'insertOne', 'insertMany',
  'updateOne', 'updateMany', 'replaceOne', 'deleteOne', 'deleteMany', 'findOneAndUpdate', 'findOneAndReplace',
  'findOneAndDelete', 'createIndex', 'dropIndex', 'getIndexes', 'drop', 'renameCollection', 'stats',
];

/** `db.` completes collection names (and `getCollection`); `db.<name>.` completes shell methods. */
function mongoSource(model: SchemaModel | null): CompletionSource {
  const collections = (model?.tables ?? []).map((t) => t.object.name);
  return (context) => {
    const method = context.matchBefore(/db\.[\w$]+\.\w*/);
    if (method) {
      const from = method.from + method.text.lastIndexOf('.') + 1;
      return { from, options: MONGO_METHODS.map((m) => ({ label: m, type: 'method', apply: `${m}()` })), validFor: /^\w*$/ };
    }
    const db = context.matchBefore(/db\.\w*/);
    if (!db) return null;
    const options: Completion[] = [
      ...collections.filter((c) => /^[A-Za-z_$][\w$]*$/.test(c)).map((c) => ({ label: c, type: 'class' })),
      ...collections.filter((c) => !/^[A-Za-z_$][\w$]*$/.test(c)).map((c) => ({ label: c, type: 'class', apply: `getCollection(${JSON.stringify(c)})` })),
      { label: 'getCollection', type: 'function', apply: 'getCollection("")' },
      { label: 'runCommand', type: 'function', apply: 'runCommand({ })' },
    ];
    return { from: db.from + 3, options, validFor: /^\w*$/ };
  };
}

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

/** CodeMirror language for a dialect, with schema-aware and snippet completion. */
export function sqlLanguage(flavor: SqlFlavor, model: SchemaModel | null = null, snippets: Snippet[] = []) {
  if (flavor === 'mongodb') {
    return [javascript(), EditorState.languageData.of(() => [{ autocomplete: mongoSource(model) }, { autocomplete: snippetSource(snippets) }])];
  }
  const language = sql({
    dialect: DIALECTS[flavor],
    schema: completionSchema(model),
    defaultSchema: defaultSchema(flavor) ?? undefined,
    upperCaseKeywords: true,
  });
  return [language, EditorState.languageData.of(() => [{ autocomplete: snippetSource(snippets) }])];
}
