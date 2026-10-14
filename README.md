# DataQuery

A fast, safe desktop database client: Tauri 2, a Rust core and a React + TypeScript UI.
Supports **PostgreSQL**, **MySQL** and **MariaDB**. Passwords live in the OS keychain, and there is no telemetry, account or cloud service.

Feature status: [`feature_completed.md`](feature_completed.md) · Target scope: [`RUSTBASE_FEATURES.md`](RUSTBASE_FEATURES.md) · Designs: [`html/`](html)

## Getting started

Requirements: Rust 1.88+, Node 22+, and Docker (for the test databases).

```sh
npm install
npm run app          # run the desktop app (tauri dev)
npm run dev          # UI only in a browser, backed by sample data (src/dev/mockBackend.ts)
npm run app:build    # release bundle
```

### Local test databases

```sh
npm run db:up        # PostgreSQL 17 :55432, MySQL 8.4 :53306, MariaDB 11.4 :53307
npm run db:down
```

All three use user `dataquery`, password `dataquery` and database `dataquery_test`, seeded from [`docker/`](docker).

## Checks

```sh
npm run check             # tsc, eslint, vitest, cargo fmt --check, clippy -D warnings, cargo test
npm run test:integration  # Rust tests against the Docker databases
npm run bindings          # regenerate src/bindings/*.ts from the Rust types
```

## Architecture

```
crates/
  dq-core       Engine-neutral types and logic: the DatabaseAdapter/Session traits, value
                contract, SQL lexer/analyzer/formatter, dialect quoting, statement builders,
                paging/edit/export/import written once in `ops`.
  dq-postgres   PostgreSQL adapter (tokio-postgres + rustls).
  dq-mysql      MySQL and MariaDB adapter (mysql_async + rustls).
  dq-store      Local SQLite store (connections, settings, history, scripts) and keychain.
src-tauri/      Tauri app: thin async commands, adapter registry, safety confirmations.
src/            React UI: components/ (design system), features/ (screens), stores/ (zustand),
                lib/ (typed IPC, formatting), bindings/ (generated from Rust, do not edit).
```

**Adding an engine:** implement `DatabaseAdapter` and `Session` from `dq-core` (catalog queries plus a few execution primitives) and register the adapter in `src-tauri/src/registry.rs`. Paging, grid edits, export and import come from `dq_core::ops`.

### Safety rules the code enforces

- Values reach SQL only as bound parameters. Identifiers are always quoted by the engine's `Dialect`.
- Results are always bounded. Exports stream to disk.
- Table views and exports run on **read-only** connections. Grid edits run in one transaction, and each change must affect exactly one row or everything rolls back.
- Destructive statements and production writes need a confirmation, checked in the backend and not only in the UI.
- Error messages are redacted before they reach the UI, logs or history.

## License

MIT OR Apache-2.0
