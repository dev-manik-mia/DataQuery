# DataQuery

A fast, safe desktop database client: Tauri 2, a Rust core and a React + TypeScript UI.
Supports **PostgreSQL**, **CockroachDB**, **MySQL**, **MariaDB**, **SQL Server**, **Oracle**, **ClickHouse**, **DuckDB**, **Snowflake**, **MongoDB**, **DynamoDB**, **Redis**, **Valkey**, **Dragonfly**, **Cassandra**, **ScyllaDB**, **Elasticsearch / OpenSearch** and **Neo4j**. Passwords live in the OS keychain, and there is no telemetry, account or cloud service.

Feature status: [`feature_completed.md`](feature_completed.md) · Task list: [`tasks.md`](tasks.md) · Target scope: [`RUSTBASE_FEATURES.md`](RUSTBASE_FEATURES.md) · Designs: [`html/`](html)

## Getting started

Requirements: Rust 1.99.0 (pinned in `rust-toolchain.toml`; rustup installs it on the first build, and CI uses the same version), Node 22+, and Docker (for the test databases).

```sh
npm install
npm run app          # run the desktop app (tauri dev)
npm run dev          # UI only in a browser, backed by sample data (src/dev/mockBackend.ts)
npm run app:build    # release bundle
npm run app:release  # release bundle plus signed updater artifacts (needs the signing key, below)
```

### Signed updates

The app checks `https://github.com/dev-manik-mia/DataQuery/releases/latest/download/latest.json` at launch and every
6 hours (Settings → About → "Check for updates automatically", on by default) and shows "Update available" there and in
a toast. With "Download and install updates automatically" on, the update downloads and installs in the background
and applies on the next restart (on Windows the installer runs when the user picks Restart). Updates are verified
against the public key in `src-tauri/tauri.conf.json` before they install.

### Homebrew

```sh
brew install dev-manik-mia/dataquery/dataquery
```

The cask lives in [`dev-manik-mia/homebrew-dataquery`](https://github.com/dev-manik-mia/homebrew-dataquery). After each
release the `homebrew` job in `build.yml` regenerates `Casks/dataquery.rb` with `scripts/homebrew-cask.sh <version>`
(DMG checksums come from the GitHub release). It needs the `HOMEBREW_TAP_TOKEN` secret: a fine-grained token with
Contents read/write on the tap repository. The cask sets `auto_updates true`, so `brew upgrade` leaves the app to its
own updater (`brew upgrade --greedy` still works).

The matching private key was generated locally at `~/.tauri/dataquery.key` (no password) and is **not** in the
repository. To publish a release, build with the key and upload the bundle, its `.sig` file and `latest.json`:

```sh
TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/dataquery.key)" npm run app:release
```

### CI and releases (GitHub Actions)

`.github/workflows/build.yml` runs only when a push to `main` has **release** in its commit message (any case):
it runs `npm run check`, builds the app for macOS (Apple silicon and Intel), Linux (`.deb`, `.rpm`, AppImage) and
Windows (`.msi`, `.exe`), and publishes the GitHub release `v<version>` with every platform's installers — only when
all platforms built. Other pushes start no jobs. **Actions → Build → Run workflow** builds every platform without
publishing (installers kept with the run for 14 days). Before releasing, bump the
same version in `package.json`, `src-tauri/tauri.conf.json` and `Cargo.toml`; the workflow refuses mismatched
versions and versions that were already released.

Repository secrets (Settings → Secrets and variables → Actions):

| Secret | Needed for |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` (+ `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` if it has one) | Signed updater files (`latest.json`); without it releases still build but installed apps do not see them as updates |
| `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID` | Optional macOS code signing and notarization (unsigned builds otherwise) |

```sh
gh secret set TAURI_SIGNING_PRIVATE_KEY < ~/.tauri/dataquery.key
```

Keep the private key safe (e.g. as a CI secret); losing it means installed apps can no longer accept updates.

### Local test databases

```sh
npm run db:up        # PostgreSQL 17 :55432, MySQL 8.4 :53306, MariaDB 11.4 :53307, SSH bastion :52222,
                     # SQL Server 2022 :51433, Oracle 23ai Free :51521 (service FREEPDB1), CockroachDB :56257,
                     # ClickHouse 25.8 HTTP :58123, fakesnow (Snowflake emulator) :58000,
                     # MongoDB 8.0 replica set :57017, DynamoDB Local :58002,
                     # Redis 8 :56379, Valkey 8 :56380, Dragonfly :56381 (password dataquery),
npm run db:up:wide   # Cassandra 5.0 :59042 (cassandra/cassandra), ScyllaDB 2025.1 :59043
npm run db:up:search # Elasticsearch 8.15 :59200 (elastic/dataquery), Neo4j 5 HTTP :57474 (neo4j/dataquery123)
npm run db:down
```

All use user `dataquery`, password `dataquery` (databases: `dataquery_test`; Oracle: service `FREEPDB1`),
seeded from [`docker/`](docker). SQL Server runs under x86-64 emulation on Apple silicon; the three newer
containers need about 5 GB of Docker memory together.

### Oracle Instant Client

Oracle connections use Oracle's client library, loaded when the first Oracle connection opens. Install
[Oracle Instant Client](https://www.oracle.com/database/technologies/instant-client/downloads.html) (Basic or
Basic Light). DataQuery looks in `DATAQUERY_ORACLE_CLIENT`, then `~/Downloads/instantclient_*`, `~/lib`,
`/opt/oracle/instantclient_*` and `/usr/local/lib`, then the system library path. Other engines do not need it.
`npm run test:oracle` runs the Oracle tests in a Linux container with the client from the test database image,
so they need no local install.
### DuckDB and Snowflake connections

DuckDB connections are a database file (pick one or create it with "New…") or `:memory:`; there is no host,
password, TLS or SSH. Snowflake connections take the account identifier (`myorg-myaccount`) or full host name,
plus optional Warehouse, Role and Schema; a URL such as
`snowflake://user@myorg-myaccount/DB/PUBLIC?warehouse=WH&role=ANALYST` fills them in. The Snowflake tests run
against [fakesnow](https://github.com/tekumara/fakesnow), not a real account.

### MongoDB and DynamoDB connections

MongoDB connections take one host or `host1:27017,host2:27017`, an optional user (with auth database), replica
set, read preference and a DNS seed list switch; `mongodb://` and `mongodb+srv://` URLs fill them in. The editor runs
mongosh-style statements (`db.orders.find({ status: "paid" }).sort({ createdAt: -1 })`, `show collections`,
`use app`, `BEGIN`/`COMMIT`); they are parsed, never executed as JavaScript. DynamoDB connections take a region,
an optional endpoint (empty for AWS, `localhost` + port for DynamoDB Local) and either access keys (the secret is
kept in the keychain) or an AWS CLI profile, whose credentials come from `aws configure export-credentials` on each
connect. Collections and tables open in the Documents view; edits use the engine's own text form (shell syntax or
DynamoDB JSON) so every type stays exact.

### Redis, Valkey and Dragonfly connections

Redis, Valkey and Dragonfly connections take a host, an optional ACL user, a password and a database number; `redis://`,
`rediss://`, `valkey://`, `valkeys://` and `dragonfly://` URLs fill them in. The explorer becomes a key browser that walks keys with
`SCAN` (never `KEYS`); keys open in tabs with an editor per type and a `redis-cli`-style console, where
`FLUSHALL`, `FLUSHDB`, `DEBUG`, `KEYS` and other dangerous commands ask for confirmation first.

### Cassandra and ScyllaDB connections

Cassandra and ScyllaDB connections take one or more contact points (`node1,node2:9043`), an optional user, a
keyspace and an optional local datacenter. The editor shows consistency and page-size selectors and a banner that
says whether the statement reads one partition or scans (`ALLOW FILTERING`); result headers mark partition (`PK`)
and clustering (`CK ↑/↓`) columns. All test databases together need more than 8 GB of Docker memory, so Cassandra /
ScyllaDB and Elasticsearch / Neo4j are compose profiles (`npm run db:up:wide`, `npm run db:up:search`) with their own
test scripts (`test:integration:wide`, `test:integration:search`); stop SQL Server and Oracle while they run.

### Elasticsearch and Neo4j connections

Elasticsearch connections take a host and port (9200), a user and password, or an API key in the password field with
no user. The editor takes Kibana Dev Tools requests (`GET /orders/_search` followed by a JSON body); indices open in
a read-only Documents view. Neo4j connections use the HTTP Query API (port 7474, Neo4j 5.19 or later; `neo4j://` and
`bolt://` URLs are mapped to it): Cypher with `$parameters`, EXPLAIN/PROFILE plans and `BEGIN`/`COMMIT`; labels and
relationship types open in a read-only Documents view.

The SSH bastion reaches the databases by service name (`postgres:5432`, …) for the tunnel tests.

## Checks

```sh
npm run check             # tsc, eslint, vitest, cargo fmt --check, clippy -D warnings, cargo test
npm run test:integration  # Rust tests against the Docker databases (includes test:oracle)
npm run test:integration:wide    # Cassandra and ScyllaDB (after db:up:wide)
npm run test:integration:search  # Elasticsearch and Neo4j (after db:up:search)
npm run test:oracle       # Oracle tests only, in a Linux container
npm run bindings          # regenerate src/bindings/*.ts from the Rust types
```

## Architecture

```
crates/
  dq-core       Engine-neutral types and logic: the DatabaseAdapter/Session/Monitor traits, value
                contract, SQL lexer/analyzer/formatter, dialect quoting, statement and DDL builders,
                schema diff, explain parsing, framework migrations, and paging/edit/export/import
                written once in `ops`.
  dq-postgres   PostgreSQL and CockroachDB adapter (tokio-postgres + rustls).
  dq-mysql      MySQL and MariaDB adapter (mysql_async + rustls).
  dq-sqlserver  SQL Server adapter (tiberius + rustls): TDS, attention-based cancel, showplan XML.
  dq-oracle     Oracle adapter (rust-oracle / ODPI-C + Oracle Instant Client).
  dq-clickhouse ClickHouse adapter (HTTP interface via reqwest, server sessions, KILL QUERY cancel).
  dq-duckdb     DuckDB adapter (bundled engine, local files, interrupt-based cancel).
  dq-snowflake  Snowflake adapter (own REST client, Arrow results, abort-based cancel).
  dq-mongodb    MongoDB adapter (official driver): mongosh-style statements, documents, killOp cancel.
  dq-dynamodb   DynamoDB adapter (own SigV4 client): PartiQL, documents, conditional writes.
  dq-redis      Redis, Valkey and Dragonfly adapter (redis-rs): SCAN key browser, typed editors, redis-cli console.
  dq-cassandra  Cassandra and ScyllaDB adapter (scylla driver): CQL, prepared binding, LWT edits, traces.
  dq-elasticsearch  Elasticsearch / OpenSearch adapter (reqwest): Dev Tools requests, documents, profiles.
  dq-neo4j      Neo4j adapter (HTTP Query API): Cypher, typed parameters, plans, transactions.
  dq-store      Local SQLite store (connections, settings, history, scripts, snippets) and keychain.
  dq-ssh        SSH tunnels (russh): jump hosts, password/key/agent auth, host-key verification.
src-tauri/      Tauri app: thin async commands, adapter registry, connector (secrets, IAM tokens,
                tunnels, retries), safety confirmations, opt-in AI assistant (bring your own key: Claude,
                OpenAI, Gemini, DeepSeek, Mistral or any OpenAI-compatible endpoint).
src/            React UI: components/ (design system), features/ (screens), stores/ (zustand),
                lib/ (typed IPC, formatting), i18n/ (string table), bindings/ (generated from Rust,
                do not edit). Commands, shortcuts and the palette share features/commands.
```

**Adding an engine:** implement `DatabaseAdapter` and `Session` from `dq-core` (catalog queries plus a few execution primitives) and register the adapter in `src-tauri/src/registry.rs`. Paging, grid edits, export and import come from `dq_core::ops`.

### Safety rules the code enforces

- Values reach SQL only as bound parameters (also user `$n`/`?`/`@Pn`/`:n` parameters, FK navigation and column search). Identifiers are always quoted by the engine's `Dialect`; user-typed types, defaults and checks must be single expressions.
- Read-only connections are read-only on the server session (PostgreSQL, CockroachDB, MySQL, ClickHouse `readonly=2`, DuckDB read-only file access), and the backend refuses grid edits, imports and schema changes. SQL Server, Oracle, Snowflake, MongoDB, DynamoDB, Redis, Valkey, Cassandra and ScyllaDB have no such session mode (Elasticsearch, too; Neo4j runs read-only sessions in the server's read mode), so DataQuery also refuses writing statements there.
- Secrets (passwords, SSH passwords/passphrases, AI provider keys) live only in the keychain; cloud IAM tokens are fetched per connect and never stored.
- Results are always bounded. Exports stream to disk.
- Table views and exports run on **read-only** connections. Grid edits run in one transaction, and each change must affect exactly one row or everything rolls back.
- Destructive statements and production writes need a confirmation, checked in the backend and not only in the UI.
- Error messages are redacted before they reach the UI, logs or history.

## License

MIT OR Apache-2.0
