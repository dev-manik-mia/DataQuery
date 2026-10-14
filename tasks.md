# DataQuery — Tasks for §1–§12 of `RUSTBASE_FEATURES.md`

Every feature row in sections 1–12 of [`RUSTBASE_FEATURES.md`](RUSTBASE_FEATURES.md), as a task.
`[x]` = done and verified (`npm run check` + `npm run test:integration`), `[ ]` = open.
Rows already shipped before this list was written are marked done with *(existing)*.

**Status (2026-10-14): every task in §1–§12 is done.** Notes under "Verification" say which ones could not be exercised against a live external service.
Engines (§13) and milestones (§14) are out of scope here; they are tracked in [`feature_completed.md`](feature_completed.md).

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
- **T12.2** Signed updates: a signing key pair was generated (`~/.tauri/dataquery.key`, public key in `tauri.conf.json`); the release feed `https://github.com/dataquery/dataquery/releases/latest/download/latest.json` must be published by a release built with `npm run app:release` and `TAURI_SIGNING_PRIVATE_KEY` set.
- **T1.9** Multiple windows and the updater plugin run only in the desktop app, not in the browser preview.

## Verification for every task

`npm run check` (tsc, eslint, vitest, `cargo fmt --check`, `clippy -D warnings`, `cargo test`) and
`npm run test:integration` against PostgreSQL 17, MySQL 8.4 and MariaDB 11.4 in Docker.
