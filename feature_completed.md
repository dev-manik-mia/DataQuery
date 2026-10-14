# DataQuery — Feature Status

Status of every feature in [`RUSTBASE_FEATURES.md`](RUSTBASE_FEATURES.md) (the target scope).
**Engines done:** PostgreSQL 12–17 ✅ · MySQL 5.7–8.4 ✅ · MariaDB 10.6–11 ✅ (shares the MySQL adapter).

Legend: ✅ Done · 🟡 Partial · ⬜ Not started · Pri = priority from the feature list.
"Done" means it works end-to-end (UI → Tauri command → adapter) and, where it touches a database, is covered by integration tests against real servers.

_Last updated: 2026-10-14._

---

## Summary

| Milestone | Scope | Status |
|---|---|---|
| **M1** Foundation + PostgreSQL (MVP) | shell, connections, keychain, explorer, editor, run/cancel, bounded grid, settings, tests | ✅ Done |
| **M2** Data management (P0) | table data view, safe inline edits, export/import, history, scripts, structure + DDL | ✅ Done, except signed updates ⬜ and two 🟡 rows (CSV import error report, schema-change preview) |
| **M3** Beta (P0) | MySQL ✅, MariaDB ✅, SQLite ⬜, production safeguards ✅, URL import ✅, signed updates ⬜, Docker integration tests ✅, packaging ⬜ | 🟡 In progress |
| **M4 / M5** | P1 / P2 engines and features | ⬜ Not started (a few P1 items already done, listed below) |

### Tests (all passing)

| Suite | Count | Command |
|---|---|---|
| Rust unit tests (core, store, app, adapters) | 122 | `cargo test --workspace` |
| PostgreSQL 17 integration | 16 | `npm run db:up` then `npm run test:integration` |
| MySQL 8.4 integration | 16 | 〃 (port 53306) |
| MariaDB 11.4 integration | 16 | 〃 (port 53307, same suite) |
| Frontend unit tests (Vitest) | 14 | `npm test` |
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
| Command palette | P1 | ⬜ | |
| Multiple windows, split panes | P1 | ⬜ | |
| Custom keymaps, editor font settings | P1 | 🟡 | Font size only |
| Localization-ready string table | P1 | ⬜ | |

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
| Advanced TLS (CA / client cert files) | P1 | ⬜ | MySQL uses Mozilla roots; private CAs need this |
| SSH tunnels | P1 | ⬜ | |
| Read-only profiles | P1 | ⬜ | |
| Keepalive / retry / pool settings | P1 | ⬜ | Sensible fixed defaults today |
| Connection export/import | P1 | ⬜ | |
| Cloud IAM auth | P2 | ⬜ | |

## 3. Database explorer

| Feature | Pri | Status | Notes |
|---|---|---|---|
| Tree with T / V / M markers | MVP | ✅ | Postgres: db → schema → relations. MySQL: db → tables/views |
| Lazy-load children, skeleton rows | MVP | ✅ | |
| Filter objects by name | MVP | ✅ | |
| Refresh a single node | MVP | ✅ | |
| Context menu: Open data, Copy qualified name, Generate SELECT | MVP | ✅ | Dialect quoting (`"x"` / `` `x` ``) |
| Row-count estimate badges | P0 | ✅ | `pg_class.reltuples` / `information_schema.TABLES.TABLE_ROWS` |
| Indexes, sequences, triggers, functions in tree | P1 | 🟡 | Indexes in Structure tab only |
| Table and index sizes | P1 | ⬜ | |
| Generate INSERT / UPDATE / DELETE templates | P1 | ⬜ | |
| Dependency browsing | P1 | ⬜ | |

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
| Autocomplete (schema-aware) | P1 | 🟡 | Keywords only |
| Dialect-aware formatter | P1 | 🟡 | Two flavours |
| Transaction controls, Explain, parameters/snippets | P1 | ⬜ | |

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
| Expanded viewer / JSON editor / binary viewer | P1 | 🟡 | Viewer only |
| Hide/pin columns, FK navigation, paste, undo | P1 | ⬜ | |

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
| Preview SQL for schema changes | P0 | 🟡 | No schema-change forms yet (they are P1), so nothing to preview |
| Create/alter/drop forms, index management, ER diagram, schema diff | P1 | ⬜ | |

## 7. Query history and saved scripts

| Feature | Pri | Status |
|---|---|---|
| Local history (SQL, connection, time, duration, status) | MVP | ✅ |
| Never stores result rows | MVP | ✅ |
| Search history | P0 | ✅ |
| Detail panel: Open in tab (no auto-run), Save as script, Copy | P0 | ✅ |
| Saved scripts list | P0 | ✅ |
| Filters by connection/date/status | P1 | 🟡 backend only |
| Folders, tags, pinning; retention settings | P1 | ⬜ / 🟡 |

## 8. Import / export

| Feature | Pri | Status | Notes |
|---|---|---|---|
| Export results / table to CSV & JSON, streamed | P0 | ✅ | |
| Export selected / filtered rows | P0 | ✅ | Selection: grid menu "Export N rows…"; refuses if any value was shortened for display |
| CSV import: mapping, preview, validation, error report | P0 | 🟡 | Error reports the failing batch's first row + server message, not every bad row |
| SQL INSERT / XLSX export, JSON import, encoding options, progress | P1 | ⬜ | |

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
| Sensitive-column masking, insecure-connection warnings | P1 | ⬜ / 🟡 |

## 10–11. Monitoring, AI assistant (P1/P2)

⬜ Not started.

## 12. About and updates

| Feature | Pri | Status | Notes |
|---|---|---|---|
| Version, platform, licence | P0 | ✅ | |
| Release notes | P0 | ✅ | |
| Honest tested-platforms statement | P0 | ✅ | |
| Signed update check / download / install | P0 | ⬜ | Needs a signing key pair and a release feed URL from the project owner |

## 13. Engines

| Engine | Pri | Status |
|---|---|---|
| PostgreSQL 12–17 | MVP | ✅ |
| MySQL 5.7–8.4 | P0 | ✅ (tested on 8.4) |
| MariaDB 10.6–11 | P0 | ✅ (tested on 11.4) |
| SQLite | P0 | ⬜ next |
| Everything else | P1/P2 | ⬜ |

---

## Known limitations

- MySQL statement timeouts apply to `SELECT` only (server behaviour); MariaDB applies them to every statement.
- MariaDB has no real JSON type, so JSON columns show and export as text.
- MySQL TLS verification uses the Mozilla root store; servers signed by a private CA need the P1 "CA file" option.
- Tested on macOS only; Windows and Linux compile from the same code but have not been run.
