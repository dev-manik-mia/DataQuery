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

const caps = { explorer: true, query_editor: true, grid_edit: true, import_export: true, cancel: true, ddl: true, schemas: true, materialized_views: true, explain: false, monitoring: false, transactions: false, schema_diff: false };
const none = { ...caps, explorer: false, query_editor: false, grid_edit: false, import_export: false, cancel: false, ddl: false };
const engine = (e: string, name: string, mark: string, hue: number, group: string, language: string, port: number | null, summary: string, priority: string, available = false): EngineInfo =>
  ({ engine: e, name, mark, hue, group, language, default_port: port, summary, versions: summary, priority, available, file_based: port === null, capabilities: available ? caps : none }) as EngineInfo;

const ENGINES: EngineInfo[] = [
  engine('postgres', 'PostgreSQL', 'Pg', 250, 'relational', 'SQL', 5432, '5432 · SQL · v12–17', 'MVP', true),
  engine('mysql', 'MySQL', 'My', 220, 'relational', 'SQL', 3306, '3306 · SQL · v5.7–8.4', 'P0', true),
  engine('mariadb', 'MariaDB', 'Ma', 60, 'relational', 'SQL', 3306, '3306 · SQL · v10.6–11', 'P0', true),
  engine('sqlite', 'SQLite', 'Sl', 200, 'relational', 'SQL', null, 'local file · SQL', 'P0'),
  engine('sqlserver', 'SQL Server', 'Ms', 0, 'relational', 'T-SQL', 1433, '1433 · T-SQL · 2017–2022', 'P1'),
  engine('clickhouse', 'ClickHouse', 'Ch', 95, 'analytical', 'SQL', 8123, '8123 · SQL', 'P1'),
  engine('redis', 'Redis', 'Rd', 25, 'key_value', 'Commands', 6379, '6379 · commands · v6–7', 'P1'),
  engine('mongodb', 'MongoDB', 'Mg', 145, 'document', 'MQL', 27017, '27017 · MQL · v5–8', 'P2'),
  engine('cassandra', 'Cassandra', 'Cs', 300, 'wide_column', 'CQL', 9042, '9042 · CQL · v4–5', 'P2'),
  engine('neo4j', 'Neo4j', 'N4', 190, 'search_graph', 'Cypher', 7687, '7687 · Cypher', 'P2'),
];

const conn = (id: string, name: string, environment: string, host: string, database: string, folder: string | null, favorite: boolean, used: number | null): SavedConnection =>
  ({ id, name, folder_id: folder, environment, favorite, has_password: true, created_at: 0, last_used_at: used, config: { engine: 'postgres', host, port: 5432, database, username: 'app_readonly', ssl_mode: 'verify-full', connect_timeout_secs: 10 } }) as SavedConnection;

const now = Date.now();
const CONNECTIONS = [
  conn('c1', 'kestrel-dev', 'Development', 'db.dev.kestrel.internal', 'app_dev', 'f1', true, now - 120_000),
  conn('c2', 'kestrel-staging', 'Staging', 'db.stg.kestrel.internal', 'app', 'f1', false, now - 86_400_000),
  conn('c3', 'kestrel-prod', 'Production', 'db.kestrel.internal', 'app', 'f1', true, now - 3 * 86_400_000),
  conn('c4', 'local-postgres', 'Local', 'localhost', 'postgres', null, false, null),
];

const obj = (kind: string, database: string, schema: string | null, name: string, extra: Partial<Node> = {}): Node =>
  ({ kind, database, schema, name, expandable: kind === 'database' || kind === 'schema', row_estimate: null, comment: null, current: false, ...extra }) as Node;

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
  const out: { from: number; to: number; kind: string; writes: boolean; destructive: string | null; has_params: boolean }[] = [];
  let start = 0;
  for (let i = 0; i <= sql.length; i += 1) {
    if (i === sql.length || sql[i] === ';') {
      const chunk = sql.slice(start, i);
      const lead = chunk.length - chunk.trimStart().length;
      if (chunk.trim()) {
        const body = chunk.trim();
        const destructive = /^\s*(drop)/i.test(body) ? 'drop' : /^\s*delete\b(?![\s\S]*\bwhere\b)/i.test(body) ? 'delete_without_where' : null;
        out.push({ from: start + lead, to: start + lead + body.length, kind: 'select', writes: !/^\s*(select|with|show)/i.test(body), destructive, has_params: false });
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
  get_settings: () => ({ theme: 'dark', row_limit: 500, timeout_secs: 30, confirm_destructive: true, confirm_production_writes: true, keep_history: true, editor_font_size: 13, page_size: 100 }),
  save_settings: (a) => a.settings,
  get_ui_state: () => null,
  set_ui_state: () => null,
  list_connections: () => ({ connections: CONNECTIONS, folders: [{ id: 'f1', name: 'Kestrel' }] }),
  connect: (a) => {
    const c = CONNECTIONS.find((x) => x.id === a.connectionId)!;
    return { session_id: `s-${c.id}`, connection: c, capabilities: caps, flavor: 'postgres', server: { engine: 'postgres', version: 'PostgreSQL 16.4', latency_ms: 84, tls: { version: 'TLSv1.3', verified: true, mode: 'verify-full' }, user: 'app_readonly', database: c.config.database, read_only: false } };
  },
  reconnect: (a) => handlers.connect!({ connectionId: String(a.sessionId).replace(/^s-/, '') }),
  disconnect: () => null,
  list_children: (a) => {
    const p = a.parent as Node | null;
    if (!p) return [obj('database', 'app_dev', null, 'app_dev', { current: true }), obj('database', 'analytics', null, 'analytics'), obj('database', 'postgres', null, 'postgres')];
    if (p.kind === 'database') return ['public', 'billing', 'audit'].map((s, i) => obj('schema', p.name, s, s, { row_estimate: [8, 3, 2][i]! }));
    return [
      obj('table', p.database, p.name, 'customers', { row_estimate: 12040 }),
      obj('table', p.database, p.name, 'invoices', { row_estimate: 3100 }),
      obj('table', p.database, p.name, 'line_items', { row_estimate: 220000 }),
      obj('table', p.database, p.name, 'orders', { row_estimate: 48213 }),
      obj('table', p.database, p.name, 'products', { row_estimate: 410 }),
      obj('table', p.database, p.name, 'sessions', { row_estimate: 1200000 }),
      obj('view', p.database, p.name, 'active_customers'),
      obj('materialized_view', p.database, p.name, 'daily_revenue', { row_estimate: 365 }),
    ];
  },
  search_objects: (a) => ['orders', 'order_items'].filter((n) => n.includes(String(a.query))).map((n) => obj('table', 'app_dev', 'public', n)),
  qualified_name: (a) => `public.${(a.object as Node).name}`,
  generate_select: (a) => `SELECT *\nFROM public.${(a.object as Node).name}\nLIMIT 100;`,
  run_sql: (a) => {
    const sql = String(a.sql);
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
    return { status: 'done', value: { outcomes, skipped: 0, duration_ms: 150 } };
  },
  fetch_more: () => ({ rows: Array.from({ length: 500 }, (_, i) => orderRow(i + 500)), has_more: false, duration_ms: 40 }),
  cancel_query: () => true,
  table_structure: (a) => ({
    object: a.object,
    columns: ORDER_COLS.map(([name, data_type, numeric]) => ({ name, data_type, numeric, nullable: name === 'discount' || name === 'notes', default: name === 'status' ? "'pending'::text" : name === 'created_at' ? 'now()' : null, primary_key: name === 'id', generated: name === 'id', editable: name !== 'id', comment: null })),
    constraints: [
      { kind: 'primary_key', name: 'orders_pkey', definition: 'PRIMARY KEY (id)' },
      { kind: 'foreign_key', name: 'orders_customer_fk', definition: 'FOREIGN KEY (customer_id) REFERENCES customers(id)' },
      { kind: 'check', name: 'orders_status_check', definition: "CHECK (status = ANY (ARRAY['pending'::text, 'paid'::text]))" },
    ],
    indexes: [{ name: 'orders_created_at_idx', definition: 'CREATE INDEX orders_created_at_idx ON public.orders USING btree (created_at DESC)', unique: false, primary: false }],
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
  list_scripts: () => [{ id: 'sc1', name: 'paid-orders', sql: "SELECT * FROM public.orders WHERE status = 'paid';", connection_id: 'c1', created_at: now, updated_at: now }],
  test_connection: () => ({ engine: 'postgres', version: 'PostgreSQL 16.4', latency_ms: 84, tls: { version: 'TLSv1.3', verified: true, mode: 'verify-full' }, user: 'app_readonly', database: 'app_dev', read_only: false }),
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
