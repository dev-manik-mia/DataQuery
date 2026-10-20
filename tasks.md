# DataQuery — Tasks for §1–§12 of `RUSTBASE_FEATURES.md`

Every feature row in sections 1–12 of [`RUSTBASE_FEATURES.md`](RUSTBASE_FEATURES.md), as a task.
`[x]` = done and verified (`npm run check` + `npm run test:integration`), `[ ]` = open.
Rows already shipped before this list was written are marked done with *(existing)*.

**Status (2026-10-14): every task in §1–§12 is done.** Notes under "Verification" say which ones could not be exercised against a live external service.
Milestones (§14) and the engines not listed below are tracked in [`feature_completed.md`](feature_completed.md).

Rules that apply to every task (from §0): validate all input in Rust, values only as bound parameters,
identifiers quoted by the engine `Dialect`, no secrets in logs/files/errors, bounded results, nothing on the UI thread,
no telemetry. Shared logic goes in `dq-core` (`ops`, `sql`) and `src/lib`; adapters only add catalog/engine primitives.

---

## 1. Application shell

- [x] T1.1 Custom title bar: connection, engine, environment, status *(existing)*
- [x] T1.2 Explorer 260px (200–420), tabbed main area, results panel, 24px status bar *(existing)*
- [x] T1.3 Dark/light theme from tokens *(existing)*
- [x] T1.4 Persist layout, panel sizes, theme, open tabs (never auto-run) *(existing)*
- [x] T1.5 Shortcuts ⌘↵ ⇧⌘↵ ⌘T ⌘W ⌘K *(existing)*
- [x] T1.6 Global search across objects and saved scripts *(existing)*
- [x] T1.7 Accessible focus, keyboard navigation, ARIA labels *(existing)*
- [x] T1.8 **Command palette** (⇧⌘P): every app action (run, new tab, format, explain, transactions, open settings pages, monitoring, ER diagram, schema diff, new window, split, theme…) with fuzzy filter and shortcut hints; actions unavailable for the engine are hidden.
- [x] T1.9 **Multiple windows**: "New window" opens another app window with its own workspace (shares saved connections, settings and backend sessions registry); window capability covers all windows.
- [x] T1.10 **Split editor panes**: split the main area right; each pane has its own active tab; move tab to other pane; close split. Persisted with the workspace.
- [x] T1.11 **Customizable keymaps**: Settings → Editor → Keyboard lists commands; record a new chord, reset to default; conflicts reported. Stored in settings; used by the global handler, the editor and the palette.
- [x] T1.12 **Editor font settings**: font family, size, line height, ligatures on/off.
- [x] T1.13 **Localization-ready string table**: `src/i18n/` with an English catalog and a typed `t()`; shell, navigation, settings, dialogs and common actions read from it.

## 2. Connection management

- [x] T2.1–T2.13 Home, rows, engine picker, form, environments, production rule, test connection, connect/reconnect, actionable errors, CRUD/folders/favorites/recents, keychain, local metadata, verify-full by default *(existing)*
- [x] T2.14 Import from URL (`postgres://`, `mysql://`, `mariadb://`) *(existing)*
- [x] T2.15 **Advanced TLS**: CA file, client certificate and key files per connection (PostgreSQL via rustls, MySQL/MariaDB via `SslOpts`); files validated and read on connect; paths only stored, never contents.
- [x] T2.16 **SSH tunnels**: host, port, user, password or private key (+ passphrase) in the keychain; **jump hosts** (chained); **host-key verification** against `~/.ssh/known_hosts` plus app-trusted keys, unknown keys shown with SHA-256 fingerprint and must be trusted explicitly; changed keys refused.
- [x] T2.17 **Read-only connection profiles**: enforced in the session (`default_transaction_read_only` / `SET SESSION TRANSACTION READ ONLY`) and in the backend for grid edits, imports and schema changes; shown in the title bar.
- [x] T2.18 **Keepalive, retry, idle timeout, pool size** per connection: TCP keepalive interval, connect retries with backoff, auto-disconnect after idle minutes, catalog pool size.
- [x] T2.19 **Connection export/import without secrets**: JSON file with folders + connections (no passwords, SSH secrets or tokens); import creates new ids and reports skipped items.
- [x] T2.20 **Cloud IAM auth** (P2): AWS RDS IAM (token via SigV4 presign using the AWS CLI credential chain), Azure AD (`az account get-access-token`), GCP Cloud SQL IAM (`gcloud sql generate-login-token`); token used as the password, never stored.

## 3. Database explorer

- [x] T3.1–T3.6 Tree with T/V/M, lazy load + skeletons, filter, single-node refresh, context menu, row-count badges *(existing)*
- [x] T3.7 **Indexes, sequences, triggers, functions, procedures** in the tree (PostgreSQL and MySQL/MariaDB), with "Show definition".
- [x] T3.8 **Table and index sizes** (`pg_total_relation_size`/`pg_relation_size`, `information_schema` DATA_LENGTH/INDEX_LENGTH) in the Structure tab.
- [x] T3.9 **Generate INSERT / UPDATE / DELETE templates** from the context menu (dialect-quoted, placeholders, never executed).
- [x] T3.10 **Dependency browsing**: Dependencies tab — what the object references (FKs, view sources) and what references it (FKs, views, triggers).

## 4. SQL editor

- [x] T4.1–T4.9 CodeMirror 6 + highlighting, tabs + dirty marker, line numbers/brackets/find, run statement/selection/script, row limit/timeout in toolbar, cancel, responsive + timer, basic formatter, open/save/drag-drop `.sql` *(existing)*
- [x] T4.15 Multiple result sets per script *(existing)*
- [x] T4.10 **Context-aware autocomplete**: schemas, tables, columns (from a bounded catalog snapshot per database), keywords; alias-aware column completion.
- [x] T4.11 **Dialect-aware formatter**: PostgreSQL vs MySQL keyword sets, casts, `ON DUPLICATE KEY`, backticks, `$$` bodies kept intact; tested per flavor.
- [x] T4.12 **Transaction controls**: manual-commit mode, BEGIN / COMMIT / ROLLBACK buttons, status-bar indicator (Auto-commit / In transaction / Failed transaction), warn before disconnect with an open transaction.
- [x] T4.13 **Explain plan view**: EXPLAIN (JSON) for PostgreSQL and MySQL/MariaDB rendered as a tree with cost/rows/time; EXPLAIN ANALYZE only for read-only statements.
- [x] T4.14 **Parameterized queries** (`$1`, `?`, `:name`): prompt for values, send as bound parameters, remember last values per tab. **Snippets**: saved, insertable via palette/autocomplete. **Bookmarks**: line bookmarks in the editor gutter, next/previous.

## 5. Results and data grid

- [x] T5.1–T5.12 Virtualized grid, bounded results, result/messages tabs, status bar, copy as TSV/CSV/JSON, resize/reorder, table data view, inline edits, insert/duplicate/delete, pending bar, row identity + confirm, JSON/timestamp viewers *(existing)*
- [x] T5.0 Value rendering contract §5.1 with tests *(existing)*
- [x] T5.13 **Expanded cell viewer, JSON/JSONB editor, binary viewer**: full value fetched on demand (bounded), JSON edited with validation into a pending change, hex dump + size + save to file for binary.
- [x] T5.14 **Hide / pin columns, per-column search** in every grid.
- [x] T5.15 **Foreign-key navigation**: structured FK metadata; FK cells open the referenced table filtered to that row.
- [x] T5.16 **Paste from spreadsheet** (TSV/CSV into selected cells or as new rows) and **bulk edit with preview** (set a column for the selected rows).
- [x] T5.17 **Undo pending edits** (⌘Z / redo ⇧⌘Z in the table view, per edit step).

## 6. Schema and structure

- [x] T6.1–T6.3 Structure tab, constraints, generated DDL + copy/open *(existing)*
- [x] T6.4 **Preview SQL for schema changes; never execute without review** (shared preview → confirm → run flow used by all forms below).
- [x] T6.5 **Create / alter / drop tables and columns via forms** (dialect-specific DDL builders in `dq-core`).
- [x] T6.6 **Index and constraint management**: create/drop index, add/drop FK, UNIQUE, CHECK.
- [x] T6.7 **ER diagram**: SVG of the schema's tables with FK edges, pan/zoom, click to open.
- [x] T6.8 **Schema diff between connections** with migration SQL preview (tables, columns, indexes, constraints).
- [x] T6.9 **Framework migration inspection** (P2): Prisma `_prisma_migrations`, Laravel `migrations`, Django `django_migrations`, plus Rails/Flyway/Knex tables when present.

## 7. Query history and saved scripts

- [x] T7.1–T7.5 Local history, never stores rows, search, detail panel, saved scripts *(existing)*
- [x] T7.6 **Filters by connection, date range, status** in the History page.
- [x] T7.7 **Folders, tags, pinning, autosave** for saved scripts (autosave writes linked scripts on edit).
- [x] T7.8 **History retention and redaction settings**: max entries / max age, redact string literals option, clear by connection.

## 8. Import / export

- [x] T8.1–T8.2 Streamed CSV/JSON export, export selection/filtered rows *(existing)*
- [x] T8.3 **CSV import error report**: validate every row before writing; report each bad row (row number, column, reason), not only the failing batch.
- [x] T8.4 **SQL INSERT export** (dialect-quoted, batched multi-row inserts).
- [x] T8.5 **XLSX export** (streamed rows, typed numbers kept exact as text where needed) and **JSON import** (array of objects, mapped by key).
- [x] T8.6 **Delimiter / quoting / encoding / NULL representation options** for CSV import and export (UTF-8, UTF-8 BOM, UTF-16LE, Windows-1252, Latin-1).
- [x] T8.7 **Batch inserts, commit size, progress + cancel** for imports and exports (progress events, cancel token; commit size splits into transactions and reports what was committed).

## 9. Safety and security

- [x] T9.1–T9.8 Destructive confirmations, typed production name, production writes, redaction, timeout/row limit settings, restore without rerun, telemetry off, Security page *(existing)*
- [x] T9.9 **Sensitive-column masking**: name patterns (password, token, secret, ssn, card…) editable in Settings; masked in grids/exports-to-clipboard until revealed per column.
- [x] T9.10 **Insecure-connection warnings**: SSL `disable`/`prefer`/`require` (unverified) on non-local hosts warns in the form, on connect and in the title bar.

## 10. Monitoring (screen 07)

- [x] T10.1 Server activity: connections vs max (gauge with 80% tick), active queries, long-running count, blocked sessions, replication lag; auto-refresh every 5 s.
- [x] T10.2 Running queries (`pg_stat_activity` / `information_schema.PROCESSLIST`) with Cancel / Terminate behind a confirmation naming pid, user and connection.
- [x] T10.3 Locks and blocking chains.
- [x] T10.4 Table / index sizes, server version and settings.
- [x] T10.5 Hide anything the engine, version or privileges do not support (each panel reports `unavailable` with the reason).

## 11. AI assistant (screen 09)

- [x] T11.1 Off by default; user supplies an Anthropic API key (keychain) or a local OpenAI-compatible endpoint (e.g. Ollama).
- [x] T11.2 Natural language → SQL, explain query, explain error, summarize schema.
- [x] T11.3 Disclosure panel per request: what is sent (schema names/types, question, SQL) and what is not (credentials, rows, results).
- [x] T11.4 Generated SQL inserted into the editor and never auto-executed; destructive SQL goes through the normal confirmation on Run.

## 12. About and updates

- [x] T12.1 Version, platform, licence *(existing)*
- [x] T12.3 Release notes *(existing)*
- [x] T12.4 Honest tested-platforms statement *(existing)*
- [x] T12.2 **Signed update check / download / install on restart** (`tauri-plugin-updater` with a minisign public key, release feed, signature verified before install; release build config produces signed artifacts).

---

## 13. Engines: SQL Server, Oracle, CockroachDB

- [x] T13.1 Core dialects: T-SQL (`[name]`, `N'…'`, `@P1`, `OFFSET … FETCH`, `IDENTITY(1,1)`, `0x…`) and Oracle (`"NAME"`, `:1`, `FETCH FIRST`, `INSERT ALL`, `HEXTORAW`) in `dq-core/sql/dialect.rs`; paging, templates, binary/boolean literals, insert batching and parameter limits are dialect methods, not per-engine branches.
- [x] T13.2 Lexer and splitter: `GO` and `/` separator lines, T-SQL `BEGIN … END` blocks and routine bodies, PL/SQL units, Oracle q-quotes, `#temp` names, `@P1` and `:1` parameters (`:new` is not a parameter); transaction effects per engine (Oracle DDL commits, `SET TRANSACTION`, T-SQL `BEGIN TRAN`).
- [x] T13.3 Schema-change SQL per engine: SQL Server `ADD`, `ALTER COLUMN`, `sp_rename`, default constraints; Oracle `ADD (…)`, `MODIFY (…)`, `CASCADE CONSTRAINTS`; DEFAULT before NOT NULL everywhere.
- [x] T13.4 `Session::explain` hook (default: the engine's EXPLAIN statement); CockroachDB text-tree parser; SQL Server showplan XML reader; Oracle PLAN_TABLE tree and DBMS_XPLAN.
- [x] T13.5 SQL Server adapter `dq-sqlserver`: connection (TLS modes, CA/client cert, keepalive, SSH endpoint), query worker that keeps results open between pages and cancels with TDS attention, statement timeout, catalog pool switching databases, explorer, structure, DDL, dependencies, schema model, edits/imports in one transaction (XACT_ABORT), monitoring and KILL.
- [x] T13.6 Oracle adapter `dq-oracle`: Instant Client discovery with an actionable error, EZConnect (TCP/TCPS), NLS settings that round-trip values, bind by name, implicit-commit semantics with explicit transactions, server-side cancel and timeout that keep the session, explorer (schemas, groups), structure, DBMS_METADATA DDL, dependencies, schema model, monitoring, ALTER SYSTEM cancel/kill.
- [x] T13.7 CockroachDB through the PostgreSQL adapter: flavor detection, `defaultdb`, hidden schemas, NULL-safe catalog columns, sizes when available, `rowid` key, `crdb_internal` monitoring and cancel.
- [x] T13.8 App and UI: registry, transaction start per dialect, URL import (`sqlserver://`, `mssql://`, `oracle://`, `cockroachdb://`, `encrypt`/`trustServerCertificate`), editor dialects (MSSQL, PL/SQL), parameter labels, type suggestions, identity labels, service-name field, SSL labels per engine, schema-diff schemas, monitor titles, tested-platforms text.
- [x] T13.9 Test databases: `sqlserver`, `oracle`, `cockroach` services with seeds in `docker/`; integration tests `crates/dq-sqlserver/tests`, `crates/dq-oracle/tests`, `crates/dq-postgres/tests/cockroach.rs`; `npm run test:oracle`.
- [x] T13.10 One rustls crypto provider (ring) installed for the whole process (`dq_core::tls`), since the SQL Server driver links a second one.

---

## 14. Engines: ClickHouse, DuckDB, Snowflake

- [x] T14.1 Core dialects: ClickHouse (backticks, `{p1:Type}` parameters cast from `Nullable(String)`, `MergeTree` tables, `Nullable(…)` columns, `MODIFY COLUMN`), DuckDB (PostgreSQL-like, `?` parameters, `db.schema.table`), Snowflake (`"NAME"`, `?`/`:1` parameters, `$$` strings, `//` comments, `SET DATA TYPE`); new `Dialect` methods `qualifies_database`, `identity_clause`, `aborts_transaction_on_error`.
- [x] T14.2 Lexer and analyzer: ClickHouse `#` comments and backslash escapes, Snowflake Scripting blocks (`DECLARE`/`BEGIN … END`; a lone `BEGIN` is a transaction), Snowflake DDL ends a transaction, named parameters.
- [x] T14.3 Config: `ConnConfig.properties` (warehouse, role, schema, account), file-based engines (`Engine::is_file_based`: absolute path or `:memory:`, no host/SSH), URL import for `clickhouse://` (`secure=`), `snowflake://user@account/DB/SCHEMA?warehouse=&role=` and `duckdb:///path`.
- [x] T14.4 ClickHouse adapter `dq-clickhouse`: HTTP(S) with a server session (`SET`, `USE`, temporary tables kept), `readonly=2` for read-only profiles and catalog reads, `KILL QUERY … SYNC` cancel and timeout, affected rows from `X-ClickHouse-Summary`, explorer, structure, DDL, dependencies, schema model, imports, `system.processes` monitoring, `EXPLAIN json=1` plans.
- [x] T14.5 DuckDB adapter `dq-duckdb` (bundled, with json and parquet): query worker thread, `InterruptHandle` cancel and timeout, exact `HUGEINT`/`DECIMAL`, zoned timestamps in UTC, read-only file access, catalog via `duckdb_*()` functions, edits in one transaction, JSON plans.
- [x] T14.6 Snowflake adapter `dq-snowflake`: REST client (login, query, async polling, session renewal, abort, chunked Arrow/JSON results, logout), account host rule, INFORMATION_SCHEMA + `SHOW … KEYS` catalog, `GET_DDL`, grid edits and imports on a separate writer session, read-only guard, `EXPLAIN USING JSON`.
- [x] T14.7 App and UI: registry, editor dialects, parameter labels, type suggestions, identity labels, Database file field with "New…" for DuckDB (host, auth, TLS and SSH hidden), Snowflake Account/Warehouse/Role/Schema fields, monitor process source, URL hints, tested-platforms text.
- [x] T14.8 Test databases: `clickhouse` (25.8, :58123) and `fakesnow` (Snowflake emulator, :58000) services; DuckDB tests use temporary files. Integration tests in `crates/dq-clickhouse/tests`, `crates/dq-duckdb/tests`, `crates/dq-snowflake/tests`.

---

## 15. Engines: MongoDB, DynamoDB

- [x] T15.1 Core document layer `dq-core/document.rs`: `DocumentStore` trait (find with cursor paging, count, aggregate, indexes, validation, insert/replace/delete one document, find statement, imports), `Document`/`DocField` trees with the engine's type labels and exact numbers, grid conversion and streaming exports that refuse to drop fields; `Session::documents()`.
- [x] T15.2 Flavors `mongodb` (mongosh statements split on lines and `;`, method-based read/write/destructive classification incl. `deleteMany({})`, `drop`, `$out`/`$merge`) and `partiql` (`"name"`, `?`); `BEGIN`/`COMMIT`/`ROLLBACK` transaction effects; shell and PartiQL templates for Generate find/insert/update/delete.
- [x] T15.3 Shared ops route document engines: table exports and Documents-view exports run the engine's find statement; imports validate every row first, then insert documents in batches (new fields get inferred types); SQL-only paths (grid page, edits, schema changes) are refused.
- [x] T15.4 Config: `mongodb://` and `mongodb+srv://` URLs (several hosts, `replicaSet`, `authSource`, `tls`), optional MongoDB user; DynamoDB region/endpoint/access key rules, AWS CLI profiles via `aws configure export-credentials`.
- [x] T15.5 MongoDB adapter `dq-mongodb` (official driver, rustls): mongosh-style statements turned into server commands (no JavaScript is run), exact BSON values, `use`, transactions on a driver session, cancel by `killOp` on a tagged operation, timeouts, read-only guard, explorer with document counts, sampled structure, `createCollection`/`createIndex` definitions, explain trees (IXSCAN/COLLSCAN…), `currentOp` monitoring and `killOp`, Documents view, aggregation, indexes, validation rules.
- [x] T15.6 DynamoDB adapter `dq-dynamodb` (own SigV4 client over reqwest): PartiQL with typed parameters and `NextToken` paging, buffered transactions sent as one `ExecuteTransaction` at `COMMIT`, conditional writes for document edits, `BatchWriteItem` imports, primary key/GSI/LSI listing, `DescribeTable` definitions, throttling retries.
- [x] T15.7 UI: Documents view (screen 13) with Filter/Project/Sort, Tree/Table/Shell(JSON) layouts, cursor paging, insert/edit in the engine's text form, delete with backend confirmation, Aggregation pipeline builder, Indexes and Validation tabs; MongoDB editor mode (JavaScript highlighting, `db.` completion); connection form fields (hosts, replica set, auth database, SRV; endpoint, region, access keys or AWS profile); Import dialog maps new fields for documents.
- [x] T15.8 Test databases: `mongodb` (8.0 single-node replica set with auth, :57017, seed `docker/mongodb-seed.js`) and `dynamodb` (DynamoDB Local, :58002); integration tests in `crates/dq-mongodb/tests` and `crates/dq-dynamodb/tests`.

---

## 16. Engines: Redis, Valkey

- [x] T16.1 Core key-value layer `dq-core/keyvalue.rs`: `KeyValueStore` (SCAN steps with MATCH/TYPE/cursor, DBSIZE, paged values per type, one-key edits, console, key exports), binary-safe `RString` (UTF-8 text or reversible `\xNN` escapes), `Session::keys()`, `NodeKind::Key`, `ExportSource::Keys`.
- [x] T16.2 `redis` flavor: one command per line, `redis-cli` argument splitting, read-only command table, `FLUSHALL`/`FLUSHDB`, `KEYS`, `DEBUG`, `SHUTDOWN`, `CONFIG SET`, `CLIENT KILL`, … destructive (always confirmed); `MULTI`/`EXEC`/`DISCARD` as transaction words (`Dialect::commit_transaction` / `rollback_transaction`).
- [x] T16.3 Config: `redis://`, `rediss://`, `valkey://`, `valkeys://` URLs; optional ACL user; database number.
- [x] T16.4 Adapter `dq-redis` (redis-rs, rustls) for Redis and Valkey: dedicated query connection (SELECT, MULTI, blocking commands), cancel/timeout with `CLIENT KILL` + reconnect, editor grids, redis-cli-formatted console replies, key browser reads (SCAN + TYPE/PTTL pipeline, HSCAN/SSCAN/LRANGE/ZRANGE/XRANGE pages, MEMORY USAGE, OBJECT ENCODING), guarded writes (WATCH for list items, RENAMENX, UNLINK), INFO/CLIENT LIST monitoring, sampled largest keys, CLIENT KILL.
- [x] T16.5 UI: key browser replacing the explorer (database and type filters, pattern, prefix groups, TTLs, cursor and "Scan more", new key, export matching keys), key tab (type · size, TTL, memory, encoding, Edit TTL, Rename, Delete…, per-type editors with paging), console panel and console tab, Redis editor mode with command completion, connection form fields.
- [x] T16.6 Test databases: `redis` (8, :56379) and `valkey` (8, :56380), password `dataquery`; integration tests in `crates/dq-redis/tests` run against both.

---

## 17. Engines: Cassandra, ScyllaDB

- [x] T17.1 `cql` flavor and `CqlDialect`: `"Name"` quoting with lower-case folding, `?` markers typed by the prepared statement, `//` comments and `$$` bodies, `BEGIN … APPLY BATCH` kept as one statement, cqlsh `CONSISTENCY`/`TRACING`/`PAGING` as settings; no transactions (manual-commit mode skips engines without them).
- [x] T17.2 Paging without `OFFSET`: `Dialect::client_side_offset` (table views read `offset + limit` rows and drop the offset, capped at 100,000); `ALLOW FILTERING` added when the table view filters.
- [x] T17.3 Schema changes in CQL: create table (primary key required), add/drop column, rename key columns, single-column secondary indexes; everything Cassandra cannot do is refused with a reason.
- [x] T17.4 Core hooks: `Session::key_roles` (partition/clustering columns with order), `Session::options`/`set_option` (consistency, serial consistency, page size), `NodeKind::Type` and a Types group for UDTs.
- [x] T17.5 Read analysis `dq-core/sql/cql.rs` (screen 14 banner): single-partition, multi-partition (IN), token range, full scan, `ALLOW FILTERING`, incomplete partition key.
- [x] T17.6 Adapter `dq-cassandra` (scylla driver, rustls) for Cassandra and ScyllaDB: keyspaces → tables / materialized views / types / functions, prepared binding of text values by type, exact varint/decimal, collections and UDTs as JSON, paging state kept in memory, edits with `IF EXISTS`, batches, explain from the analysis and traced explain analyze, system tables monitoring.
- [x] T17.7 UI: consistency and page-size selectors in the editor toolbar, analysis banner, `PK` / `CK ↑↓` markers in result and table headers, CQL highlighting, contact points and local datacenter fields.
- [x] T17.8 Test databases: `cassandra` (5.0, :59042, PasswordAuthenticator) and `scylla` (2025.1, :59043); seed `docker/cassandra-seed.cql`; integration tests in `crates/dq-cassandra/tests` run against both.

---

## 18. Engines: Elasticsearch / OpenSearch, Neo4j

- [x] T18.1 `query_dsl` flavor: Kibana Dev Tools requests (`GET /index/_search` + JSON body) split at method lines, `#`/`//` comments; read/write classification by method and endpoint (`_search`, `_count`, `_sql`, `_cat` read; `DELETE /index` and `_delete_by_query` with `match_all` destructive; cluster settings and index close/shrink ask first).
- [x] T18.2 `cypher` flavor: backtick names, `//` comments, `$name` parameters (named, in order of first appearance); clause-based classification (`DETACH DELETE` without a filter, `DROP`, `TERMINATE` are destructive; read-only procedures recognised); `BEGIN`/`COMMIT`/`ROLLBACK` explicit transactions.
- [x] T18.3 Templates per language (search request, `MATCH … RETURN`, index/update/delete requests, `CREATE`/`SET`/`DETACH DELETE`), Neo4j URLs (`neo4j://`, `neo4j+s://`, `neo4j+ssc://`, `bolt://`), optional Elasticsearch user (API key in the password).
- [x] T18.4 Adapter `dq-elasticsearch` (reqwest): Elasticsearch and OpenSearch, cluster → indices / aliases / data streams, request results as grids (hits, `_cat`, `_sql`, text), read-only Documents view (Query DSL or query string filters, `_source` fields, sort, search_after paging), profile-based explain, `_tasks` monitoring and cancel, mapping-based structure, `_bulk` imports, point-in-time exports.
- [x] T18.5 Adapter `dq-neo4j`: databases → labels / relationship types / indexes and constraints, typed `$parameters`, nodes / relationships / paths as JSON cells, explicit transactions, EXPLAIN / PROFILE plan trees, read-only Documents view over labels and relationship types (Cypher WHERE / RETURN / ORDER BY fragments), `SHOW TRANSACTIONS` monitoring and `TERMINATE TRANSACTION`, imports as nodes.
- [x] T18.6 UI: Dev Tools editor mode (method, path, JSON highlighting, endpoint completion), Cypher highlighting, per-language Documents-view labels and examples, explorer groups for labels / relationship types / indexes, connection form labels.
- [x] T18.7 Test databases: `elasticsearch` (8.15, :59200, elastic / dataquery) and `neo4j` (5 community, Bolt :57687, HTTP :57474, neo4j / dataquery123); integration tests in `crates/dq-elasticsearch/tests` and `crates/dq-neo4j/tests`.

---

## 19. Connection form: `.pem` files

- [x] T19.1 SSH private keys from `.pem` files (`dq-ssh/key.rs`): AWS EC2 PKCS#1 RSA, PKCS#8 (plain and encrypted), SEC1 EC, OpenSSH; byte-order mark, Windows line ends and indentation tolerated; extra blocks ignored; a passphrase saved for an unencrypted key is ignored; PuTTY `.ppk`, public keys, certificates and old DES-encrypted keys get a message that says what to do. The key picker offers `.pem`/`.key` files.
- [x] T19.2 TLS client certificate and key in one `.pem`: `TlsFiles::client_pair` (key file optional) used by every adapter; the shared `dq_core::tls::client_identity_pem` replaces three copies in the HTTP adapters.
- [x] T19.3 `dq_core::tls::check_files` before connecting: CA bundle holds certificates, client certificate holds a certificate, a private key exists (in the key file or the combined `.pem`) and is not encrypted.
- [x] T19.4 Tests: key format fixtures (`dq-ssh/src/testdata`), combined-`.pem` fixture parsed like the rustls drivers, and SSH tunnels authenticated with RSA, EC and encrypted `.pem` keys against the bastion (its image now trusts the fixture keys).

---

## 20. Engine: Dragonfly

- [x] T20.1 `Engine::Dragonfly` in the catalog and `Engine::is_redis_like()` for the Redis-protocol engines (database numbers, optional user); `dragonfly://` / `dragonflys://` URLs.
- [x] T20.2 `RedisAdapter::DRAGONFLY`; `INFO server` labels the server (`dragonfly_version` → "Dragonfly 1.34.2"); Dragonfly's `ACL WHOAMI` wording handled; no `OBJECT` command → no encoding shown.
- [x] T20.3 Test database `dragonfly` (1.34.2, :56381, version check off); the Redis integration suite runs against Redis, Valkey and Dragonfly.
- [x] T20.4 Found while testing: PostgreSQL "largest tables" panicked when a table was dropped during the size query; it now skips such tables.

---

## 21. AI assistant: bring your own key

- [x] T21.1 Providers (`dq_store::AiProvider`): Claude (Anthropic), OpenAI, Google Gemini, DeepSeek, Mistral and Custom (any OpenAI-compatible endpoint); older "local model" settings load as Custom. DataQuery ships no AI service of its own.
- [x] T21.2 `src-tauri/ai_client.rs`: one provider table and three request formats (Anthropic Messages, OpenAI chat completions for OpenAI / DeepSeek / Mistral / Custom, Gemini `generateContent`); readable errors (bad key, unknown model, rate limit, declined) naming the provider.
- [x] T21.3 Keys per provider in the keychain (`ai:<provider>`; the Anthropic key keeps its id); never sent over plain HTTP to another machine; the disclosure names the provider and model.
- [x] T21.4 Models: defaults only where the name is stable (`claude-opus-5-5`, `deepseek-chat`, `mistral-large-latest`); "Load models" lists the chat models a key can use (and checks the key) without spending tokens.
- [x] T21.5 Settings → AI assistant: provider picker, per-provider key, endpoint for Custom, model with suggestions from Load models.
- [x] T21.6 Tests: request shape, headers and replies of all three formats against a mock HTTP server; settings migration; key safety; end to end against a local Ollama (`DATAQUERY_TEST_OLLAMA=http://localhost:11434/v1`).

---

## 22. CI and releases

- [x] T22.1 `.github/workflows/build.yml`: `npm run check` on Linux, then builds for macOS (Apple silicon, Intel), Linux and Windows on every push and pull request; installers kept as run artifacts.
- [x] T22.2 Releases: a push to `main` whose commit message contains "release" checks that `package.json`, `tauri.conf.json` and `Cargo.toml` agree and the tag is new, creates a draft `v<version>`, uploads every platform's installers (and signed updater files when `TAURI_SIGNING_PRIVATE_KEY` is set), and publishes only when all platforms built.
- [x] T22.3 Updater feed and repository URL point at `github.com/dev-manik-mia/DataQuery`; optional Apple signing/notarization secrets; workflow checked with actionlint.
- [x] T22.4 Homebrew cask (`brew install dev-manik-mia/dataquery/dataquery`) in the `homebrew-dataquery` tap, regenerated after each release by the `homebrew` job (`scripts/homebrew-cask.sh`); `brew style` and `brew audit --online` pass and the cask installs.
- [x] T22.5 Updates: automatic checks at launch and every 6 h (on by default) with an "Update" badge on Settings → About; optional automatic download and install that applies on restart (`auto_install_updates`).
- [x] T22.6 New connections default to SSL mode `disable` (Snowflake keeps `verify-full`); any mode can still be chosen.
- [x] T22.7 Visual filter builder in the table view: column · operator (=, ≠, >, ≥, <, ≤, contains, does not contain, starts with, ends with, in, is NULL, is not NULL) · value rows joined with AND or OR; values are parameters (`RowFilter.conditions`, `match_any`); applies to paging, row counts and exports. Unit tests (builder SQL, helpers) and integration tests on PostgreSQL and MySQL.

## 23. Engine: SQLite; remaining capabilities

- [x] T23.1 `SqlFlavor::Sqlite` + `SqliteDialect`: `"…"`/`` `…` ``/`[…]` identifiers, `?`, `?N`, `:name`, `@name`, `$name` parameters, `CREATE TRIGGER … BEGIN … END` kept as one statement, `BEGIN [DEFERRED|IMMEDIATE|EXCLUSIVE]`/`END`/savepoints; text parameters rely on column affinity; `LIKE … ESCAPE '\'` searches.
- [x] T23.2 SQLite DDL: rename table/column, add/drop column, `CREATE [UNIQUE] INDEX "db"."name" ON "table"`, unique constraints as unique indexes, identity only as a lone `INTEGER PRIMARY KEY`; altering columns, adding foreign keys or checks report that SQLite cannot (also as schema-diff warnings, which no longer abort the whole diff for any engine).
- [x] T23.3 Adapter `dq-sqlite` (rusqlite, bundled): files and shared-cache `:memory:`, read-only files (`SQLITE_OPEN_READ_ONLY` + `query_only`), foreign keys enforced, busy timeout; query thread with interrupt-based cancel/timeout and resumable results; explorer databases (`main`, attached; `ATTACH`/`DETACH` mirrored to the catalog and edit connections) → tables/views → indexes/triggers; structure from `pragma_table_xinfo`/`index_list`/`foreign_key_list` (generated columns, rowid identity, unique keys); DDL from `sqlite_master`; dependencies; schema model; `EXPLAIN QUERY PLAN` tree; `sqlite:///path` URLs.
- [x] T23.4 Schema diff for ClickHouse (create/drop tables, add/drop columns; round trip tested on ClickHouse 25.8).
- [x] T23.5 Elasticsearch Documents view edits: insert (`POST _doc`), replace (`PUT _doc/<id>`), delete, each `refresh=wait_for`; document keys carry the concrete index; read-only profiles refuse.
- [x] T23.6 Capability matrix: "—" with the reason for what the database itself does not offer; "Planned" only for Snowflake monitoring (needs a real account to verify) and Cassandra/ScyllaDB schema diff (composite partition keys and clustering order need CQL-specific create statements).

## 24. Release 0.2.1: engine logos, DuckDB monitoring, CQL schema diff

- [x] T24.1 Engine logos: `EngineMark` draws each engine's brand glyph on its brand color (`src/components/ui/engineIcons.ts`; Simple Icons CC0, Devicon MIT for SQL Server and DynamoDB, the Valkey and Dragonfly project marks, Oracle as its capsule "O"); the monogram stays as the fallback. The website's engine grid uses the same glyphs from `assets/engines.svg`.
- [x] T24.2 DuckDB monitoring: the embedded engine's one process is the query editor connection — its running statement and duration come from the query worker, and Cancel interrupts it (Terminate explains there is no connection to end); settings from `duckdb_settings()`; table sizes from `duckdb_tables()` row estimates and each table's persistent storage blocks.
- [x] T24.3 Cassandra/ScyllaDB schema diff: the diff carries each table's partition and clustering keys (`DiffTable`), so new tables get `PRIMARY KEY ((a, b), c) WITH CLUSTERING ORDER BY (…)` and static columns (`ddl::cql_create_table`, also used by the create-table form); columns and secondary indexes are added and dropped; a different primary key or column type is a warning (CQL cannot change them). User-defined types are not compared and must exist in the target keyspace. Round trip tested on Cassandra 5 and ScyllaDB.
- [x] T24.4 The fallback CQL DDL (servers without `DESCRIBE`) no longer puts `DESC` inside `PRIMARY KEY (…)`.

## 25. Release 0.2.2: performance

- [x] T25.1 Re-render fan-out: whole-store zustand subscriptions became `useShallow` selectors and `TabContent` is memoized, so a keystroke in one editor (or a toast, or a splitter drag) no longer re-renders every open tab and its grid.
- [x] T25.2 Result grid: rows share one stable handler object, so the row memo holds on scroll, selection and parent renders; client-side column search is deferred and precomputes its needles; the cell inspector formats JSON once per value.
- [x] T25.3 Table edits: paste builds its edits in one pass (`applyPaste`, was O(rows² × columns)); delete lookups use sets; row keys are computed once per page.
- [x] T25.4 ER diagram: edges and boxes are a memoized layer, so panning and zooming only move the transform; foreign-key and column lookups are built once per model.
- [x] T25.5 Monitor polling pauses while another page is open or the window is hidden. The SQL editor no longer copies the whole document a second time on every keystroke.
- [x] T25.6 PostgreSQL: catalog connections are opened outside the pool lock (a slow handshake no longer stalls every database), and exports stream on their own read-only connection without the catalog's 60 s statement timeout (long exports used to be cut off, and blocked the explorer while they ran).
- [x] T25.7 DuckDB and SQLite exports use their own connection (SQLite replays `ATTACH`es), so the explorer keeps working during an export; SQLite row estimates are a map lookup.
- [x] T25.8 ClickHouse result lines are split in one pass per network chunk (was quadratic).
- [x] T25.9 Query history: a run's statements are written in batched transactions (every 100 statements and at the end) instead of one commit each; `synchronous=NORMAL` under WAL; SQL is capped before redaction, and literal redaction runs off the async runtime.
- [x] T25.10 Schema diff reads table structures 8 at a time (was one by one) and no longer clones every structure; SSH tunnels set `TCP_NODELAY`.

## 26. Release 0.2.3: performance, part 2

- [x] T26.1 Table structure cache per session (`dq_core::schema_cache::StructureCache`, 30 s TTL as a safety net; concurrent misses share one read): grid pages, counts, cells, edits, exports and imports no longer re-read the catalog each time. Invalidated by the structure tab (always fresh), schema changes, any editor statement that may change structure, commit/rollback, explorer refresh and disconnect.
- [x] T26.2 Reusable write connection (`dq_core::adapter::IdleConnection`): grid saves, schema applies and import chunks reuse one connection per session and database instead of connecting each time (PostgreSQL/CockroachDB, SQL Server, Redis/Valkey/Dragonfly, Oracle; MySQL/MariaDB through a write pool). Returned only after a clean commit, dropped on any error or after 60 s idle. MySQL's catalog pool now keeps idle connections (it closed every one before).
- [x] T26.3 Result grid windows its columns above 40 visible columns (pinned, active and editing columns always rendered; header and body stay aligned); the explorer tree and key browser are virtualized; "Copy all" serializes large results in chunks and starts the clipboard write within the click (WebKit); completion sources are cached.
- [x] T26.4 Oracle: foreign keys in one set-based query (was 2N+1); fetch array size from the result's row width (up to 1,000 rows per round trip, defaults kept for LOB/LONG columns); pool rollback only when a transaction is open; schema list counts in one grouped query; search narrows names first, then joins.
- [x] T26.5 MongoDB Documents view continues after the last `_id` (default order is now `_id` order; other sorts stay offset-based with an `_id` tiebreaker, since MongoDB array and missing-field ordering makes field seeks inexact). Neo4j continues after the last sort key for sorted pages (follows Cypher's cross-type ordering); unsorted pages keep `SKIP`, which measured faster than an `elementId` seek.
- [x] T26.6 Neo4j editor keeps at most 100,000 rows while parsing (the rest is skipped without building values), caps plain read queries server-side, and says when rows were dropped; label counts and samples run 8 at a time. MongoDB search lists collections 16 databases at a time.

## 27. Release 0.2.4

- [x] T27.1 About page: removed the "Tested platforms" note (Windows and Linux builds have since been run on real machines) and the static release-notes list; About shows version, platform, licence and updates.

---

## 28. Free and Pro; TablePlus parity

- [x] T28.1 **Free and Pro plans**: Pro is an Ed25519-signed token verified offline (`src-tauri/src/license.rs`); a Lemon Squeezy licence key is exchanged once at the website's activation endpoint (`DataQueryWebsite/functions/api/license/[action].js`), or a `DQ1.…` token from `scripts/license-token.mjs` is pasted directly. Settings → License activates and deactivates (frees the seat). Official builds (`DATAQUERY_OFFICIAL_BUILD`, set in CI) limit Free to 4 tabs, 4 windows and 4 filter conditions; schema diff and data compare are Pro (ProGate page in the UI, refused in the backend). Builds from source have every feature.
- [x] T28.2 **Import connections from other apps**: TablePlus (`Connections.plist`, groups), DBeaver (`data-sources*.json`), Sequel Ace / Sequel Pro (`Favorites.plist`), DataGrip and other JetBrains IDEs (`dataSources.xml`, JDBC URLs, `sshConfigs.xml`); detected at their standard paths per OS or picked by hand; preview with duplicates marked; never imports secrets (`dq_store::import`).
- [x] T28.3 **Backup and restore**: `pg_dump`/`pg_restore`/`psql`, `mysqldump`/`mariadb-dump` + client, `mongodump`/`mongorestore`, SQLite online backup API; tool discovery with version matching and per-tool overrides in Settings; passwords via environment or a 0600 option file; SSH tunnels; progress and cancel; restore is destructive-confirmed and refused on read-only profiles (`dq_core::backup`).
- [x] T28.4 **Row details panel** (Space) in every grid: the active row as a form with types, editing into pending changes, NULL, inspect, FK links, column filter; ⇧Space opens the cell inspector.
- [x] T28.5 **Table actions**: Truncate (dialect options), Empty, Duplicate (structure or with rows) for every SQL engine that can; multi-select in the explorer (⌘/Ctrl-click, Shift ranges, keyboard) with bulk Drop / Truncate / Empty / Copy names; all through preview → confirm → run (`dq_core::sql::table_actions`).
- [x] T28.6 **Data compare**: merge-join of two key-ordered streams (bounded memory, same or different SQL engines), value normalisation across engines, filters, tolerance; counts, differing rows, sync SQL (INSERT / UPDATE / DELETE) to copy, save, open or apply in one transaction (`dq_core::data_compare`).
- [x] T28.7 **Charts from a result**: bar, line, pie (small slices folded into "Other"), scatter; aggregation per x value; validated categorical palette in both themes; tooltips and crosshair.
- [x] T28.8 **Vim mode** in the SQL editor (`@replit/codemirror-vim`, `:w` saves), Settings → Editor.
- [x] T28.9 **Users and roles** for PostgreSQL, CockroachDB, MySQL and MariaDB: accounts and attributes, privileges, create / alter / rename / drop, password changes (masked in previews and history; SCRAM hash on PostgreSQL), grant / revoke and role membership (`dq_core::users`).
- [x] T28.10 **Stable / Beta labels** in the engine picker; Snowflake and DynamoDB are Beta (tested only against fakesnow and DynamoDB Local).
- [x] T28.11 **Connection sync** through an encrypted file (`DataQuery.connections.dqsync`, Argon2id + AES-256-GCM) in a folder the user picks; last writer wins per item, tombstones, conflicted copies merged; no secrets in the file.
- [x] T28.12 **Scripts folder**: saved scripts mirrored two-way as plain `.sql` files (sub-folders = script folders, optional `-- dataquery:` header), watched for changes; unsaved tabs ask before reloading.
- [x] T28.13 README and website: why DataQuery, Free vs Pro, TablePlus Free vs DataQuery Free and TablePlus paid vs DataQuery Pro (from TablePlus's pricing page and docs index, 2026-10-20), licence setup.

Not verified: TablePlus on Windows (path and format undocumented), DBeaver TLS property variants, DataGrip `ssl-config` values; backup tools on Windows and with TLS files; data compare writes into ClickHouse, SQL Server and Oracle targets; users and roles on SQL Server (not built). The TablePlus importer was checked against a real macOS `Connections.plist` (9 connections, MySQL and PostgreSQL, SSH).

---

## Verification

| Area | How it was verified |
|---|---|
| Parameters, read-only profiles, row filters, cell fetch, explorer objects, sizes, FKs, dependencies, schema model, monitoring + kill, schema changes, schema diff round-trip, migrations, import/export (CSV/JSON/XLSX/SQL, encodings, validation, commit size, cancel) | Integration tests on PostgreSQL 17, MySQL 8.4 and MariaDB 11.4 (`crates/dq-postgres/tests`, `crates/dq-mysql/tests`) |
| SSH tunnels, jump hosts, host-key trust, wrong password | Integration tests against an OpenSSH bastion container (`crates/dq-ssh/tests/tunnel.rs`) |
| Transactions, explain parsing, DDL builders, diff planning, AI prompt/disclosure, safety rules | Unit tests (`dq-core`, `dataquery` crate) |
| Grid helpers (paste parsing, masking, hex dump), edits | Vitest |
| UI flows (parameters, explain plan, split panes, palette, monitoring, ER diagram, pin/search columns, structure forms, connection form, keyboard settings) | Exercised in the browser preview (`npm run dev`, mock backend); no console errors |
| Desktop build | `tauri build --bundles app` succeeds; the app starts |

Not exercised against a live external service (code paths are implemented and unit-tested where possible):
- **T2.15** CA / client-certificate files: no TLS-enabled test server in Docker.
- **T2.20** Cloud IAM: needs real AWS/Azure/GCP accounts and their CLIs.
- **T11** AI assistant: needs the user's Anthropic key or a local model; request shape follows the Messages API.
- **T12.2** Signed updates: a signing key pair was generated (`~/.tauri/dataquery.key`, public key in `tauri.conf.json`); the release feed `https://github.com/dev-manik-mia/DataQuery/releases/latest/download/latest.json` is published by the release workflow (§22) when `TAURI_SIGNING_PRIVATE_KEY` is a repository secret.
- **T1.9** Multiple windows and the updater plugin run only in the desktop app, not in the browser preview.

## Verification for every task

`npm run check` (tsc, eslint, vitest, `cargo fmt --check`, `clippy -D warnings`, `cargo test`) and
`npm run test:integration` against PostgreSQL 17, CockroachDB 25.2, MySQL 8.4, MariaDB 11.4, SQL Server 2022
and Oracle 23ai Free in Docker (Oracle through `npm run test:oracle`).
