//! Integration tests against a real Greenplum (or Apache Cloudberry) cluster,
//! through the PostgreSQL adapter.
//!
//! Start one with `docker compose -p dataquery up -d greenplum` (Greenplum 7,
//! a coordinator and two segments; the account is created on first start) and
//! run `DATAQUERY_TEST_GREENPLUM=localhost:55433 cargo test -p dq-postgres --test greenplum`.
//! Without the variable every test is skipped. The tests create the `mpp` schema.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::backup::{self, BackupFormat, BackupOptions, RestoreOptions, Tool, ToolOverrides};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::edit::{ColumnValue, RowChange};
use dq_core::engine::Engine;
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::ops;
use dq_core::result::ExecOptions;
use dq_core::sql::ddl::SchemaChange;
use dq_core::sql::{SqlFlavor, split_statements};
use dq_core::table::{RowFilter, SortSpec, TablePageRequest};
use dq_core::transfer::TransferControl;
use dq_core::value::Cell;
use dq_core::{DbResult, ErrorKind};
use dq_postgres::PostgresAdapter;
use tokio::sync::OnceCell;

const DB: &str = "dataquery_test";
const SCHEMA: &str = "mpp";
const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 60 };
const ADAPTER: PostgresAdapter = PostgresAdapter::GREENPLUM;

/// `gp_` schema names are reserved, so the fixtures live in `mpp`.
const SEED: &str = "DROP SCHEMA IF EXISTS mpp CASCADE;
CREATE SCHEMA mpp;
CREATE TABLE mpp.customers (id int PRIMARY KEY, name text NOT NULL) DISTRIBUTED BY (id);
CREATE TABLE mpp.orders (id bigint PRIMARY KEY, customer_id int REFERENCES mpp.customers (id),
  total numeric(12,2) NOT NULL, status text) DISTRIBUTED BY (id);
CREATE TABLE mpp.events (id bigint, at date, payload text)
  WITH (appendoptimized = true, orientation = column) DISTRIBUTED RANDOMLY;
CREATE TABLE mpp.regions (code text PRIMARY KEY, name text) DISTRIBUTED REPLICATED;
CREATE TABLE mpp.sales (id int, sold date, amount numeric) DISTRIBUTED BY (id)
  PARTITION BY RANGE (sold) (START (date '2026-01-01') INCLUSIVE END (date '2026-04-01') EXCLUSIVE EVERY (INTERVAL '1 month'));
CREATE VIEW mpp.paid AS SELECT * FROM mpp.orders WHERE status = 'paid';
INSERT INTO mpp.customers SELECT g, 'c' || g FROM generate_series(1, 10) g;
INSERT INTO mpp.orders SELECT g, 1 + g % 10, g * 1.5, CASE WHEN g % 2 = 0 THEN 'paid' ELSE 'open' END
  FROM generate_series(1, 1000) g;
INSERT INTO mpp.sales SELECT g, date '2026-01-01' + g % 90, g FROM generate_series(1, 300) g;
ANALYZE mpp.orders;";

fn config() -> Option<ConnConfig> {
    let target = std::env::var("DATAQUERY_TEST_GREENPLUM").ok()?;
    let (host, port) = target.split_once(':').unwrap_or((&target, "5432"));
    Some(ConnConfig {
        engine: Engine::Greenplum,
        host: host.into(),
        port: port.parse().unwrap(),
        database: DB.into(),
        username: "dataquery".into(),
        ssl_mode: SslMode::Disable,
        connect_timeout_secs: 5,
        ..ConnConfig::default()
    })
}

fn params(cfg: &ConnConfig) -> ConnectParams {
    ConnectParams::new(cfg.clone(), Some("dataquery".into()))
}

async fn run_all(s: &dyn Session, sql: &str) {
    for statement in split_statements(sql, SqlFlavor::Postgres) {
        let text = statement.text(sql);
        s.execute(text, &[], OPTS).await.unwrap_or_else(|e| panic!("{text}: {e:?}"));
    }
}

static SEEDED: OnceCell<()> = OnceCell::const_new();

/// A session on the seeded test database, or `None` (with a message) without a server.
async fn session() -> Option<Arc<dyn Session>> {
    let Some(cfg) = config() else {
        eprintln!("skipped: set DATAQUERY_TEST_GREENPLUM to run Greenplum integration tests");
        return None;
    };
    SEEDED
        .get_or_init(|| async {
            let s = ADAPTER.connect(&params(&cfg)).await.expect("connect");
            run_all(&*s, SEED).await;
            s.close().await;
        })
        .await;
    Some(ADAPTER.connect(&params(&cfg)).await.expect("connect"))
}

fn table(name: &str) -> ObjectRef {
    ObjectRef::new(NodeKind::Table, DB, Some(SCHEMA.into()), name)
}

fn text(cell: &Cell) -> String {
    cell.to_text().unwrap_or_else(|| "NULL".into())
}

#[tokio::test]
async fn reports_the_product_and_mpp_capabilities() {
    let Some(s) = session().await else { return };
    let info = s.info();
    assert!(info.version.starts_with("Greenplum ") || info.version.contains("Cloudberry"), "{}", info.version);
    assert_eq!((info.engine, info.database.as_str()), (Engine::Greenplum, DB));
    let caps = ADAPTER.capabilities();
    assert!(caps.backup && caps.schema_diff && caps.monitoring && caps.explain);
    assert!(s.users().is_some());
}

#[tokio::test]
async fn explorer_hides_partitions_and_system_schemas() {
    let Some(s) = session().await else { return };
    let db = ObjectRef::new(NodeKind::Database, DB, None, DB);
    let schemas = s.list_children(Some(&db)).await.unwrap();
    let names: Vec<_> = schemas.iter().map(|n| n.object.name.as_str()).collect();
    assert!(names.contains(&SCHEMA) && !names.contains(&"gp_toolkit"), "{names:?}");
    let mpp = schemas.iter().find(|n| n.object.name == SCHEMA).unwrap();
    let rels = s.list_children(Some(&mpp.object)).await.unwrap();
    let rel_names: Vec<_> = rels.iter().map(|n| n.object.name.as_str()).collect();
    assert!(rel_names.contains(&"sales") && !rel_names.iter().any(|n| n.starts_with("sales_1_prt")), "{rel_names:?}");
    assert!(rels.iter().any(|r| r.object.name == "paid" && r.object.kind == NodeKind::View));
    let orders = rels.iter().find(|r| r.object.name == "orders").unwrap();
    assert!(orders.row_estimate.is_some_and(|n| n > 0), "ANALYZE gathered rows on the segments");
    let children = s.list_children(Some(&orders.object)).await.unwrap();
    assert!(children.iter().any(|c| c.object.kind == NodeKind::Index && c.object.name == "orders_pkey"));
}

#[tokio::test]
async fn ddl_carries_distribution_storage_and_partitioning() {
    let Some(s) = session().await else { return };
    let orders = s.ddl(&table("orders")).await.unwrap();
    assert!(orders.contains("\n)\nDISTRIBUTED BY (id);"), "{orders}");
    let events = s.ddl(&table("events")).await.unwrap();
    // Greenplum 7 names the access method; Greenplum 6 keeps it in the storage options.
    let column_store = events.contains("USING ao_column") || events.contains("orientation=column");
    assert!(column_store && events.contains("DISTRIBUTED RANDOMLY"), "{events}");
    assert!(s.ddl(&table("regions")).await.unwrap().contains("DISTRIBUTED REPLICATED"));
    let sales = s.ddl(&table("sales")).await.unwrap();
    assert!(sales.contains("DISTRIBUTED BY (id)\nPARTITION BY RANGE"), "{sales}");
    let view = s.ddl(&ObjectRef { kind: NodeKind::View, ..table("paid") }).await.unwrap();
    assert!(view.starts_with("CREATE VIEW") && !view.contains("DISTRIBUTED"), "{view}");

    let st = s.structure(&table("orders")).await.unwrap();
    assert_eq!(st.row_identity, ["id"]);
    assert_eq!(st.foreign_keys.first().map(|f| f.target.name.as_str()), Some("customers"));
    assert!(st.size.is_some_and(|size| size.total_bytes > 0), "sizes are summed over the segments");
    let model = s.schema_model(DB, Some(SCHEMA)).await.unwrap();
    assert!(model.tables.iter().any(|t| t.object.name == "sales"));
    assert!(!model.tables.iter().any(|t| t.object.name.starts_with("sales_1_prt")));
}

#[tokio::test]
async fn pages_edits_and_table_actions() -> DbResult<()> {
    let Some(s) = session().await else { return Ok(()) };
    let req = TablePageRequest {
        object: table("orders"),
        rows: RowFilter { filter: Some("status = 'paid'".into()), ..RowFilter::default() },
        sort: Some(SortSpec { column: "total".into(), descending: true }),
        offset: 0,
        limit: 100,
    };
    let page = ops::table_page(&*s, &req).await?;
    assert_eq!((page.rows.len(), page.has_more, text(&page.rows[0][2])), (100, true, "1500.00".into()));
    assert_eq!(ops::row_count(&*s, &req.object, &req.rows).await?.count, 500);

    run_all(&*s, "DROP TABLE IF EXISTS mpp.it_edits; CREATE TABLE mpp.it_edits (id int PRIMARY KEY, note text) DISTRIBUTED BY (id); INSERT INTO mpp.it_edits VALUES (1, 'a'), (2, 'b')").await;
    let t = table("it_edits");
    let cv = |c: &str, v: Option<&str>| ColumnValue { column: c.into(), value: v.map(Into::into) };
    let changes = vec![
        RowChange::Update { key: vec![cv("id", Some("1"))], values: vec![cv("note", Some("changed"))] },
        RowChange::Insert { values: vec![cv("id", Some("3")), cv("note", None)] },
        RowChange::Delete { key: vec![cv("id", Some("2"))] },
    ];
    let report = ops::apply_changes(&*s, &t, &changes, false).await?;
    assert_eq!((report.updated, report.inserted, report.deleted), (1, 1, 1));

    let copy = SchemaChange::DuplicateTable { table: t.clone(), new_name: "it_edits_copy".into(), with_data: true };
    let truncate = SchemaChange::TruncateTable {
        table: table("it_edits_copy"),
        restart_identity: true,
        cascade: false,
        disable_foreign_keys: false,
    };
    for change in [copy, truncate] {
        let sql = ops::schema_statements(&*s, &[change]).await?;
        run_all(&*s, &sql.join(";\n")).await;
    }
    let copy_ddl = s.ddl(&table("it_edits_copy")).await?;
    assert!(copy_ddl.contains("DISTRIBUTED BY (id)"), "LIKE keeps the distribution: {copy_ddl}");
    let rows = s.execute("SELECT count(*) FROM mpp.it_edits_copy", &[], OPTS).await?;
    assert_eq!(text(&rows.rows[0][0]), "0");
    run_all(&*s, "DROP TABLE mpp.it_edits_copy; DROP TABLE mpp.it_edits").await;
    Ok(())
}

#[tokio::test]
async fn explain_reads_json_plans_with_motions() {
    let Some(s) = session().await else { return };
    let plan = s.explain("SELECT status, sum(total) FROM mpp.orders GROUP BY status", &[], false, 60).await.unwrap();
    let root = plan.plan.expect("plan tree");
    let mut labels = Vec::new();
    let mut stack = vec![&root];
    while let Some(n) = stack.pop() {
        labels.push(n.label.clone());
        stack.extend(n.children.iter());
    }
    assert!(labels.iter().any(|l| l.contains("Motion")), "{labels:?}");
    let analyzed = s.explain("SELECT count(*) FROM mpp.orders", &[], true, 60).await.unwrap();
    assert!(analyzed.execution_ms.is_some(), "{}", analyzed.raw);
}

#[tokio::test]
async fn monitor_reports_segments_and_cancels() {
    let Some(runner) = session().await else { return };
    let watcher = session().await.unwrap();
    let slow = tokio::spawn({
        let runner = runner.clone();
        async move { runner.execute("SELECT pg_sleep(20)", &[], OPTS).await }
    });
    let monitor = watcher.monitor().expect("greenplum monitors");
    let deadline = Instant::now() + Duration::from_secs(10);
    let (snapshot, pid) = loop {
        let snapshot = monitor.snapshot(1).await.unwrap();
        let pid = snapshot
            .processes
            .iter()
            .find(|p| p.query.as_deref().is_some_and(|q| q.contains("pg_sleep(20)")))
            .map(|p| p.pid);
        if pid.is_some() || Instant::now() > deadline {
            break (snapshot, pid);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let pid = pid.unwrap_or_else(|| panic!("{:#?}", snapshot.processes));
    let segments = snapshot.replication.as_ref().expect("segments");
    assert_eq!(segments.role, "coordinator");
    let state = segments.state.as_deref().unwrap_or_default();
    assert!(state.contains("primary segments up") && !state.contains("down:"), "{state}");
    assert!(snapshot.unavailable.is_empty(), "{:?}", snapshot.unavailable);
    assert!(monitor.kill(pid, false).await.unwrap());
    assert_eq!(slow.await.unwrap().unwrap_err().kind, ErrorKind::Cancelled);
    assert!(!monitor.server_settings(Some("gp_")).await.unwrap().is_empty());
    assert!(monitor.largest_tables(DB, 10).await.unwrap().iter().any(|t| t.object.name == "orders"));
}

#[tokio::test]
async fn users_and_roles_read_pg_roles() {
    let Some(s) = session().await else { return };
    let accounts = s.users().unwrap().accounts().await.unwrap();
    assert!(accounts.accounts.iter().any(|a| a.name == "dataquery" && a.current), "{:?}", accounts.accounts);
    assert!(accounts.accounts.iter().any(|a| a.name == "gpadmin"));
}

#[tokio::test]
async fn pg_dump_round_trips_a_small_schema() {
    let Some(s) = session().await else { return };
    for tool in [Tool::PgDump, Tool::PgRestore] {
        if backup::tools::locate(tool, Engine::Greenplum, None, None).await.found.is_none() {
            eprintln!("skipped: {} is not installed", tool.id());
            return;
        }
    }
    let p = params(&config().unwrap());
    let version = s.info().version.clone();
    let none = ToolOverrides::new();
    run_all(&*s, "DROP TABLE IF EXISTS public.gp_bk; CREATE TABLE public.gp_bk (id int PRIMARY KEY, note text) DISTRIBUTED BY (id); INSERT INTO public.gp_bk SELECT g, 'n' || g FROM generate_series(1, 2000) g").await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gp.dump");
    let options = BackupOptions {
        database: DB.into(),
        format: BackupFormat::Custom,
        tables: vec!["public.gp_bk".into()],
        no_owner: true,
        ..BackupOptions::default()
    };
    let report = backup::backup(&p, &version, None, &options, &path, &none, &TransferControl::none()).await.unwrap();
    assert_eq!(report.objects, 1);
    // pg_restore 17 and newer always sends `SET transaction_timeout`, which servers
    // built on PostgreSQL 16 or older (every Greenplum) reject.
    let server = backup::ServerVersion::parse(&version);
    let restore_tool = backup::tools::locate(Tool::PgRestore, Engine::Greenplum, server, None).await;
    if restore_tool.found.as_ref().is_some_and(|f| f.version.major >= 17) {
        eprintln!("restore skipped: only pg_restore 17+ is installed, which this server rejects");
        run_all(&*s, "DROP TABLE public.gp_bk").await;
        return;
    }
    run_all(&*s, "DROP TABLE public.gp_bk").await;
    let restore =
        RestoreOptions { database: DB.into(), no_owner: true, single_transaction: true, ..RestoreOptions::default() };
    backup::restore(&p, &version, None, &restore, &path, &none, &TransferControl::none()).await.unwrap();
    let rows = s.execute("SELECT count(*) FROM public.gp_bk", &[], OPTS).await.unwrap();
    assert_eq!(text(&rows.rows[0][0]), "2000");
    run_all(&*s, "DROP TABLE public.gp_bk").await;
}
