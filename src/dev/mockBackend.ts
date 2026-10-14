/**
 * Browser preview only (`npm run dev` opened in a normal browser). Replaces the
 * Tauri IPC with in-memory sample data so the UI can be developed and reviewed
 * without the desktop shell. Never bundled into the app: main.tsx imports it
 * only when running in dev outside Tauri.
 */
import { mockIPC, mockWindows } from '@tauri-apps/api/mocks';

import type { Cell } from '@/bindings/Cell';
import type { EngineInfo } from '@/bindings/EngineInfo';
import type { Node } from '@/bindings/Node';
import type { SavedConnection } from '@/bindings/SavedConnection';

type Args = Record<string, unknown>;

const caps = { explorer: true, query_editor: true, grid_edit: true, import_export: true, cancel: true, ddl: true, schemas: true, materialized_views: true, explain: true, monitoring: true, transactions: true, schema_diff: true };
const none = { ...caps, explorer: false, query_editor: false, grid_edit: false, import_export: false, cancel: false, ddl: false };
const engine = (e: string, name: string, mark: string, hue: number, group: string, language: string, port: number | null, summary: string, priority: string, available = false): EngineInfo =>
  ({ engine: e, name, mark, hue, group, language, default_port: port, summary, versions: summary, priority, available, file_based: port === null, capabilities: available ? caps : none }) as EngineInfo;

const ENGINES: EngineInfo[] = [
  engine('postgres', 'PostgreSQL', 'Pg', 250, 'relational', 'SQL', 5432, '5432 · SQL · v12–17', 'MVP', true),
  engine('mysql', 'MySQL', 'My', 220, 'relational', 'SQL', 3306, '3306 · SQL · v5.7–8.4', 'P0', true),
  engine('mariadb', 'MariaDB', 'Ma', 60, 'relational', 'SQL', 3306, '3306 · SQL · v10.6–11', 'P0', true),
  engine('sqlite', 'SQLite', 'Sl', 200, 'relational', 'SQL', null, 'local file · SQL', 'P0'),
  engine('sqlserver', 'SQL Server', 'Ms', 0, 'relational', 'T-SQL', 1433, '1433 · T-SQL · 2017–2022', 'P1', true),
  engine('oracle', 'Oracle', 'Or', 15, 'relational', 'PL/SQL', 1521, '1521 · PL/SQL · 19c–23ai', 'P2', true),
  engine('cockroachdb', 'CockroachDB', 'Cr', 280, 'relational', 'SQL', 26257, '26257 · Postgres wire', 'P2', true),
  engine('clickhouse', 'ClickHouse', 'Ch', 95, 'analytical', 'SQL', 8123, '8123 · SQL', 'P1', true),
  engine('duckdb', 'DuckDB', 'Dk', 85, 'analytical', 'SQL', null, 'local file · SQL', 'P2', true),
  engine('snowflake', 'Snowflake', 'Sf', 210, 'analytical', 'SQL', 443, 'HTTPS · SQL', 'P2', true),
  engine('redis', 'Redis', 'Rd', 25, 'key_value', 'Commands', 6379, '6379 · commands · v6–7', 'P1', true),
  engine('valkey', 'Valkey', 'Vk', 175, 'key_value', 'Commands', 6379, '6379 · Redis-compatible', 'P2', true),
  engine('mongodb', 'MongoDB', 'Mg', 145, 'document', 'MQL', 27017, '27017 · MQL · v5–8', 'P2', true),
  engine('dynamodb', 'DynamoDB', 'Dy', 330, 'document', 'PartiQL', 443, 'AWS · PartiQL', 'P2', true),
  engine('cassandra', 'Cassandra', 'Cs', 300, 'wide_column', 'CQL', 9042, '9042 · CQL · v4–5', 'P2', true),
  engine('scylladb', 'ScyllaDB', 'Sc', 330, 'wide_column', 'CQL', 9042, '9042 · CQL', 'P2', true),
  engine('neo4j', 'Neo4j', 'N4', 190, 'search_graph', 'Cypher', 7687, '7687 · Cypher', 'P2'),
];

const conn = (id: string, name: string, environment: string, host: string, database: string, folder: string | null, favorite: boolean, used: number | null): SavedConnection =>
  ({ id, name, folder_id: folder, environment, favorite, has_password: true, created_at: 0, last_used_at: used, ssh_secrets: [], config: { engine: 'postgres', host, port: 5432, database, username: 'app_readonly', ssl_mode: 'verify-full', connect_timeout_secs: 10, tls_files: { ca_file: null, client_cert_file: null, client_key_file: null }, ssh: null, read_only: false, options: { keepalive_secs: 60, connect_retries: 0, idle_timeout_mins: 0, pool_size: 4 }, auth: { method: 'password' } } }) as SavedConnection;

const now = Date.now();
const CONNECTIONS = [
  conn('c1', 'kestrel-dev', 'Development', 'db.dev.kestrel.internal', 'app_dev', 'f1', true, now - 120_000),
  conn('c2', 'kestrel-staging', 'Staging', 'db.stg.kestrel.internal', 'app', 'f1', false, now - 86_400_000),
  conn('c3', 'kestrel-prod', 'Production', 'db.kestrel.internal', 'app', 'f1', true, now - 3 * 86_400_000),
  conn('c4', 'local-postgres', 'Local', 'localhost', 'postgres', null, false, null),
  { ...conn('c5', 'events-mongo', 'Development', 'events.dev.kestrel.internal', 'analytics', 'f1', false, now - 2 * 86_400_000), config: { ...conn('c5', '', 'Development', 'events.dev.kestrel.internal', 'analytics', null, false, null).config, engine: 'mongodb', port: 27017 } } as SavedConnection,
  { ...conn('c6', 'cache-redis', 'Staging', 'cache.stg.kestrel.internal', '0', 'f1', false, now - 3_600_000), config: { ...conn('c6', '', 'Staging', 'cache.stg.kestrel.internal', '0', null, false, null).config, engine: 'redis', port: 6379 } } as SavedConnection,
  { ...conn('c7', 'telemetry-cassandra', 'Production', '10.0.4.12', 'telemetry', 'f1', false, now - 7 * 86_400_000), config: { ...conn('c7', '', 'Production', '10.0.4.12', 'telemetry', null, false, null).config, engine: 'cassandra', port: 9042 } } as SavedConnection,
];

const CQL_KEYS = [
  { column: 'device_id', role: 'partition', descending: false },
  { column: 'bucket', role: 'partition', descending: false },
  { column: 'ts', role: 'clustering', descending: true },
];
let consistency = 'LOCAL_QUORUM';

const rs = (text: string) => ({ text, binary: false });
const REDIS_KEYS = [
  ['user:1040', 'hash', 7_200_000], ['user:1041', 'hash', null], ['user:1042', 'hash', 8_040_000], ['user:1042:roles', 'set', null],
  ['user:1042:scores', 'zset', null], ['user:1043', 'hash', 21_600_000], ['session:7f3a91', 'string', 1_740_000], ['session:7f3b02', 'string', 720_000],
  ['events:checkout', 'stream', null], ['queue:emails', 'list', null], ['feature:new_checkout', 'string', null],
].map(([k, t, ttl]) => ({ key: rs(k as string), key_type: t as string, type_name: t as string, ttl_ms: ttl as number | null }));

const MONGO_FEATURES = {
  language: 'MQL',
  filter_example: '{ status: "paid", total: { $gte: 100 } }',
  projection_example: '{ email: 1, total: 1 }',
  sort: true,
  aggregation: true,
  validation: true,
  text_format: 'shell',
  new_document: '{\n  \n}',
  filtered_count: true,
};

const field = (key: string, type_label: string, value: Cell, children: unknown[] = []) => ({ key, type_label, value, children, more: 0 });
const mongoDoc = (i: number) => {
  const id = `66f1c0a2e4b0${String(9 + i).padStart(12, '0')}`;
  return {
    key: `ObjectId('${id}')`,
    label: `_id: ObjectId('${id}')`,
    size_bytes: 412 + i,
    text: `{\n  _id: ObjectId('${id}'),\n  status: 'paid',\n  total: Decimal128('${120 + i}.50'),\n  orderNo: Long('9007199254740993'),\n  createdAt: ISODate('2026-10-14T09:12:00.000Z'),\n  coupon: null\n}`,
    truncated: false,
    fields: [
      field('_id', 'ObjectId', text(id)),
      field('status', 'String', text('paid')),
      field('total', 'Decimal128', num(`${120 + i}.50`)),
      field('orderNo', 'Int64', num('9007199254740993')),
      field('createdAt', 'Date', { kind: 'temporal', value: '2026-10-14T09:12:00.000Z', zoned: true }),
      field('coupon', 'Null', NULL),
      field('items', 'Array', { kind: 'json', value: '[{"sku":"A-1","qty":2}]' }, [field('0', 'Object', { kind: 'json', value: '{"sku":"A-1","qty":2}' }, [field('sku', 'String', text('A-1')), field('qty', 'Int32', num('2'))])]),
    ],
  };
};

const obj = (kind: string, database: string, schema: string | null, name: string, extra: Partial<Node> = {}): Node =>
  ({ kind, database, schema, name, expandable: ['database', 'schema', 'table', 'group'].includes(kind), row_estimate: null, comment: null, current: false, ...extra }) as Node;

const text = (value: string): Cell => ({ kind: 'text', value });
const num = (value: string): Cell => ({ kind: 'number', value });
const NULL: Cell = { kind: 'null' };
const emails = ['ana@kestrel.io', 'li.wei@mosaic.dev', 'sam@fieldnote.co', 'r.okafor@lumen.app', 'jo@harbor.studio', 'm.silva@quanta.io'];
const orderRow = (i: number): Cell[] => [
  num(String(9007199254741990n + BigInt(48213 - i))),
  text(emails[i % emails.length]!),
  text('paid'),
  num(`${(i * 37) % 400 + 12}.${String((i * 13) % 100).padStart(2, '0')}`),
  i % 3 === 2 ? NULL : num(i % 2 ? '5.00' : '12.50'),
  i % 4 === 0 ? NULL : i % 4 === 1 ? text('') : text('Gift wrap'),
  { kind: 'json', value: '{"channel":"web","items":3}' },
  { kind: 'temporal', value: `2026-10-14T09:${String(59 - (i % 60)).padStart(2, '0')}:12.${String(100 + i).slice(0, 3)}+00:00`, zoned: true },
];
const ORDER_COLS = [
  ['id', 'bigint', true],
  ['email', 'text', false],
  ['status', 'text', false],
  ['total', 'numeric(12,2)', true],
  ['discount', 'numeric(12,2)', true],
  ['notes', 'text', false],
  ['metadata', 'jsonb', false],
  ['created_at', 'timestamptz', false],
] as const;

function split(sql: string) {
  const out: { from: number; to: number; kind: string; writes: boolean; destructive: string | null; has_params: boolean; param_count: number }[] = [];
  let start = 0;
  for (let i = 0; i <= sql.length; i += 1) {
    if (i === sql.length || sql[i] === ';') {
      const chunk = sql.slice(start, i);
      const lead = chunk.length - chunk.trimStart().length;
      if (chunk.trim()) {
        const body = chunk.trim();
        const destructive = /^\s*(drop)/i.test(body) ? 'drop' : /^\s*delete\b(?![\s\S]*\bwhere\b)/i.test(body) ? 'delete_without_where' : null;
        const params = body.match(/\$\d+/g)?.map((m) => Number(m.slice(1))) ?? [];
        out.push({ from: start + lead, to: start + lead + body.length, kind: 'select', writes: !/^\s*(select|with|show)/i.test(body), destructive, has_params: params.length > 0, param_count: Math.max(0, ...params) });
      }
      start = i + 1;
    }
  }
  return out;
}

const handlers: Record<string, (a: Args) => unknown> = {
  app_info: () => ({ name: 'DataQuery', version: '0.1.0', os: 'macos', arch: 'aarch64', license: 'MIT OR Apache-2.0' }),
  list_engines: () => ENGINES,
  keychain_status: () => ({ available: true, backend: 'macOS Keychain', detail: null }),
  analyze_sql: (a) => split(String(a.sql)),
  format_sql: (a) => String(a.sql),
  get_settings: () => ({
    theme: 'dark',
    language: 'en',
    row_limit: 500,
    timeout_secs: 30,
    confirm_destructive: true,
    confirm_production_writes: true,
    keep_history: true,
    editor_font_size: 13,
    editor_font_family: 'JetBrains Mono',
    editor_line_height: 150,
    editor_ligatures: false,
    keymap: {},
    page_size: 100,
    manual_commit: false,
    mask_columns: true,
    mask_patterns: ['password', 'token', 'secret'],
    history_max_entries: 20000,
    history_max_days: 0,
    history_redact_literals: false,
    autosave_scripts: true,
    ai_enabled: true,
    ai_provider: 'anthropic',
    ai_model: '',
    ai_endpoint: 'http://localhost:11434/v1',
    check_updates_on_launch: false,
  }),
  save_settings: (a) => a.settings,
  get_ui_state: () => null,
  set_ui_state: () => null,
  list_connections: () => ({ connections: CONNECTIONS, folders: [{ id: 'f1', name: 'Kestrel' }] }),
  connect: (a) => {
    const c = CONNECTIONS.find((x) => x.id === a.connectionId)!;
    if (c.config.engine === 'cassandra') {
      const cqlCaps = { ...caps, schemas: false, transactions: false, schema_diff: false };
      return { session_id: `s-${c.id}`, connection: c, capabilities: cqlCaps, flavor: 'cql', documents: null, keys: null, tunneled: false, insecure: null, server: { engine: 'cassandra', version: 'Cassandra 5.0.2', latency_ms: 12, tls: { version: 'TLSv1.3', verified: true, mode: 'verify-full' }, user: 'analyst', database: 'telemetry', read_only: false } };
    }
    if (c.config.engine === 'redis') {
      const redisCaps = { ...caps, ddl: false, schemas: false, materialized_views: false, explain: false, schema_diff: false };
      return { session_id: `s-${c.id}`, connection: c, capabilities: redisCaps, flavor: 'redis', documents: null, keys: { databases: 16, memory_usage: true }, tunneled: false, insecure: null, server: { engine: 'redis', version: 'Redis 7.4.1', latency_ms: 3, tls: null, user: 'default', database: '0', read_only: false } };
    }
    if (c.config.engine === 'mongodb') {
      const mongoCaps = { ...caps, ddl: false, schemas: false, materialized_views: false, schema_diff: false };
      return { session_id: `s-${c.id}`, connection: c, capabilities: mongoCaps, flavor: 'mongodb', documents: MONGO_FEATURES, tunneled: false, insecure: null, server: { engine: 'mongodb', version: 'MongoDB 7.0.14', latency_ms: 18, tls: { version: 'TLSv1.3', verified: true, mode: 'verify-full' }, user: 'analyst', database: 'analytics', read_only: false } };
    }
    return { session_id: `s-${c.id}`, connection: c, capabilities: caps, flavor: 'postgres', tunneled: false, insecure: null, server: { engine: 'postgres', version: 'PostgreSQL 16.4', latency_ms: 84, tls: { version: 'TLSv1.3', verified: true, mode: 'verify-full' }, user: 'app_readonly', database: c.config.database, read_only: false } };
  },
  reconnect: (a) => handlers.connect!({ connectionId: String(a.sessionId).replace(/^s-/, '') }),
  disconnect: () => null,
  list_children: (a) => {
    const p = a.parent as Node | null;
    if (String(a.sessionId) === 's-c6') return Array.from({ length: 16 }, (_, i) => obj('database', String(i), null, String(i), { current: i === 0, expandable: false }));
    if (String(a.sessionId) === 's-c5') {
      if (!p) return ['analytics', 'events', 'admin', 'config', 'local'].map((d, i) => obj('database', d, null, d, { current: i === 0 }));
      return [obj('table', p.name, null, 'orders', { row_estimate: 3118, expandable: false }), obj('table', p.name, null, 'customers', { row_estimate: 1204, expandable: false }), obj('view', p.name, null, 'paid_orders', { expandable: false })];
    }
    if (!p) return [obj('database', 'app_dev', null, 'app_dev', { current: true }), obj('database', 'analytics', null, 'analytics'), obj('database', 'postgres', null, 'postgres')];
    if (p.kind === 'database') return ['public', 'billing', 'audit'].map((s, i) => obj('schema', p.name, s, s, { row_estimate: [8, 3, 2][i]! }));
    if (p.kind === 'table') return [obj('index', p.database, p.schema, `${p.name}_pkey`, { parent: p.name }), obj('trigger', p.database, p.schema, `${p.name}_touch`, { parent: p.name })];
    if (p.kind === 'group') return [obj('function', p.database, p.schema, 'touch', { parent: '' }), obj('function', p.database, p.schema, 'add', { parent: 'a integer, b integer' })];
    return [
      obj('table', p.database, p.name, 'customers', { row_estimate: 12040 }),
      obj('table', p.database, p.name, 'invoices', { row_estimate: 3100 }),
      obj('table', p.database, p.name, 'line_items', { row_estimate: 220000 }),
      obj('table', p.database, p.name, 'orders', { row_estimate: 48213 }),
      obj('table', p.database, p.name, 'products', { row_estimate: 410 }),
      obj('table', p.database, p.name, 'sessions', { row_estimate: 1200000 }),
      obj('view', p.database, p.name, 'active_customers'),
      obj('materialized_view', p.database, p.name, 'daily_revenue', { row_estimate: 365 }),
      obj('group', p.database, p.name, 'functions', { row_estimate: 2 }),
    ];
  },
  search_objects: (a) => ['orders', 'order_items'].filter((n) => n.includes(String(a.query))).map((n) => obj('table', 'app_dev', 'public', n)),
  qualified_name: (a) => `public.${(a.object as Node).name}`,
  generate_sql: (a) => `-- ${String(a.kind)}\nSELECT *\nFROM public.${(a.object as Node).name}\nLIMIT 100;`,
  object_dependencies: () => ({ uses: [{ object: obj('table', 'app_dev', 'public', 'customers'), relation: 'foreign key orders_customer_fk' }], used_by: [{ object: obj('view', 'app_dev', 'public', 'paid_orders'), relation: 'view' }] }),
  schema_model: () => ({
    truncated: false,
    tables: ['customers', 'orders', 'invoices', 'line_items', 'products'].map((n) => ({ object: obj('table', 'app_dev', 'public', n), columns: [{ name: 'id', data_type: 'bigint', nullable: false, primary_key: true }, { name: `${n}_name`, data_type: 'text', nullable: true, primary_key: false }, { name: 'customer_id', data_type: 'bigint', nullable: true, primary_key: false }] })),
    foreign_keys: [
      { name: 'orders_customer_fk', source: obj('table', 'app_dev', 'public', 'orders'), columns: ['customer_id'], target: obj('table', 'app_dev', 'public', 'customers'), target_columns: ['id'], on_update: null, on_delete: null },
      { name: 'invoices_customer_fk', source: obj('table', 'app_dev', 'public', 'invoices'), columns: ['customer_id'], target: obj('table', 'app_dev', 'public', 'customers'), target_columns: ['id'], on_update: null, on_delete: null },
    ],
  }),
  explain: () => ({
    analyzed: false,
    planning_ms: 0.2,
    execution_ms: null,
    raw: '[...]',
    plan: { label: 'Sort', detail: null, cost: 42.1, rows: 500, actual_ms: null, actual_rows: null, loops: null, props: [['Sort Key', 'total DESC']], children: [{ label: 'Seq Scan', detail: 'public.orders', cost: 30, rows: 500, actual_ms: null, actual_rows: null, loops: null, props: [['Filter', "(status = 'paid'::text)"]], children: [] }] },
  }),
  transaction: (a) => (a.action === 'begin' ? 'open' : 'auto'),
  monitor_snapshot: () => ({
    version: 'PostgreSQL 16.4',
    connections: { used: 142, max: 200 },
    active_queries: 7,
    long_running: 2,
    blocked: 1,
    replication: { role: 'primary', lag_secs: 0.4, replicas: 1, state: 'streaming' },
    processes: [
      { pid: 48211, user: 'app', database: 'app', client: '10.0.0.4', state: 'active', duration_ms: 42000, query: 'UPDATE public.orders SET status = $1 WHERE id = $2', wait: null, blocked_by: [], active: true },
      { pid: 48230, user: 'report', database: 'app', client: '10.0.0.9', state: 'active', duration_ms: 3100, query: 'SELECT * FROM public.orders WHERE id = 1 FOR UPDATE', wait: 'Lock: transactionid', blocked_by: [48211], active: true },
    ],
    locks: [{ pid: 48230, lock_type: 'transactionid', mode: 'ShareLock', granted: false, object: null }],
    unavailable: [],
  }),
  server_settings: () => [{ name: 'max_connections', value: '200', unit: null, category: 'Connections', description: 'Sets the maximum number of concurrent connections.' }],
  largest_tables: () => [{ object: obj('table', 'app_dev', 'public', 'sessions'), total_bytes: 734003200, table_bytes: 524288000, index_bytes: 209715200, rows: 1200000 }],
  framework_migrations: () => [{ framework: 'Prisma', table: obj('table', 'app_dev', 'public', '_prisma_migrations'), entries: [{ version: 'a1', name: '20260101_init', applied_at: '2026-01-01 10:00:00', status: 'applied', batch: null }] }],
  list_snippets: () => [{ id: 'sn1', name: 'Count rows', prefix: 'cnt', body: 'SELECT count(*) FROM ${1:table};', updated_at: now }],
  ai_key_status: () => true,
  ai_disclosure: () => ({ provider: 'Anthropic', model: 'claude-opus-5-5', sent: ['Column names and types for 5 tables', 'Your question'], not_sent: ['Credentials and connection details', 'Row data', 'Query results'], tables: ['public.orders', 'public.customers'], characters: 1800 }),
  ai_ask: () => ({ text: 'Here is a query:\n```sql\nSELECT 1;\n```', sql: 'SELECT 1;', model: 'claude-opus-5-5', disclosure: null }),
  run_sql: (a) => {
    const sql = String(a.sql);
    if (String(a.sessionId) === 's-c7') {
      const cols = [['device_id', 'text', false], ['bucket', 'date', false], ['ts', 'timestamp', false], ['metric', 'text', false], ['value', 'double', true]] as const;
      const metrics = ['cpu.temp', 'battery.v', 'rssi'];
      const rows = Array.from({ length: 18 }, (_, i) => [text('dev-88f2'), { kind: 'temporal', value: '2026-10-14', zoned: false }, { kind: 'temporal', value: `2026-10-14T09:${String(59 - i * 3).padStart(2, '0')}:12.000Z`, zoned: true }, text(metrics[i % 3]!), num(((i * 7.3) % 60 + 3).toFixed(2))]);
      const result = { columns: cols.map(([name, type_name, numeric]) => ({ name, type_name, numeric })), rows, has_more: false, command: 'SELECT', affected: null, duration_ms: 9, notices: [] };
      return { status: 'done', value: { outcomes: [{ index: 0, from: 0, to: sql.length, result, error: null, can_fetch_more: false }], skipped: 0, duration_ms: 9, tx: 'auto' } };
    }
    const parts = split(sql);
    if (!a.confirmation && parts.some((p) => p.destructive)) {
      return { status: 'needs_confirmation', request: { title: 'Run a destructive statement?', reasons: ['DELETE without WHERE removes every row.'], require_typed_name: 'kestrel-prod', destructive: true, connection_name: 'kestrel-prod', environment: 'Production' } };
    }
    const outcomes = parts.map((p, index) => {
      const text_ = sql.slice(p.from, p.to);
      if (/totl/.test(text_)) {
        return { index, from: p.from, to: p.to, result: null, can_fetch_more: false, error: { kind: 'syntax', code: '42703', message: 'column "totl" does not exist', detail: null, hint: 'Perhaps you meant to reference the column "orders.total".', action: null, position: text_.indexOf('totl') + 1 } };
      }
      return {
        index,
        from: p.from,
        to: p.to,
        error: null,
        can_fetch_more: true,
        result: { columns: ORDER_COLS.map(([name, type_name, numeric]) => ({ name, type_name, numeric })), rows: Array.from({ length: 500 }, (_, i) => orderRow(i)), has_more: true, command: 'SELECT', affected: null, duration_ms: 142, notices: [] },
      };
    });
    return { status: 'done', value: { outcomes, skipped: 0, duration_ms: 150, tx: 'auto' } };
  },
  fetch_more: () => ({ rows: Array.from({ length: 500 }, (_, i) => orderRow(i + 500)), has_more: false, duration_ms: 40 }),
  cancel_query: () => true,
  table_structure: (a) => ({
    object: a.object,
    columns: ORDER_COLS.map(([name, data_type, numeric]) => ({ name, data_type, numeric, nullable: name === 'discount' || name === 'notes', default: name === 'status' ? "'pending'::text" : name === 'created_at' ? 'now()' : null, primary_key: name === 'id', generated: name === 'id', identity: name === 'id', editable: name !== 'id', comment: null })),
    foreign_keys: [],
    size: { total_bytes: 9437184, table_bytes: 7340032, index_bytes: 2097152 },
    constraints: [
      { kind: 'primary_key', name: 'orders_pkey', definition: 'PRIMARY KEY (id)' },
      { kind: 'foreign_key', name: 'orders_customer_fk', definition: 'FOREIGN KEY (customer_id) REFERENCES customers(id)' },
      { kind: 'check', name: 'orders_status_check', definition: "CHECK (status = ANY (ARRAY['pending'::text, 'paid'::text]))" },
    ],
    indexes: [{ name: 'orders_created_at_idx', definition: 'CREATE INDEX orders_created_at_idx ON public.orders USING btree (created_at DESC)', unique: false, primary: false, columns: ['created_at'], size_bytes: 524288 }],
    row_identity: ['id'],
    full_row_key: [],
    comment: null,
    row_estimate: 48213,
  }),
  table_page: (a) => {
    const r = a.request as { offset: number; limit: number };
    return { columns: ORDER_COLS.map(([name, type_name, numeric]) => ({ name, type_name, numeric })), rows: Array.from({ length: r.limit }, (_, i) => orderRow(i + r.offset)), offset: r.offset, has_more: true, duration_ms: 31 };
  },
  row_count: () => ({ count: 48213, exact: true }),
  table_ddl: () => "CREATE TABLE public.orders (\n  id bigint GENERATED ALWAYS AS IDENTITY NOT NULL,\n  status text NOT NULL DEFAULT 'pending'::text,\n  total numeric(12,2) NOT NULL,\n  CONSTRAINT orders_pkey PRIMARY KEY (id)\n);\n\nCREATE INDEX orders_created_at_idx ON public.orders USING btree (created_at DESC);",
  preview_changes: () => `UPDATE "public"."orders" SET "total" = '84.50' WHERE "id" = '9007199254744201';`,
  apply_changes: (a) => (a.confirmation ? { status: 'done', value: { inserted: 0, updated: 1, deleted: 0, duration_ms: 12 } } : { status: 'needs_confirmation', request: { title: 'Write to production?', reasons: ['This changes data on the production connection kestrel-prod.'], require_typed_name: null, destructive: false, connection_name: 'kestrel-prod', environment: 'Production' } }),
  list_history: () => ({
    total: 3,
    entries: [
      { id: 3, connection_id: 'c1', connection_name: 'kestrel-dev', database: 'app_dev', sql: "SELECT o.id, c.email, o.status, o.total FROM public.orders o JOIN public.customers c ON c.id = o.customer_id WHERE o.status = 'paid'", executed_at: now - 60_000, duration_ms: 142, status: 'ok', rows: 500, error: null },
      { id: 2, connection_id: 'c3', connection_name: 'kestrel-prod', database: 'app', sql: 'SELECT count(*) FROM public.sessions WHERE expires_at < now();', executed_at: now - 3_600_000, duration_ms: 88, status: 'ok', rows: 1, error: null },
      { id: 1, connection_id: 'c2', connection_name: 'kestrel-staging', database: 'app', sql: 'ALTER TABLE public.orders ADD COLUMN refunded_at timestamptz;', executed_at: now - 86_400_000, duration_ms: 15, status: 'error', rows: null, error: 'permission denied for table orders' },
    ],
  }),
  list_scripts: () => [{ id: 'sc1', name: 'paid-orders', sql: "SELECT * FROM public.orders WHERE status = 'paid';", connection_id: 'c1', created_at: now, updated_at: now, folder: 'Reports', tags: ['billing'], pinned: true }],
  test_connection: () => ({ engine: 'postgres', version: 'PostgreSQL 16.4', latency_ms: 84, tls: { version: 'TLSv1.3', verified: true, mode: 'verify-full' }, user: 'app_readonly', database: 'app_dev', read_only: false }),
  session_options: (a) =>
    String(a.sessionId) === 's-c7'
      ? [
          { name: 'consistency', label: 'Consistency', value: consistency, choices: ['ONE', 'QUORUM', 'LOCAL_QUORUM', 'LOCAL_ONE', 'ALL'] },
          { name: 'page_size', label: 'Page size', value: '100', choices: ['100', '500', '1000', '5000'] },
        ]
      : [],
  set_session_option: (a) => {
    if (a.name === 'consistency') consistency = String(a.value);
    return handlers.session_options!({ sessionId: a.sessionId });
  },
  key_roles: (a) => (String(a.sessionId) === 's-c7' ? CQL_KEYS : []),
  query_analysis: (a) => {
    if (String(a.sessionId) !== 's-c7' || !/^\s*select/i.test(String(a.sql))) return null;
    const sql = String(a.sql);
    const single = /device_id\s*=/.test(sql) && /bucket\s*=/.test(sql) && !/allow filtering/i.test(sql);
    return {
      level: single ? 'good' : 'caution',
      message: single ? 'Single-partition read. Partition key fully specified, no ALLOW FILTERING.' : 'ALLOW FILTERING: Cassandra reads whole partitions and discards rows that do not match. It can be slow and time out on large tables.',
      object: { kind: 'table', database: 'telemetry', schema: null, name: 'readings_by_device' },
      keys: CQL_KEYS,
    };
  },
  keys_scan: (a) => {
    const r = a.request as { cursor: string | null };
    return r.cursor ? { keys: [], cursor: null, duration_ms: 2 } : { keys: REDIS_KEYS, cursor: '1840', duration_ms: 4 };
  },
  keys_count: () => 48302,
  keys_value: (a) => {
    const name = (a.request as { key: { key: { text: string } } }).key.key.text;
    const entry = REDIS_KEYS.find((k) => k.key.text === name) ?? REDIS_KEYS[2]!;
    const details = { key: entry.key, key_type: entry.key_type, type_name: entry.type_name, ttl_ms: entry.ttl_ms, memory_bytes: 312, encoding: 'listpack', length: 6 };
    const fv = (f: string, v: string) => ({ field: rs(f), value: rs(v) });
    const value =
      entry.key_type === 'hash'
        ? { type: 'hash', entries: [fv('email', 'li.wei@mosaic.dev'), fv('plan', 'pro'), fv('seats', '12'), fv('last_login', '2026-10-14T09:12:44Z'), fv('region', 'eu-west-1'), fv('flags', 'beta,new_checkout')] }
        : entry.key_type === 'zset'
          ? { type: 'zset', members: [{ member: rs('math'), score: '98.5' }, { member: rs('art'), score: '71' }] }
          : entry.key_type === 'stream'
            ? { type: 'stream', entries: [{ id: '1728897164000-0', fields: [fv('order', '48213'), fv('total', '129.00')] }] }
            : entry.key_type === 'list'
              ? { type: 'list', items: [{ index: 0, value: rs('welcome:ana@kestrel.io') }, { index: 1, value: rs('reset:li.wei@mosaic.dev') }] }
              : entry.key_type === 'set'
                ? { type: 'set', members: [rs('admin'), rs('billing')] }
                : { type: 'string', value: rs('{"user":1042,"csrf":"9f2c"}'), truncated: false };
    return { details, value, next: null, duration_ms: 3 };
  },
  keys_write: (a) =>
    (a.write as { edit: { op: string } }).edit.op === 'delete' && !a.confirmation
      ? { status: 'needs_confirmation', request: { title: 'Run a destructive statement?', reasons: ['Deletes the key from database 0.'], require_typed_name: null, destructive: true, connection_name: 'cache-redis', environment: 'Staging' } }
      : { status: 'done', value: null },
  keys_console: (a) => {
    const line = String(a.line);
    if (/^flush/i.test(line) && !a.confirmation) return { status: 'needs_confirmation', request: { title: 'Run a destructive statement?', reasons: ['FLUSHALL and FLUSHDB delete every key.'], require_typed_name: null, destructive: true, connection_name: 'cache-redis', environment: 'Staging' } };
    const text = /^hget/i.test(line) ? '"pro"' : /^ttl/i.test(line) ? '(integer) 1784' : /^bad/i.test(line) ? "(error) ERR unknown command 'bad'" : 'OK';
    return { status: 'done', value: { text, database: 0, error: text.startsWith('(error)'), duration_ms: 1 } };
  },
  documents_find: (a) => {
    const r = a.request as { cursor: string | null; limit: number };
    const skip = Number(r.cursor ?? 0);
    return { documents: Array.from({ length: Math.min(r.limit, 3118 - skip) }, (_, i) => mongoDoc(skip + i)), next: skip + r.limit < 3118 ? String(skip + r.limit) : null, duration_ms: 18 };
  },
  documents_count: () => ({ count: 3118, exact: true }),
  documents_aggregate: () => ({ documents: [mongoDoc(0), mongoDoc(1)], next: null, duration_ms: 9 }),
  documents_indexes: () => [
    { name: '_id_', keys: '{ _id: 1 }', unique: true, props: [], size_bytes: 135168 },
    { name: 'status_created', keys: '{ status: 1, createdAt: -1 }', unique: false, props: [['partialFilterExpression', '{ status: { $exists: true } }']], size_bytes: 98304 },
  ],
  documents_validation: () => "{\n  validator: { $jsonSchema: { bsonType: 'object', required: ['status'] } },\n  validationLevel: 'strict',\n  validationAction: 'error'\n}",
  documents_statement: () => 'db.orders.find({ status: "paid" }).sort({ createdAt: -1 })',
  documents_write: (a) => {
    const change = a.change as { op: string };
    if (change.op === 'delete' && !a.confirmation) {
      return { status: 'needs_confirmation', request: { title: 'Run a destructive statement?', reasons: ['Deletes the document from orders.'], require_typed_name: null, destructive: true, connection_name: 'events-mongo', environment: 'Development' } };
    }
    return { status: 'done', value: "ObjectId('66f1c0a2e4b0000000000009')" };
  },
  parse_url: () => {
    throw { kind: 'invalid_input', code: null, message: 'This is not a valid connection URL.', detail: null, hint: null, action: null, position: null };
  },
};

export function installMockBackend() {
  mockWindows('main');
  mockIPC((cmd, args) => {
    const handler = handlers[cmd];
    if (handler) return handler((args ?? {}) as Args);
    if (cmd.startsWith('plugin:')) return null;
    console.warn(`[mock] unhandled command ${cmd}`, args);
    return null;
  });
}
