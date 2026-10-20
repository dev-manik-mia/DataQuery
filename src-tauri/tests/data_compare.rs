//! Data compare against real databases: PostgreSQL, MySQL (also across the
//! two) and SQLite files. Start the servers with
//! `docker compose -p dataquery up -d postgres mysql` and run
//! `DATAQUERY_TEST_PG=localhost:55432 DATAQUERY_TEST_MYSQL=localhost:53306 cargo test -p dataquery --test data_compare`.
//! Server tests are skipped without the variables; the SQLite test always runs.

use std::sync::Arc;

use dq_core::DbResult;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::data_compare::{
    self, Changes, CompareCounts, CompareSide, Compared, DataCompareOptions, SyncChange, SyncKinds,
};
use dq_core::edit::ApplyReport;
use dq_core::engine::Engine;
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::result::ExecOptions;
use dq_core::table::{FilterCondition, FilterOp, RowFilter};
use dq_core::transfer::TransferControl;

const OPTS: ExecOptions = ExecOptions { row_limit: 100, timeout_secs: 60 };
const DB: &str = "dataquery_test";

fn server(var: &str, engine: Engine, default_port: &str) -> Option<ConnConfig> {
    let target = std::env::var(var).ok()?;
    let (host, port) = target.split_once(':').unwrap_or((&target, default_port));
    Some(ConnConfig {
        engine,
        host: host.into(),
        port: port.parse().unwrap(),
        database: DB.into(),
        username: "dataquery".into(),
        ssl_mode: SslMode::Disable,
        connect_timeout_secs: 5,
        ..ConnConfig::default()
    })
}

async fn postgres() -> Option<Arc<dyn Session>> {
    let cfg = server("DATAQUERY_TEST_PG", Engine::Postgres, "5432")?;
    Some(
        dq_postgres::PostgresAdapter::POSTGRES
            .connect(&ConnectParams::new(cfg, Some("dataquery".into())))
            .await
            .expect("connect"),
    )
}

async fn mysql() -> Option<Arc<dyn Session>> {
    let cfg = server("DATAQUERY_TEST_MYSQL", Engine::Mysql, "3306")?;
    Some(
        dq_mysql::MysqlAdapter::MYSQL
            .connect(&ConnectParams::new(cfg, Some("dataquery".into())))
            .await
            .expect("connect"),
    )
}

async fn run(s: &dyn Session, statements: &[&str]) {
    for sql in statements {
        s.execute(sql, &[], OPTS).await.unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
}

fn side(session: &Arc<dyn Session>, database: &str, schema: Option<&str>, table: &str) -> CompareSide {
    CompareSide {
        session: session.clone(),
        object: ObjectRef::new(NodeKind::Table, database, schema.map(Into::into), table),
    }
}

async fn compare(
    source: &CompareSide,
    target: &CompareSide,
    options: &DataCompareOptions,
) -> DbResult<(Compared, Vec<SyncChange>)> {
    let mut changes = Vec::new();
    let compared = data_compare::compare(source, target, options, &mut changes, &TransferControl::none()).await?;
    Ok((compared, changes))
}

/// Applies the changes to the target, the way the app does from its journal.
async fn sync(target: &CompareSide, compared: &Compared, changes: Vec<SyncChange>) -> ApplyReport {
    let open = move || Ok(Box::new(changes.clone().into_iter().map(Ok)) as Changes);
    let statements = compared.target.statements(open, SyncKinds::ALL, false);
    data_compare::apply(&*target.session, &target.object.database, statements, &TransferControl::none())
        .await
        .expect("apply")
}

fn counts(identical: u64, only_in_source: u64, only_in_target: u64, different: u64) -> CompareCounts {
    CompareCounts { identical, only_in_source, only_in_target, different }
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_to_mysql_then_sync_to_zero_differences() {
    let (Some(pg), Some(my)) = (postgres().await, mysql().await) else {
        eprintln!("skipped: set DATAQUERY_TEST_PG and DATAQUERY_TEST_MYSQL");
        return;
    };
    run(
        &*pg,
        &[
            "DROP TABLE IF EXISTS dc_xe_items",
            "CREATE TABLE dc_xe_items (id bigint PRIMARY KEY, name text, price numeric(10,2), active boolean, created timestamptz, note varchar(40))",
            "INSERT INTO dc_xe_items VALUES
               (1, 'Alpha', 1.50, true, '2024-05-01 10:00:00+02', 'x'),
               (2, 'beta', 2.00, false, '2024-05-02 00:00:00+00', NULL),
               (3, 'Gamma', 3.25, true, '2024-05-03 12:30:00.5+00', 'same'),
               (4, 'Δelta ✓', 4.00, NULL, NULL, 'it''s'),
               (10, 'ten', 10, true, '2024-01-01 00:00:00+00', 't')",
        ],
    )
    .await;
    run(
        &*my,
        &[
            "DROP TABLE IF EXISTS dc_xe_items",
            "CREATE TABLE dc_xe_items (id bigint PRIMARY KEY, name text, price decimal(10,2), active tinyint(1), created datetime(6), note varchar(40)) DEFAULT CHARSET utf8mb4",
            "INSERT INTO dc_xe_items VALUES
               (1, 'Alpha', 1.5, 1, '2024-05-01 08:00:00', 'x'),
               (2, 'Beta', 2.00, 0, '2024-05-02 00:00:00', NULL),
               (3, 'Gamma', 3.25, 1, '2024-05-03 12:30:00.5', 'different'),
               (5, 'five', 5, 0, NULL, NULL),
               (9, 'nine', 9, 1, NULL, NULL)",
        ],
    )
    .await;
    let source = side(&pg, DB, Some("public"), "dc_xe_items");
    let target = side(&my, DB, None, "dc_xe_items");

    let (compared, changes) = compare(&source, &target, &DataCompareOptions::default()).await.unwrap();
    assert_eq!(compared.diff.counts, counts(1, 2, 2, 2), "keys 4 and 10 missing, 5 and 9 extra, 2 and 3 differ");
    assert!(compared.plan.columns.iter().all(|c| !c.type_mismatch), "{:?}", compared.plan.columns);
    let names: Vec<_> = compared.diff.examples.iter().map(|e| e.changed.clone()).filter(|c| !c.is_empty()).collect();
    assert_eq!(names, vec![vec![1], vec![5]], "name of row 2, note of row 3");
    let loose = DataCompareOptions { ignore_case: true, ..DataCompareOptions::default() };
    assert_eq!(compare(&source, &target, &loose).await.unwrap().0.diff.counts.different, 1);

    let report = sync(&target, &compared, changes).await;
    assert_eq!((report.inserted, report.updated, report.deleted), (2, 2, 2));
    let (again, rest) = compare(&source, &target, &DataCompareOptions::default()).await.unwrap();
    assert_eq!(again.diff.counts, counts(5, 0, 0, 0), "{:?}", again.diff.examples);
    assert_eq!(rest, Vec::<SyncChange>::new());

    run(&*pg, &["DROP TABLE dc_xe_items"]).await;
    run(&*my, &["DROP TABLE dc_xe_items"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_text_keys_stream_filter_and_sync() {
    let Some(pg) = postgres().await else {
        eprintln!("skipped: set DATAQUERY_TEST_PG");
        return;
    };
    // Mixed-case text keys: the database's linguistic collation would sort
    // them differently from the merge; the compare asks for code point order.
    let key = "CASE WHEN i % 2 = 0 THEN upper(substr(md5(i::text), 1, 4)) ELSE substr(md5(i::text), 1, 4) END";
    run(
        &*pg,
        &[
            "DROP TABLE IF EXISTS dc_pp_src, dc_pp_dst",
            "CREATE TABLE dc_pp_src (code varchar(20) NOT NULL, n int NOT NULL, v text, PRIMARY KEY (code, n))",
            &format!("INSERT INTO dc_pp_src SELECT {key}, i, 'v' || i FROM generate_series(1, 20000) i"),
            "CREATE TABLE dc_pp_dst (LIKE dc_pp_src INCLUDING ALL)",
            "INSERT INTO dc_pp_dst SELECT * FROM dc_pp_src",
            "DELETE FROM dc_pp_dst WHERE n % 1000 = 0",
            "UPDATE dc_pp_dst SET v = NULL WHERE n % 1000 = 1",
            "UPDATE dc_pp_dst SET v = 'changed' WHERE n % 1000 = 2",
            "INSERT INTO dc_pp_dst VALUES ('zz', 1, 'extra'), ('ZZ', 2, 'extra')",
        ],
    )
    .await;
    let source = side(&pg, DB, Some("public"), "dc_pp_src");
    let target = side(&pg, DB, Some("public"), "dc_pp_dst");
    let options = DataCompareOptions { max_examples: 10, ..DataCompareOptions::default() };
    let (compared, changes) = compare(&source, &target, &options).await.unwrap();
    assert_eq!(compared.diff.counts, counts(19_940, 20, 2, 40));
    assert_eq!(compared.diff.examples.len(), 10);
    assert!(compared.diff.examples_truncated);

    let filtered = DataCompareOptions {
        rows: RowFilter {
            conditions: vec![FilterCondition { column: "n".into(), op: FilterOp::Lte, value: "2000".into() }],
            ..RowFilter::default()
        },
        ..DataCompareOptions::default()
    };
    assert_eq!(compare(&source, &target, &filtered).await.unwrap().0.diff.counts, counts(1_994, 2, 2, 4));

    let duplicates = DataCompareOptions { key_columns: vec!["v".into()], ..DataCompareOptions::default() };
    run(&*pg, &["UPDATE dc_pp_src SET v = 'dup' WHERE n IN (5, 6)"]).await;
    let Err(e) = compare(&source, &target, &duplicates).await else { panic!("duplicate keys must fail") };
    assert!(e.message.contains("more than one row with key"), "{}", e.message);
    run(&*pg, &["UPDATE dc_pp_src SET v = 'v' || n WHERE n IN (5, 6)"]).await;

    let report = sync(&target, &compared, changes).await;
    assert_eq!((report.inserted, report.updated, report.deleted), (20, 40, 2));
    assert_eq!(compare(&source, &target, &options).await.unwrap().0.diff.counts, counts(20_000, 0, 0, 0));
    run(&*pg, &["DROP TABLE dc_pp_src, dc_pp_dst"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_files_with_nocase_keys_and_tolerance() {
    let dir = std::env::temp_dir().join(format!("dq-compare-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let open = |name: &str| {
        let cfg = ConnConfig {
            engine: Engine::Sqlite,
            database: dir.join(name).to_string_lossy().into_owned(),
            ..ConnConfig::default()
        };
        async move { dq_sqlite::SqliteAdapter.connect(&ConnectParams::new(cfg, None)).await.expect("open") }
    };
    let (a, b) = (open("a.sqlite").await, open("b.sqlite").await);
    let ddl = "CREATE TABLE dc_t (k TEXT COLLATE NOCASE PRIMARY KEY, v REAL, flag BOOLEAN, raw BLOB)";
    run(&*a, &[ddl, "INSERT INTO dc_t VALUES ('a', 1.0, 1, X'01'), ('B', 2.5, 0, NULL), ('c', 3.0, NULL, NULL), ('D', 4, 1, NULL)"])
        .await;
    run(&*b, &[ddl, "INSERT INTO dc_t VALUES ('a', 1.0000001, 1, X'01'), ('B', 2.5, 1, NULL), ('e', 5, 0, NULL)"])
        .await;
    let source = side(&a, "main", None, "dc_t");
    let target = side(&b, "main", None, "dc_t");

    let (compared, _) = compare(&source, &target, &DataCompareOptions::default()).await.unwrap();
    assert_eq!(compared.diff.counts, counts(0, 2, 1, 2));
    assert!(!compared.plan.columns.iter().find(|c| c.name == "raw").unwrap().synced, "binary is compared, not written");
    let tolerant = DataCompareOptions { float_tolerance: 0.001, ..DataCompareOptions::default() };
    let (compared, changes) = compare(&source, &target, &tolerant).await.unwrap();
    assert_eq!(compared.diff.counts, counts(1, 2, 1, 1));

    let report = sync(&target, &compared, changes).await;
    assert_eq!((report.inserted, report.updated, report.deleted), (2, 1, 1));
    assert_eq!(compare(&source, &target, &tolerant).await.unwrap().0.diff.counts, counts(4, 0, 0, 0));
    a.close().await;
    b.close().await;
    let _ = std::fs::remove_dir_all(dir);
}

/// A million rows per side; run with `--ignored` to check speed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow: a million rows per side"]
async fn postgres_million_rows() {
    let Some(pg) = postgres().await else {
        eprintln!("skipped: set DATAQUERY_TEST_PG");
        return;
    };
    run(
        &*pg,
        &[
            "DROP TABLE IF EXISTS dc_mm_src, dc_mm_dst",
            "CREATE TABLE dc_mm_src AS SELECT i AS id, md5(i::text) AS v, i * 0.5 AS n FROM generate_series(1, 1000000) i",
            "ALTER TABLE dc_mm_src ADD PRIMARY KEY (id)",
            "CREATE TABLE dc_mm_dst AS SELECT * FROM dc_mm_src WHERE id % 100000 <> 0",
            "ALTER TABLE dc_mm_dst ADD PRIMARY KEY (id)",
            "UPDATE dc_mm_dst SET v = 'x' WHERE id % 250000 = 1",
        ],
    )
    .await;
    let source = side(&pg, DB, Some("public"), "dc_mm_src");
    let target = side(&pg, DB, Some("public"), "dc_mm_dst");
    let started = std::time::Instant::now();
    let (compared, changes) = compare(&source, &target, &DataCompareOptions::default()).await.unwrap();
    eprintln!("compared two million rows in {:?}", started.elapsed());
    assert_eq!(compared.diff.counts, counts(999_986, 10, 0, 4));
    assert_eq!(changes.len(), 14);
    run(&*pg, &["DROP TABLE dc_mm_src, dc_mm_dst"]).await;
}
