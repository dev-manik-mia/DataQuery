import { snippetCompletion, type Completion, type CompletionSource } from '@codemirror/autocomplete';
import { javascript } from '@codemirror/lang-javascript';
import { MSSQL, MySQL, PLSQL, PostgreSQL, SQLDialect, StandardSQL, sql, type SQLNamespace } from '@codemirror/lang-sql';
import { StreamLanguage } from '@codemirror/language';
import { EditorState } from '@codemirror/state';

import type { SchemaModel } from '@/bindings/SchemaModel';
import type { Snippet } from '@/bindings/Snippet';
import type { SqlFlavor } from '@/bindings/SqlFlavor';
import { defaultSchema } from '@/lib/engines';

/** Cypher: clauses, functions and `//` comments; labels and properties stay plain words. */
const CYPHER = SQLDialect.define({
  keywords:
    'match optional where return with unwind order by skip limit asc ascending desc descending create merge on set delete detach remove foreach call yield union all distinct as and or xor not in is null true false case when then else end exists count collect starts ends contains load csv headers from fieldterminator use show drop index constraint for require unique key node relationship database databases transactions terminate begin commit rollback explain profile',
  builtin: 'id elementId labels type keys properties nodes relationships size length head last tail range toString toInteger toFloat toLower toUpper trim date datetime time duration point coalesce startNode endNode',
  slashComments: true,
  doubleQuotedStrings: true,
});

/** CQL: SQL-like with its own keywords, collection types and `//` comments. */
const CQL = SQLDialect.define({
  keywords:
    'select from where and in contains key limit per partition allow filtering order by asc desc group insert into values json default unset if not exists update set delete using ttl timestamp begin unlogged counter batch apply create alter drop truncate keyspace keyspaces table tables columnfamily materialized view index custom type function functions aggregate returns called null input language on to as with primary clustering compact storage replication durable_writes grant revoke all permissions role roles user users list describe desc use token writetime ttl cast distinct is consistency serial tracing paging expand',
  types:
    'ascii bigint blob boolean counter date decimal double duration float inet int smallint text time timestamp timeuuid tinyint uuid varchar varint list set map tuple frozen vector',
  slashComments: true,
  doubleQuotedStrings: false,
});

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
  redis: StandardSQL,
  cql: CQL,
  query_dsl: StandardSQL,
  cypher: CYPHER,
};

/** Dev Tools: method lines, then JSON. */
const HTTP_METHODS = ['GET', 'POST', 'PUT', 'DELETE', 'HEAD'];
const ES_ENDPOINTS = ['_search', '_count', '_doc', '_bulk', '_mapping', '_settings', '_cat/indices?v', '_cat/nodes?v', '_cluster/health', '_sql?format=txt', '_delete_by_query', '_update_by_query', '_aliases', '_tasks'];
const devToolsMode = StreamLanguage.define<{ line: 'start' | 'request' | 'body' }>({
  startState: () => ({ line: 'start' }),
  token(stream, state) {
    if (stream.sol()) state.line = 'start';
    if (stream.eatSpace()) return null;
    if (state.line === 'start' && (stream.match('#') || stream.match('//'))) {
      stream.skipToEnd();
      return 'comment';
    }
    if (state.line === 'start' && stream.match(/^(GET|POST|PUT|DELETE|HEAD|PATCH)\b/i)) {
      state.line = 'request';
      return 'keyword';
    }
    if (state.line === 'request') {
      stream.skipToEnd();
      return 'string-2';
    }
    state.line = 'body';
    if (stream.peek() === '"') {
      stream.next();
      let escaped = false;
      for (let c = stream.next(); c !== undefined; c = stream.next()) {
        if (c === '"' && !escaped) break;
        escaped = !escaped && c === '\\';
      }
      return stream.match(/^\s*:/, false) ? 'propertyName' : 'string';
    }
    if (stream.match(/^-?\d+(\.\d+)?([eE][+-]?\d+)?/)) return 'number';
    if (stream.match(/^(true|false|null)\b/)) return 'atom';
    stream.next();
    return null;
  },
});

const devToolsSource: CompletionSource = (context) => {
  const method = context.matchBefore(/^[A-Za-z]*$/);
  if (method && (method.from !== method.to || context.explicit)) return { from: method.from, options: HTTP_METHODS.map((m) => ({ label: m, type: 'keyword', apply: `${m} ` })) };
  const path = context.matchBefore(/^(GET|POST|PUT|DELETE|HEAD)\s+\S*$/i);
  if (!path) return null;
  const slash = path.text.lastIndexOf('/');
  const from = slash >= 0 ? path.from + slash + 1 : path.from + path.text.search(/\S*$/);
  return { from, options: ES_ENDPOINTS.map((e) => ({ label: e, type: 'function' })) };
};

/** Common commands offered at the start of a Redis line. */
export const REDIS_COMMANDS = [
  'GET', 'SET', 'MGET', 'MSET', 'DEL', 'UNLINK', 'EXISTS', 'TYPE', 'TTL', 'PTTL', 'EXPIRE', 'PEXPIRE', 'PERSIST', 'RENAME',
  'SCAN', 'HGET', 'HSET', 'HGETALL', 'HDEL', 'HSCAN', 'LRANGE', 'LPUSH', 'RPUSH', 'LPOP', 'RPOP', 'LLEN', 'SMEMBERS',
  'SADD', 'SREM', 'SSCAN', 'ZRANGE', 'ZADD', 'ZREM', 'ZSCORE', 'ZSCAN', 'XADD', 'XRANGE', 'XLEN', 'XDEL', 'INCR', 'DECR',
  'INFO', 'DBSIZE', 'SELECT', 'MEMORY', 'OBJECT', 'CLIENT', 'CONFIG', 'SLOWLOG', 'MULTI', 'EXEC', 'DISCARD', 'PING',
];

/** `redis-cli` lines: the first word is the command; quoted arguments are strings. */
const redisMode = StreamLanguage.define<{ first: boolean }>({
  startState: () => ({ first: true }),
  token(stream, state) {
    if (stream.sol()) state.first = true;
    if (stream.eatSpace()) return null;
    const quote = stream.peek();
    if (quote === '"' || quote === "'") {
      stream.next();
      let escaped = false;
      for (let c = stream.next(); c !== undefined; c = stream.next()) {
        if (c === quote && !escaped) break;
        escaped = !escaped && c === '\\';
      }
      state.first = false;
      return 'string';
    }
    stream.eatWhile(/\S/);
    if (state.first) {
      state.first = false;
      return 'keyword';
    }
    return /^-?\d+(\.\d+)?$/.test(stream.current()) ? 'number' : null;
  },
});

const redisSource: CompletionSource = (context) => {
  const word = context.matchBefore(/^\s*[A-Za-z.]*$/);
  if (!word || (word.from === word.to && !context.explicit)) return null;
  const from = word.from + (word.text.length - word.text.trimStart().length);
  return { from, options: REDIS_COMMANDS.map((c) => ({ label: c, type: 'keyword' })), validFor: /^[A-Za-z.]*$/ };
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
  if (flavor === 'query_dsl') {
    return [devToolsMode, EditorState.languageData.of(() => [{ autocomplete: devToolsSource }, { autocomplete: snippetSource(snippets) }])];
  }
  if (flavor === 'redis') {
    return [redisMode, EditorState.languageData.of(() => [{ autocomplete: redisSource }, { autocomplete: snippetSource(snippets) }])];
  }
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
