# DataQuery

A fast, safe desktop database client: Tauri 2, a Rust core and a React + TypeScript UI.
Supports **PostgreSQL**, **MySQL** and **MariaDB**. Passwords live in the OS keychain, and there is no telemetry, account or cloud service.

Feature status: [`feature_completed.md`](feature_completed.md) · Task list for §1–§12: [`tasks.md`](tasks.md) · Target scope: [`RUSTBASE_FEATURES.md`](RUSTBASE_FEATURES.md) · Designs: [`html/`](html)

## Getting started

Requirements: Rust 1.88+, Node 22+, and Docker (for the test databases).

```sh
npm install
npm run app          # run the desktop app (tauri dev)
npm run dev          # UI only in a browser, backed by sample data (src/dev/mockBackend.ts)
npm run app:build    # release bundle
npm run app:release  # release bundle plus signed updater artifacts (needs the signing key, below)
```

### Signed updates

The app checks `https://github.com/dataquery/dataquery/releases/latest/download/latest.json` only when asked
(Settings → About) or at launch if the user enables it. Updates are verified against the public key in
`src-tauri/tauri.conf.json` before they install.

The matching private key was generated locally at `~/.tauri/dataquery.key` (no password) and is **not** in the
repository. To publish a release, build with the key and upload the bundle, its `.sig` file and `latest.json`:

```sh
TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/dataquery.key)" npm run app:release
```

Keep the private key safe (e.g. as a CI secret); losing it means installed apps can no longer accept updates.

### Local test databases

```sh
npm run db:up        # PostgreSQL 17 :55432, MySQL 8.4 :53306, MariaDB 11.4 :53307, SSH bastion :52222
npm run db:down
```

All use user `dataquery`, password `dataquery` (databases: `dataquery_test`), seeded from [`docker/`](docker).
The SSH bastion reaches the databases by service name (`postgres:5432`, …) for the tunnel tests.

## Checks

```sh
npm run check             # tsc, eslint, vitest, cargo fmt --check, clippy -D warnings, cargo test
npm run test:integration  # Rust tests against the Docker databases
npm run bindings          # regenerate src/bindings/*.ts from the Rust types
```

## Architecture

```
crates/
  dq-core       Engine-neutral types and logic: the DatabaseAdapter/Session/Monitor traits, value
                contract, SQL lexer/analyzer/formatter, dialect quoting, statement and DDL builders,
                schema diff, explain parsing, framework migrations, and paging/edit/export/import
                written once in `ops`.
  dq-postgres   PostgreSQL adapter (tokio-postgres + rustls).
  dq-mysql      MySQL and MariaDB adapter (mysql_async + rustls).
  dq-store      Local SQLite store (connections, settings, history, scripts, snippets) and keychain.
  dq-ssh        SSH tunnels (russh): jump hosts, password/key/agent auth, host-key verification.
src-tauri/      Tauri app: thin async commands, adapter registry, connector (secrets, IAM tokens,
                tunnels, retries), safety confirmations, opt-in AI assistant.
src/            React UI: components/ (design system), features/ (screens), stores/ (zustand),
                lib/ (typed IPC, formatting), i18n/ (string table), bindings/ (generated from Rust,
                do not edit). Commands, shortcuts and the palette share features/commands.
```

**Adding an engine:** implement `DatabaseAdapter` and `Session` from `dq-core` (catalog queries plus a few execution primitives) and register the adapter in `src-tauri/src/registry.rs`. Paging, grid edits, export and import come from `dq_core::ops`.

### Safety rules the code enforces

- Values reach SQL only as bound parameters (also user `$n`/`?` parameters, FK navigation and column search). Identifiers are always quoted by the engine's `Dialect`; user-typed types, defaults and checks must be single expressions.
- Read-only connections are read-only on the server session, and the backend refuses grid edits, imports and schema changes.
- Secrets (passwords, SSH passwords/passphrases, the AI key) live only in the keychain; cloud IAM tokens are fetched per connect and never stored.
- Results are always bounded. Exports stream to disk.
- Table views and exports run on **read-only** connections. Grid edits run in one transaction, and each change must affect exactly one row or everything rolls back.
- Destructive statements and production writes need a confirmation, checked in the backend and not only in the UI.
- Error messages are redacted before they reach the UI, logs or history.

## License

MIT OR Apache-2.0
