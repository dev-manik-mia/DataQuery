# DataQuery — Feature Status

Status of every feature in [`RUSTBASE_FEATURES.md`](RUSTBASE_FEATURES.md) (the target scope).
**Engines done:** PostgreSQL 12–17 ✅ · CockroachDB ✅ (PostgreSQL adapter) · MySQL 5.7–8.4 ✅ · MariaDB 10.6–11 ✅ (shares the MySQL adapter) · SQL Server 2017–2022 ✅ · Oracle 19c–23ai ✅ · ClickHouse ✅ · DuckDB ✅ · Snowflake ✅ (emulator-tested) · MongoDB 5–8 ✅ · DynamoDB ✅ (DynamoDB Local-tested) · Redis 6–8 ✅ · Valkey ✅ · Cassandra 4–5 ✅ · ScyllaDB ✅ · Elasticsearch / OpenSearch ✅ · Neo4j 5 ✅.

Legend: ✅ Done · 🟡 Partial · ⬜ Not started · Pri = priority from the feature list.
"Done" means it works end-to-end (UI → Tauri command → adapter) and, where it touches a database, is covered by integration tests against real servers.

_Last updated: 2026-10-14 (all of §1–§12 complete; SQL Server, Oracle, CockroachDB, ClickHouse, DuckDB, Snowflake, MongoDB, DynamoDB, Redis, Valkey, Cassandra, ScyllaDB, Elasticsearch and Neo4j added; task list in [`tasks.md`](tasks.md))._

---

## Summary

| Milestone | Scope | Status |
|---|---|---|
| **M1** Foundation + PostgreSQL (MVP) | shell, connections, keychain, explorer, editor, run/cancel, bounded grid, settings, tests | ✅ Done |
| **M2** Data management (P0) | table data view, safe inline edits, export/import, history, scripts, structure + DDL | ✅ Done |
| **M3** Beta (P0) | MySQL ✅, MariaDB ✅, SQLite ⬜, production safeguards ✅, URL import ✅, signed updates ✅, Docker integration tests ✅, macOS bundle ✅ (Windows/Linux packaging untested) | 🟡 SQLite left |
| **M4 / M5** | P1 / P2 features of §1–§12 | ✅ Done (see [`tasks.md`](tasks.md)). Engines: SQL Server ✅, Oracle ✅, CockroachDB ✅, ClickHouse ✅, DuckDB ✅, Snowflake ✅, MongoDB ✅, DynamoDB ✅, Redis ✅, Valkey ✅, Cassandra ✅, ScyllaDB ✅, Elasticsearch ✅, Neo4j ✅; SQLite (P0) ⬜ |

### Tests (all passing)

| Suite | Count | Command |
|---|---|---|
| Rust unit tests (core, adapters, store, ssh, app) | 351 | `cargo test --workspace` |
| PostgreSQL 17 integration | 24 | `npm run db:up` then `npm run test:integration` |
| CockroachDB 25.2 integration (PostgreSQL adapter) | 11 | 〃 (port 56257) |
| SQL Server 2022 integration | 21 | 〃 (port 51433) |
| Oracle 23ai Free integration | 15 | `npm run test:oracle` (runs in a Linux container; part of `test:integration`) |
| MySQL 8.4 integration | 22 | 〃 (port 53306) |
| MariaDB 11.4 integration | 22 | 〃 (port 53307, same suite) |
| ClickHouse 25.8 integration | 9 | 〃 (port 58123) |
| DuckDB integration (temporary files, no server) | 9 | 〃 (always runs) |
| Snowflake integration against the fakesnow emulator | 6 | 〃 (port 58000) |
| MongoDB 8.0 integration (single-node replica set) | 11 | 〃 (port 57017) |
| DynamoDB integration against DynamoDB Local | 9 | 〃 (port 58002) |
| Redis 8 and Valkey 8 integration (each test runs on both) | 10 | 〃 (ports 56379, 56380) |
| Cassandra 5.0 and ScyllaDB 2025.1 integration (each test runs on both) | 13 | `npm run db:up:wide` then `npm run test:integration:wide` |
| Elasticsearch 8.15 integration | 9 | `npm run db:up:search` then `npm run test:integration:search` |
| Neo4j 5.26 community integration (HTTP Query API) | 11 | 〃 |
| SSH tunnel integration (OpenSSH bastion → PostgreSQL) | 3 | 〃 (port 52222) |
| Frontend unit tests (Vitest) | 23 | `npm test` |
| `cargo fmt --check`, `clippy -D warnings` (pedantic), `tsc --noEmit`, `eslint` | clean | `npm run check` |

---

## 1. Application shell

| Feature | Pri | Status | Notes |
|---|---|---|---|
| Custom title bar: active connection, engine tag, environment, status | MVP | ✅ | Engine mark + name + env chip + live status (Connected / Connecting / Failed) |
| Explorer (260px, 200–420), tabbed main area, results panel, 24px status bar | MVP | ✅ | |
| Dark (default) and light theme from design tokens | MVP | ✅ | Plus "System" |
| Persist window layout, panel sizes, theme, open tabs (never auto-run) | MVP | ✅ | |
| Shortcuts ⌘↵, ⇧⌘↵, ⌘T, ⌘W, ⌘K | MVP | ✅ | Also ⌘S, Ctrl+Tab |
| Global search across objects and saved scripts (⌘K) | P0 | ✅ | |
| Accessible focus states, keyboard navigation, ARIA labels | MVP | ✅ | |
| Command palette | P1 | ✅ | ⇧⌘P; every command, fuzzy filter, shortcuts shown; unavailable commands hidden |
| Multiple windows, split panes | P1 | ✅ | ⇧⌘N new window; ⌘\\ split; move tabs between panes; split persisted |
| Custom keymaps, editor font settings | P1 | ✅ | Settings → Keyboard records shortcuts, shows conflicts; font family, size, line height, ligatures |
| Localization-ready string table | P1 | ✅ | `src/i18n` with typed keys; shell, navigation and dialogs use it (English only) |

## 2. Connection management

| Feature | Pri | Status | Notes |
|---|---|---|---|
| Connections home: folders, search, All / Favorites / Recent | MVP | ✅ | |
| Row: star, engine, name, env chip, host:port/db, last used, Connect | MVP | ✅ | |
| Engine picker; unavailable engines not selectable | MVP | ✅ | PostgreSQL, MySQL, MariaDB selectable |
| Connection form (name … connect timeout) | MVP | ✅ | Engine-aware defaults and SSL names (`VERIFY_IDENTITY` etc. for MySQL) |
| Environment labels (word + colour) | MVP | ✅ | |
| Production: 3px rule, tinted title bar, writes need confirmation | MVP | ✅ | Enforced in the backend |
| Test connection: version, latency, TLS version, certificate status | MVP | ✅ | All three engines |
| Connect / disconnect / **reconnect**; status chip | MVP | ✅ | Reconnect keeps tabs; network errors flip status to Failed with a Reconnect button |
| Actionable errors with SQLSTATE / driver code | MVP | ✅ | Postgres SQLSTATE (28P01…), MySQL codes (1045, 1792, 3024…) |
| Create, edit, duplicate, delete; folders; favorites; recents | MVP | ✅ | |
| Passwords in the OS keychain | MVP | ✅ | |
| Non-secret metadata in local SQLite, never passwords | MVP | ✅ | |
| TLS verify-full by default, no silent bypass | MVP | ✅ | MySQL `prefer` falls back to plain only if the server has no TLS |
| Import from URL (`postgres://`, `mysql://`, `mariadb://`) | P0 | ✅ | `mongodb+srv://` reports "planned" (MongoDB is P2) |
| Advanced TLS (CA / client cert files) | P1 | ✅ | Both adapters; not tested against a TLS server in Docker |
| SSH tunnels, jump hosts, host-key verification | P1 | ✅ | Password, key file (+passphrase), agent; ~/.ssh/known_hosts + app known_hosts; trust by fingerprint; tested |
| Read-only profiles | P1 | ✅ | Session-level read-only transactions + backend refusal of writes; tested |
| Keepalive / retry / idle timeout / pool size | P1 | ✅ | Per connection; idle sessions closed and can be reconnected |
| Connection export/import | P1 | ✅ | JSON without secrets |
| Cloud IAM auth | P2 | ✅ | AWS RDS (aws CLI), Azure AD (az), GCP Cloud SQL (gcloud); not tested against real clouds |

## 3. Database explorer

| Feature | Pri | Status | Notes |
|---|---|---|---|
| Tree with T / V / M markers | MVP | ✅ | Postgres: db → schema → relations. MySQL: db → tables/views |
| Lazy-load children, skeleton rows | MVP | ✅ | |
| Filter objects by name | MVP | ✅ | |
| Refresh a single node | MVP | ✅ | |
| Context menu: Open data, Copy qualified name, Generate SELECT | MVP | ✅ | Dialect quoting (`"x"` / `` `x` ``) |
| Row-count estimate badges | P0 | ✅ | `pg_class.reltuples` / `information_schema.TABLES.TABLE_ROWS` |
| Indexes, sequences, triggers, functions, procedures in tree | P1 | ✅ | With definitions; tested |
| Table and index sizes | P1 | ✅ | Structure tab and Monitoring |
| Generate INSERT / UPDATE / DELETE templates | P1 | ✅ | |
| Dependency browsing | P1 | ✅ | Dependencies tab: FKs, view sources/users, triggers, owned sequences |

## 4. SQL editor

| Feature | Pri | Status | Notes |
|---|---|---|---|
| CodeMirror 6 with SQL highlighting | MVP | ✅ | PostgreSQL and MySQL dialects |
| Multiple tabs with dirty marker | MVP | ✅ | |
| Line numbers, bracket matching, find/replace | MVP | ✅ | |
| Run statement / selection / script | MVP | ✅ | |
| Row limit (500) and timeout (30s) in toolbar | MVP | ✅ | MySQL: `max_execution_time` (SELECT only); MariaDB: `max_statement_time` |
| Cancel running query | MVP | ✅ | Postgres cancel request; MySQL `KILL QUERY` from a separate connection |
| UI responsive while running, running timer | MVP | ✅ | |
| Basic SQL formatter | P0 | ✅ | |
| Open / save / drag-and-drop `.sql` | P0 | ✅ | |
| Multiple result sets per script | P1 | ✅ | |
| Autocomplete (schema-aware) | P1 | ✅ | Schemas, tables, columns (with aliases), keywords, snippets |
| Dialect-aware formatter | P1 | ✅ | PostgreSQL and MySQL flavours |
| Transaction controls, Explain, parameters, snippets, bookmarks | P1 | ✅ | Manual-commit mode, status indicator, disconnect warning; plan tree; bound `$n`/`?` values |

## 5. Results and data grid

| Feature | Pri | Status | Notes |
|---|---|---|---|
| Virtualized grid, 28px rows, name + type header | MVP | ✅ | |
| Bounded results + "Fetch next" | MVP | ✅ | Both engines keep a resumable server cursor |
| Result tabs + Messages tab | MVP | ✅ | Postgres notices; MySQL `SHOW WARNINGS` |
| Status bar: rows · duration · cursor | MVP | ✅ | |
| Copy cell / row / column as TSV, CSV, JSON | P0 | ✅ | |
| Resize and reorder columns | P0 | ✅ | |
| Table data view: pagination, WHERE, ORDER BY | P0 | ✅ | |
| Inline editing with dirty tracking | P0 | ✅ | |
| Insert / duplicate / delete rows | P0 | ✅ | |
| Pending-changes bar, Preview SQL, Save in one transaction | P0 | ✅ | Each change must hit exactly one row or all roll back |
| Row identity via PK; warn **and confirm** without a key | P0 | ✅ | "Edit anyway…" matches on all comparable columns after confirmation |
| JSON / timestamp viewers | P0 | ✅ | |
| Expanded viewer / JSON editor / binary viewer | P1 | ✅ | Full value fetched by key; hex dump; save to file |
| Hide/pin columns, column search, FK navigation, paste, bulk edit, undo | P1 | ✅ | |

### 5.1 Value rendering contract

| Kind | PostgreSQL | MySQL / MariaDB |
|---|---|---|
| NULL vs empty string distinct | ✅ tested | ✅ tested |
| int8 / unsigned bigint > 2^53 exact | ✅ tested | ✅ tested (`18446744073709551615`) |
| numeric(38,10) exact | ✅ tested | ✅ tested |
| timestamptz / datetime | ✅ RFC 3339 UTC | ✅ server value (no zone) |
| JSON with unicode | ✅ tested | ✅ tested (MariaDB: JSON is text) |
| bytea / binary head + length | ✅ tested | ✅ tested |
| arrays, uuid, inet, interval, enum | ✅ tested | enum/set labelled |

## 6. Schema and structure

| Feature | Pri | Status | Notes |
|---|---|---|---|
| Structure tab: columns | MVP | ✅ | |
| Constraints PK / FK / UNIQUE / CHECK | P0 | ✅ | |
| Generated DDL, Copy, Open in editor | P0 | ✅ | MySQL uses `SHOW CREATE TABLE` |
| Preview SQL for schema changes | P0 | ✅ | Every form shows generated SQL before Apply |
| Create/alter/drop forms, index/constraint management, ER diagram, schema diff | P1 | ✅ | Diff round-trip tested on all engines |
| Framework migration inspection | P2 | ✅ | Prisma, Laravel, Django, Rails, Flyway, Knex, Alembic, TypeORM, Sequelize, golang-migrate |

## 7. Query history and saved scripts

| Feature | Pri | Status |
|---|---|---|
| Local history (SQL, connection, time, duration, status) | MVP | ✅ |
| Never stores result rows | MVP | ✅ |
| Search history | P0 | ✅ |
| Detail panel: Open in tab (no auto-run), Save as script, Copy | P0 | ✅ |
| Saved scripts list | P0 | ✅ |
| Filters by connection/date/status | P1 | ✅ |
| Folders, tags, pinning, autosave; retention and redaction settings | P1 | ✅ |

## 8. Import / export

| Feature | Pri | Status | Notes |
|---|---|---|---|
| Export results / table to CSV & JSON, streamed | P0 | ✅ | |
| Export selected / filtered rows | P0 | ✅ | Selection: grid menu "Export N rows…"; refuses if any value was shortened for display |
| CSV import: mapping, preview, validation, error report | P0 | ✅ | Every row validated first; each bad row reported by number |
| SQL INSERT / XLSX export, JSON import, delimiter/quote/encoding/NULL options, batch + commit size, progress + cancel | P1 | ✅ | |

## 9. Safety and security

| Feature | Pri | Status |
|---|---|---|
| Confirm DROP / TRUNCATE / DELETE-UPDATE without WHERE | MVP | ✅ |
| Production: typed connection name | MVP | ✅ |
| Production: confirm every write (grid edits, imports) | P0 | ✅ |
| Redact secrets from logs and errors | MVP | ✅ tested |
| Default statement timeout and row limit settings | MVP | ✅ |
| Restore workspace without rerunning | MVP | ✅ |
| Telemetry off (none exists) | MVP | ✅ |
| Settings → Security page | MVP | ✅ |
| One statement per call on MySQL (driver multi-statement mode can't run hidden statements) | — | ✅ tested |
| Read-only catalog connections (table views/exports cannot write) | — | ✅ tested on all engines |
| Sensitive-column masking, insecure-connection warnings | P1 | ✅ |

## 10–11. Monitoring, AI assistant (P1/P2)

| Feature | Pri | Status | Notes |
|---|---|---|---|
| Activity: connections gauge, active, long-running, blocked, replication | P1 | ✅ | Refresh every 5 s |
| Running queries with Cancel / Terminate (confirmed) | P1 | ✅ | Production terminate needs the typed name; tested |
| Locks and blocking chains | P1 | ✅ | `pg_locks` / `performance_schema.data_locks` / InnoDB views; tested |
| Table sizes, server settings | P1 | ✅ | |
| Unsupported panels hidden with the reason | P1 | ✅ | |
| AI assistant: opt-in, own key or local model, NL→SQL, explain query/error, schema summary, disclosure, never runs SQL | P1/P2 | ✅ | Not exercised against the live API |

## 12. About and updates

| Feature | Pri | Status | Notes |
|---|---|---|---|
| Version, platform, licence | P0 | ✅ | |
| Release notes | P0 | ✅ | |
| Honest tested-platforms statement | P0 | ✅ | |
| Signed update check / download / install | P0 | ✅ | tauri-plugin-updater with minisign key; feed must be published (see README) |

## 13. Engines

| Engine | Pri | Status |
|---|---|---|
| PostgreSQL 12–17 | MVP | ✅ |
| MySQL 5.7–8.4 | P0 | ✅ (tested on 8.4) |
| MariaDB 10.6–11 | P0 | ✅ (tested on 11.4) |
| SQLite | P0 | ⬜ next |
| SQL Server 2017–2022 | P1 | ✅ (tested on 2022) — `dq-sqlserver` (tiberius, rustls): database → schema (dbo) → tables/views, routines, sequences, indexes, triggers; T-SQL batches with `GO`, `@P1` parameters, cancel and timeout via TDS attention, results kept open between pages, SHOWPLAN/STATISTICS XML plans, DMV monitoring and `KILL`, schema changes (sp_rename, default constraints) |
| Oracle 19c–23ai | P2 | ✅ (tested on 23ai Free) — `dq-oracle` (rust-oracle / ODPI-C, needs Oracle Instant Client): service → schema (user) → tables/views/materialized views, routines, sequences; PL/SQL blocks ended by `/`, `:1` parameters bound by name, server-side cancel, `EXPLAIN PLAN` and `DBMS_XPLAN` plans, `V$SESSION` monitoring, `DBMS_METADATA` DDL, implicit transactions committed per statement unless one is opened |
| CockroachDB | P2 | ✅ (tested on 25.2) — PostgreSQL adapter with a CockroachDB flavor: text `EXPLAIN` tree, `crdb_internal` monitoring with `CANCEL QUERY`/`SESSION`, hidden `rowid` key used for editing keyless tables |
| ClickHouse | P1 | ✅ (tested on 25.8) — `dq-clickhouse` (HTTP/HTTPS via reqwest): database → tables and views, `{p1:Type}` parameters, server sessions keep `SET`/`USE`/temporary tables, `KILL QUERY` cancel and timeout, `readonly=2` read-only profiles, `system.processes` monitoring, `EXPLAIN json=1` plans, `MergeTree` table creation, imports |
| DuckDB | P2 | ✅ (DuckDB 1.5, bundled) — `dq-duckdb`: local database file or `:memory:`, catalog → schema → tables/views, sequences, indexes; exact `HUGEINT`/`DECIMAL`, interrupt-based cancel and timeout, read-only file access, grid edits, imports, JSON plans |
| Snowflake | P2 | ✅ (tested against the fakesnow emulator only) — `dq-snowflake` (own REST client, Arrow results): account/warehouse/role/schema settings, database → schema → tables/views, routines, sequences; Snowflake Scripting blocks, `?` parameters, abort-based cancel and timeout, `GET_DDL`, grid edits on a separate writer session, `EXPLAIN USING JSON` plans |
| MongoDB 5–8 | P2 | ✅ (tested on 8.0) — `dq-mongodb` (official driver, rustls): database → collections/views with document counts; mongosh-style statements (`db.orders.find(…).sort(…)`, `aggregate`, `insertOne`, `updateMany`, `createIndex`, `runCommand`, `show`, `use`) turned into server commands without running JavaScript; exact BSON types (Int64, Decimal128, Date, ObjectId); Documents view (screen 13) with Filter/Project/Sort, Tree/Table/Shell, insert/edit/delete, Aggregation pipeline builder, Indexes, Validation; transactions (`BEGIN`/`COMMIT`/`ROLLBACK`), `killOp` cancel, explain trees, `currentOp` monitoring, imports and exports |
| DynamoDB | P2 | ✅ (tested against DynamoDB Local) — `dq-dynamodb` (own SigV4 client): region → tables with item counts; PartiQL editor with `?` parameters and paging; Documents view with PartiQL filters and attribute lists, insert/edit/delete as DynamoDB JSON with conditional writes; primary key/GSI/LSI listing; transactions queued until `COMMIT` and sent as one `ExecuteTransaction`; access keys or an AWS CLI profile; imports (`BatchWriteItem`) and exports |
| Redis 6–8 | P1 | ✅ (tested on 8) — `dq-redis` (redis-rs, rustls): key browser (screen 12) with SCAN + MATCH/COUNT/TYPE and "Scan more" (KEYS is never sent), prefix groups, TTLs; STRING/HASH/LIST/SET/ZSET/STREAM viewers and editors with paging; TTL edit, memory usage, encoding, rename (RENAMENX), delete with confirmation; redis-cli console where FLUSHALL, FLUSHDB, DEBUG, KEYS and other dangerous commands ask first; command editor with result grids; MULTI/EXEC transactions; INFO/CLIENT LIST monitoring and CLIENT KILL |
| Valkey | P2 | ✅ (tested on 8) — the Redis adapter, detected and labelled as Valkey |
| Cassandra 4–5 | P2 | ✅ (tested on 5.0) — `dq-cassandra` (scylla driver, rustls): keyspaces → tables / materialized views / types / functions; CQL editor with consistency, serial consistency and page-size selectors, paging state kept in memory only, `PK` / `CK ↑↓` header markers, single-partition vs `ALLOW FILTERING` analysis banner; prepared binding by type (exact varint/decimal, collections and UDTs as JSON); grid edits as lightweight transactions (`IF EXISTS`), batches; table views without OFFSET; explain from the analysis and traced explain analyze; system tables monitoring; CQL schema changes |
| ScyllaDB | P2 | ✅ (tested on 2025.1) — the Cassandra adapter, detected and labelled as ScyllaDB |
| Elasticsearch / OpenSearch | P2 | ✅ (tested on Elasticsearch 8.15; OpenSearch detected but untested) — `dq-elasticsearch` (reqwest): cluster → indices / aliases / data streams; Kibana Dev Tools editor (`GET /orders/_search` + JSON body) with method/endpoint completion; hits, `_cat`, `_sql` and text responses as grids; read-only Documents view (Query DSL or query-string filters, `_source` fields, sort, search_after paging); Profile-API explain; `_tasks` monitoring with cancel; mapping-based structure and recreate-index DDL; `_bulk` imports; scroll exports; deleting indices or every document asks first |
| Neo4j 5 | P2 | ✅ (tested on 5.26 community) — `dq-neo4j` over the HTTP Query API (Neo4j 5.19+, explicit transactions 5.26+): databases → labels / relationship types / indexes and constraints with counts; Cypher editor with typed `$parameters`, exact integers, nodes / relationships / paths as JSON; BEGIN / COMMIT / ROLLBACK; EXPLAIN / PROFILE plan trees; read-only Documents view over labels and relationship types (Cypher WHERE / RETURN / ORDER BY); `SHOW TRANSACTIONS` monitoring and `TERMINATE TRANSACTION`; CSV imports as nodes; `DETACH DELETE` without a filter and `DROP` ask first |
| Everything else | P1/P2 | ⬜ |

---

## Known limitations

- MySQL statement timeouts apply to `SELECT` only (server behaviour); MariaDB applies them to every statement.
- MariaDB has no real JSON type, so JSON columns show and export as text.
- MySQL TLS verification uses the Mozilla root store unless a CA file is set on the connection.
- MySQL `EXPLAIN ANALYZE` returns a text tree, shown raw; PostgreSQL and MariaDB plans render as a tree.
- Schema diff does not change primary keys or expression indexes; it lists them as warnings.
- The update feed URL points at the repository in `Cargo.toml`; releases must be built with `npm run app:release` and the signing key.
- Tested on macOS only; Windows and Linux compile from the same code but have not been run.
- Oracle needs Oracle Instant Client installed (found via `DATAQUERY_ORACLE_CLIENT`, `~/Downloads/instantclient_*`, `/opt/oracle/instantclient_*` or the system library path). The Oracle tests run on Linux with the client copied from the test container; the macOS app with a real Instant Client was not tried.
- Oracle and SQL Server have no read-only session mode: read-only profiles are enforced by DataQuery, which refuses writing statements, grid edits, imports and schema changes. Oracle and SQL Server TLS (TCPS, CA files, client certificates) were not tested against a TLS server.
- SQL Server cancels a statement with an attention packet; the monitor can only end whole sessions (`KILL`). Only the first result set of a T-SQL batch is shown.
- Oracle cancel uses `ALTER SYSTEM CANCEL SQL` when the account may run it, else a client break (which needs out-of-band network support to act immediately).
- CockroachDB has no per-table size functions; sizes come from ranges and are approximate.
- ClickHouse has no transactions or row identity: grid editing is off, schema diff is unavailable, and imports are not atomic (rows already inserted stay when a later batch fails). Indexes, foreign keys and unique constraints cannot be created there.
- Snowflake was tested only against the fakesnow emulator, not a real account. Key-pair, SSO and MFA sign-in are not supported (password only). Monitoring is off for Snowflake and DuckDB.
- `EXPLAIN ANALYZE` is not available for ClickHouse, DuckDB and Snowflake; their plans show estimates only.
- Bundling DuckDB grows the macOS app from about 20 MB to 58.6 MB; MongoDB, DynamoDB, Redis, Valkey, Cassandra, ScyllaDB, Elasticsearch and Neo4j bring it to 66.3 MB.
- MongoDB: editor statements are mongosh syntax, not JavaScript (no variables, loops or functions; `bulkWrite` unsupported). `VerifyCa` behaves like `VerifyFull`. Monitoring "terminate" is a `killOp` like cancel (MongoDB cannot end a connection). Editing a document replaces it by `_id` (last write wins).
- DynamoDB: tested only against DynamoDB Local (which does not check signatures; SigV4 is covered by the AWS test vector). Cancel and timeouts stop waiting but cannot stop a request on the server. Writes report 1 affected item; counts exist only without a filter (table estimate); no sort input. In a transaction, reads run immediately and writes run together at `COMMIT` (at most 100).
- Document engines: query results and exports take their columns from the first page/batch; an export stops with a clear error instead of dropping a field that appears later. Imports are written in batches and are not atomic. Editing is off while a projection is applied (the documents shown are partial).
- Redis/Valkey: standalone servers only (no Cluster or Sentinel; with cluster mode on only database 0 is shown); RESP2. Cancel and timeouts kill and reopen the query connection, so an open MULTI is lost. Module types (RedisJSON, time series) are read-only; only RedisJSON values are shown. Binary keys, fields and values show as `\xNN` escapes and are read-only in the editors (use the console). "Largest keys" is the top of a sample of up to 10,000 keys. TLS and SSH tunnels were not tested against a server.
- Cassandra/ScyllaDB: no transactions — one grid change is a lightweight transaction, 2–20 changes a logged batch, larger imports are written row by row (rows written before a failure stay). Cancel and timeouts stop waiting but cannot stop the request on the server; monitoring cannot end other clients. Table views read the skipped rows (no OFFSET) and stop at row 100,000; sorting a table view needs a restricted partition key (CQL rule). Explain shows the read analysis; Explain analyze shows the query trace. Through a single loopback contact point (local Docker, SSH tunnel) all nodes are reached through it. TLS and SSH tunnels were not tested against a server.
- The test databases together need more than the default 8 GB of Docker memory (SQL Server is killed first). Cassandra/ScyllaDB (`wide`) and Elasticsearch/Neo4j (`search`) are compose profiles started with `npm run db:up:wide` / `db:up:search` and tested with `test:integration:wide` / `test:integration:search`; stop SQL Server and Oracle (or give Docker more memory) while they run.
- Elasticsearch: no grid edits (change documents with requests in the editor). Integers beyond 64 bits and decimals beyond `f64` lose precision, and decimals print in shortest form (`120.50` → `120.5`). Filtered Documents-view pages past 10,000 hits need a sort. Explain applies to `_search` requests only. "Terminate" cancels the task. OpenSearch, TLS and SSH tunnels were not tested.
- Neo4j: needs Neo4j 5.19+ (Query API), 5.26+ for explicit transactions; Bolt is not used (default port 7474, Bolt URLs map to it). Results arrive whole (up to 256 MB; rows past the limit stay in memory for "fetch more"). Documents view and grid are read-only (change the graph with Cypher). Monitoring has no connection counts; cancel and terminate both end the transaction. TLS, SSH tunnels and clusters were not tested.
