# RustBase — Feature List for Implementation

> Desktop database client. **Stack:** Tauri 2 · Rust core · React + TypeScript UI · SQLx and engine-specific crates · OS keychain for secrets.
> Companion files: `rustbase_product_blueprint.md` (product spec), `export/RustBase Screens.html` (UI screens 01–15), `export/RustBase Design System.html` and `ds/theme-ide.css` (tokens).
>
> **Status: nothing below is implemented yet.** This list is the target scope. Build it in milestone order and keep the app runnable after each one. Do not show any control whose feature doesn't work yet. Planned features appear in the UI disabled and labelled `Planned · P1/P2`.

Priority key: **MVP** = first milestone · **P0** = first public beta · **P1** = next release · **P2** = later.

---

## 0. Instructions for Claude Code

1. Read `rustbase_product_blueprint.md` §13 before writing code.
2. Inspect the repo and state which features below already exist. Don't rewrite working code.
3. Implement one milestone at a time (see §14). After each one, run fmt, clippy, tsc, tests and build, then report what passed.
4. Rust commands validate every input from the frontend. Values go into SQL only as parameters. Identifiers are quoted with each engine's own rules. Never concatenate untrusted input into a query.
5. Never log, persist or display secrets. Results are always bounded or paginated. No blocking work on the UI thread.
6. No telemetry, accounts, billing or cloud services unless explicitly requested.

---

## 1. Application shell (screens: all)

| Feature | Priority |
|---|---|
| Tauri 2 window with custom title bar showing active connection, engine tag, environment label, status | MVP |
| Left explorer panel (260px, resizable 200–420), main tabbed area, bottom results panel, 24px status bar | MVP |
| Dark theme (default) and light theme from `ds/theme-ide.css` tokens | MVP |
| Persist window layout, panel sizes, theme, open tabs (SQL text only; never auto-run) | MVP |
| Keyboard shortcuts: Run statement `⌘/Ctrl+↵`, Run script `⇧⌘↵`, New tab `⌘T`, Close tab `⌘W`, Search `⌘K` | MVP |
| Global search across objects and saved scripts (`⌘K`) | P0 |
| Command palette | P1 |
| Multiple windows, split editor panes | P1 |
| Customizable keymaps, editor font settings | P1 |
| Localization-ready string table | P1 |
| Accessible focus states, full keyboard navigation, ARIA labels on icon buttons | MVP |

## 2. Connection management (screens 01, 02a, 02, 11)

| Feature | Priority |
|---|---|
| Connections home: list grouped by folder, search, All / Favorites / Recent filter | MVP |
| Each row: favorite star, engine tag, name, environment chip, host:port/db, last used, Connect | MVP |
| Engine picker (step 1): grouped tiles with port, language, roadmap chip; unavailable engines not selectable | MVP |
| Connection form: name, folder, environment, host, port, database, username, password, SSL mode, connect timeout | MVP |
| Environment labels: Local, Development, Staging, Production (word + colour, never colour alone) | MVP |
| Production: 3px critical rule across window top, tinted title bar, writes require confirmation | MVP |
| Test connection: shows server version, latency, TLS version and certificate status | MVP |
| Connect / disconnect / reconnect; status chip: Connected, Connecting, Failed, Disconnected | MVP |
| Actionable errors with SQLSTATE / driver code (e.g. 28P01 → "check username and password") | MVP |
| Create, edit, duplicate, delete connections; folders; favorites; recents | MVP |
| Passwords stored in OS keychain (macOS Keychain, Windows Credential Manager, Linux Secret Service) | MVP |
| Non-secret metadata saved locally (JSON or SQLite); never contains passwords | MVP |
| TLS verify-full by default; no silent bypass | MVP |
| Import connection from URL (`postgres://…`, `mysql://…`, `mongodb+srv://…`) without storing secret in URL | P0 |
| Advanced TLS: CA / client cert / key files | P1 |
| SSH tunnels, jump hosts, host-key verification | P1 |
| Read-only connection profiles (enforced with a read-only transaction or session setting, not just in the UI) | P1 |
| Keepalive, retry, idle timeout, pool size settings | P1 |
| Connection export/import without secrets | P1 |
| Cloud IAM auth (AWS RDS, Azure AD, GCP) | P2 |

## 3. Database explorer (screens 03, 04, 05)

| Feature | Priority |
|---|---|
| Tree: database → schema → tables / views / materialized views (T / V / M markers) | MVP |
| Lazy-load children on expand; skeleton rows while loading | MVP |
| Filter objects by name | MVP |
| Refresh a single node (not the whole tree) | MVP |
| Context menu: Open data, Copy qualified name (correctly quoted), Generate SELECT | MVP |
| Row-count estimate badges where cheap (e.g. `pg_class.reltuples`) | P0 |
| Indexes, sequences, triggers, functions, procedures | P1 |
| Table and index sizes | P1 |
| Generate INSERT / UPDATE / DELETE templates | P1 |
| Dependency browsing | P1 |

## 4. SQL editor (screen 03)

| Feature | Priority |
|---|---|
| CodeMirror 6 editor with SQL syntax highlighting (keywords `--syn-kw`, types `--syn-type`, strings `--positive`, numbers `--caution`) | MVP |
| Multiple query tabs with dirty marker | MVP |
| Line numbers, bracket matching, find/replace | MVP |
| Run current statement (highlighted), run selection, run full script | MVP |
| Toolbar shows row limit (default 500) and statement timeout (default 30s) | MVP |
| Cancel running query (PostgreSQL: `pg_cancel_backend` from a separate connection) | MVP |
| UI stays responsive while a query runs; running timer shown | MVP |
| Basic SQL formatter | P0 |
| Open / save `.sql` files; drag-and-drop `.sql` files | P0 |
| Context-aware autocomplete (schemas, tables, columns, keywords) | P1 |
| Dialect-aware formatter | P1 |
| Transaction controls (BEGIN / COMMIT / ROLLBACK, status-bar indicator) | P1 |
| Explain plan view | P1 |
| Parameterized queries, snippets, bookmarks | P1 |
| Multiple result sets per script | P1 |

## 5. Results and data grid (screens 03, 04)

| Feature | Priority |
|---|---|
| Virtualized grid (TanStack Table + virtualizer), 28px rows, 12px mono, header shows column name + type | MVP |
| Bounded results: fetch first N rows, "Showing first 500 rows. More rows are available." + Fetch next | MVP |
| Result tabs + Messages tab (errors, notices, affected rows, duration) | MVP |
| Status bar: rows · duration · cursor position | MVP |
| Copy cell / row / column; copy as TSV, CSV, JSON | P0 |
| Resize and reorder columns | P0 |
| Table data view: server-side pagination, WHERE filter, ORDER BY sort | P0 |
| Inline cell editing with dirty tracking (tint + ● marker) | P0 |
| Insert row (+), duplicate row, delete row (− with strikethrough) | P0 |
| Pending-changes action bar: counts, Discard, Preview SQL, Save N changes (single transaction) | P0 |
| Row identity via primary key; warn and confirm when no PK / unique key exists | P0 |
| JSON / timestamp viewers | P0 |
| Expanded cell viewer, JSON/JSONB editor, binary viewer | P1 |
| Hide / pin columns, per-column search | P1 |
| Foreign-key navigation | P1 |
| Paste from spreadsheet, bulk edit with preview | P1 |
| Undo pending edits | P1 |

### 5.1 Value rendering contract (Rust → TS)

| Kind | Wire format | Display |
|---|---|---|
| NULL | `{"kind":"null"}` | italic muted `NULL` token on sunk background |
| Empty string | `{"kind":"text","value":""}` | dashed-outline `empty` token (never the same as NULL) |
| int8 / numeric / decimal | string | right-aligned, tabular, exact digits (never a JS number) |
| timestamptz / timestamp | RFC 3339 string | server value, muted offset; local time in tooltip |
| json / jsonb | raw text | single-line preview, opens viewer |
| bytea / binary | `{length, head_hex}` | `\x89504e47… · 4.2 KB`, full value on demand |
| arrays, enums, uuid, inet, interval | typed or text | dedicated renderer or text fallback |
| Unsupported | server text cast + type name | read-only in grid |

Tests required: NULL vs empty, int8 > 2^53, numeric(38,10), timestamptz with offsets, JSON with unicode, bytea.

## 6. Schema and structure (screen 05)

| Feature | Priority |
|---|---|
| Structure tab: columns (name, type, nullable, default, key) | MVP |
| Constraints: PK, FK, UNIQUE, CHECK | P0 |
| Generated DDL view (from catalog), Copy, Open in editor | P0 |
| Preview SQL for supported schema changes; never execute without review | P0 |
| Create / alter / drop tables and columns via forms | P1 |
| Index and constraint management | P1 |
| ER diagram | P1 |
| Schema diff between connections; migration SQL preview | P1 |
| Framework migration inspection (Prisma, Laravel, Django) | P2 |

## 7. Query history and saved scripts (screen 06)

| Feature | Priority |
|---|---|
| Local history: SQL text, connection, timestamp, duration, status (OK / Error / Cancelled) | MVP |
| Search history | P0 |
| Detail panel: full SQL, Open in new tab (never auto-run), Save as script, Copy | P0 |
| Saved scripts list | P0 |
| Never store result rows in history | MVP |
| Filters by connection, date, status | P1 |
| Folders, tags, pinning, autosave | P1 |
| History retention and redaction settings | P1 |

## 8. Import / export

| Feature | Priority |
|---|---|
| Export results / table to CSV and JSON, streamed to disk | P0 |
| Export selected rows / filtered results | P0 |
| CSV import with column mapping, preview, validation, error report | P0 |
| SQL INSERT export | P1 |
| XLSX export, JSON import | P1 |
| Delimiter / quoting / encoding / NULL representation options | P1 |
| Batch inserts, commit size, progress + cancel | P1 |

## 9. Safety and security (screens 08, 11)

| Feature | Priority |
|---|---|
| Confirm `DROP`, `TRUNCATE`, `DELETE`/`UPDATE` without `WHERE` on all connections | MVP |
| Production: type connection name to confirm destructive statements | MVP |
| Production: confirm every write, including grid edits and imports | P0 |
| Redact passwords, tokens and connection strings from logs and errors | MVP |
| Default statement timeout and row limit (settings) | MVP |
| Restore workspace without rerunning queries | MVP |
| Telemetry off by default | MVP |
| Settings → Security page: keychain status, redaction, safety toggles, data sharing | MVP |
| Sensitive-column masking | P1 |
| Insecure-connection warnings | P1 |

## 10. Monitoring (screen 07) — P1

- Server activity: connections vs max (gauge with threshold tick), active queries, blocked sessions, replication lag.
- Running queries from `pg_stat_activity` (or engine equivalent) with Cancel / Terminate (with confirmation).
- Locks and blocking chains.
- Table / index sizes, server version and settings.
- Availability depends on engine, version and privileges, so hide anything unsupported.

## 11. AI assistant (screen 09) — P1/P2, opt-in

- Off by default; user supplies their own API key or a local model.
- Natural language → SQL, explain query, explain error, summarize schema.
- Disclosure panel per request: what is sent (schema names/types, question) and what is not (credentials, rows, results).
- Generated SQL is inserted into the editor and **never auto-executed**.
- Destructive generated SQL goes through the normal confirmation flow.

## 12. About and updates (screen 10)

| Feature | Priority |
|---|---|
| Version, platform, licence (Free, no account) | P0 |
| Signed update check / download / install on restart | P0 |
| Release notes | P0 |
| Tested-platforms statement (honest) | P0 |

---

## 13. Database engines

Each engine is a separate adapter implementing a shared Rust trait plus engine-specific capabilities. Don't force non-relational engines into the SQL interface.

```rust
trait DatabaseAdapter {
    async fn test(&self, cfg: &ConnConfig) -> Result<ServerInfo>;
    async fn connect(&self, cfg: &ConnConfig) -> Result<SessionId>;
    async fn disconnect(&self, s: SessionId) -> Result<()>;
    async fn list_children(&self, s: SessionId, node: &NodePath) -> Result<Vec<Node>>; // lazy
    async fn execute(&self, s: SessionId, q: QueryRequest) -> Result<QueryHandle>;     // bounded
    async fn fetch(&self, h: QueryHandle, limit: u32) -> Result<ResultBatch>;
    async fn cancel(&self, h: QueryHandle) -> Result<()>;
    fn capabilities(&self) -> Capabilities; // explain, monitoring, transactions, edit, …
}
```

| Engine | Group | Default port | Language | Priority | Notes |
|---|---|---|---|---|---|
| PostgreSQL 12–17 | Relational | 5432 | SQL | **MVP** | also Timescale, Supabase, Neon |
| MySQL 5.7–8.4 | Relational | 3306 | SQL | P0 | no schemas; database = schema |
| MariaDB 10.6–11 | Relational | 3306 | SQL | P0 | shares MySQL adapter with dialect flags |
| SQLite | Relational | file | SQL | P0 | file picker instead of host/port |
| SQL Server 2017–2022 | Relational | 1433 | T-SQL | P1 | `tiberius` crate; database → schema (dbo) |
| Oracle 19c–23ai | Relational | 1521 | PL/SQL | P2 | |
| CockroachDB | Relational | 26257 | SQL | P2 | Postgres wire protocol |
| ClickHouse | Analytical | 8123 | SQL | P1 | read-heavy; no row edits |
| DuckDB | Analytical | file | SQL | P2 | local files, Parquet/CSV |
| Snowflake | Analytical | HTTPS | SQL | P2 | |
| Redis 6–7 | Key-value | 6379 | Commands | P1 | key browser (screen 12) |
| Valkey | Key-value | 6379 | Commands | P2 | Redis-compatible adapter |
| MongoDB 5–8 | Document | 27017 | MQL | P2 | document view (screen 13) |
| DynamoDB | Document | AWS | PartiQL | P2 | |
| Cassandra 4–5 | Wide-column | 9042 | CQL | P2 | CQL view (screen 14) |
| ScyllaDB | Wide-column | 9042 | CQL | P2 | Cassandra adapter |
| Elasticsearch / OpenSearch | Search | 9200 | Query DSL | P2 | |
| Neo4j | Graph | 7687 | Cypher | P2 | |

### 13.1 Redis (screen 12) — P1
- Key browser using `SCAN` with `MATCH` + `COUNT` (never `KEYS`), cursor and "Scan more".
- Key types: STR, HASH, LIST, SET, ZSET, STREAM, each with its own viewer and editor.
- TTL view/edit, memory usage, encoding, rename, delete with confirmation.
- Command console; `FLUSHALL`, `FLUSHDB`, `DEBUG` and other dangerous commands ask for confirmation.

### 13.2 MongoDB (screen 13) — P2
- Databases → collections / views tree with document counts.
- Filter / Project / Sort inputs (validated JSON), Find, paginated results.
- Tree / Table / JSON views; BSON type labels (Int64, Decimal128, Date, ObjectId, Null), exact numerics.
- Tabs: Documents, Aggregation pipeline builder, Indexes, Validation rules.
- Insert / edit / delete documents with confirmation.

### 13.3 Cassandra / ScyllaDB (screen 14) — P2
- Keyspaces → tables / materialized views / UDTs.
- CQL editor; consistency-level selector; page size; paging state held in memory only.
- Column headers mark partition keys (PK) and clustering keys (CK ↑/↓).
- Query analysis banner: single-partition read vs `ALLOW FILTERING` scan warning.

### 13.4 Capability matrix (screen 15)
Settings → Engines shows each adapter's runtime capabilities. Controls for a capability an engine doesn't support are hidden for that engine.

---

## 14. Milestones

1. **M1 — Foundation + PostgreSQL slice (MVP):** shell, theme, connection CRUD + keychain, test/connect/disconnect, lazy explorer, CodeMirror editor, run + cancel, bounded results grid, errors/status, settings persistence, tests, README.
2. **M2 — Data management (P0):** table data view with pagination/filter/sort, safe inline edits + action bar, CSV/JSON export (streamed), CSV import preview, history search, saved scripts, structure + DDL.
3. **M3 — Beta (P0):** MySQL, MariaDB, SQLite adapters; production safeguards; URL import; signed updates; integration tests against real DBs in Docker; packaging on each tested OS.
4. **M4 — P1:** SQL Server, ClickHouse, Redis; SSH/TLS; autocomplete; explain; monitoring; transactions; schema editing; ER diagram.
5. **M5 — P2:** MongoDB, Cassandra/ScyllaDB, Oracle, DuckDB, Snowflake, DynamoDB, Elasticsearch, Neo4j; AI assistant; schema diff; team features.

## 15. Testing checklist (per milestone)

- [ ] `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`
- [ ] `tsc --noEmit`, lint, frontend unit tests (Vitest)
- [ ] Integration tests against real engine containers (`docker compose`), including invalid credentials, network failure, timeout and cancel
- [ ] Value-type round-trip tests (§5.1)
- [ ] Credentials absent from logs, settings files and error messages (assert in tests)
- [ ] Large result (1M rows) stays bounded and the UI stays responsive
- [ ] Inline edits target the correct rows; no-PK tables warn
- [ ] Destructive statements require confirmation; Production requires typed confirmation
- [ ] Workspace restore does not execute SQL
- [ ] `tauri build` succeeds on each declared platform
