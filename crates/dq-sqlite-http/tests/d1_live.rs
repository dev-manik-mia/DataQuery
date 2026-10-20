//! Read-only checks against a real Cloudflare D1 database. Nothing is written:
//! the connection uses a read-only profile, and the only write attempted must be
//! refused. Only table names and counts are printed, never row values.
//!
//! ```sh
//! DATAQUERY_TEST_D1_ACCOUNT=<account id> DATAQUERY_TEST_D1_DATABASE=<database id or name> \
//! DATAQUERY_TEST_D1_TOKEN=<API token with D1 read> cargo test -p dq-sqlite-http --test d1_live -- --nocapture
//! ```

use std::collections::BTreeMap;

use dq_core::adapter::{ConnectParams, DatabaseAdapter};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::engine::Engine;
use dq_core::node::NodeKind;
use dq_core::result::ExecOptions;
use dq_sqlite_http::D1Adapter;

struct Live {
    account: String,
    database: String,
    token: String,
}

fn live() -> Option<Live> {
    let var = |name| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    Some(Live {
        account: var("DATAQUERY_TEST_D1_ACCOUNT")?,
        database: var("DATAQUERY_TEST_D1_DATABASE")?,
        token: var("DATAQUERY_TEST_D1_TOKEN")?,
    })
}

fn params(live: &Live, database: &str) -> ConnectParams {
    let config = ConnConfig {
        engine: Engine::D1,
        host: "api.cloudflare.com".into(),
        port: 443,
        database: database.into(),
        ssl_mode: SslMode::VerifyFull,
        read_only: true,
        properties: BTreeMap::from([(ConnConfig::D1_ACCOUNT.to_owned(), live.account.clone())]),
        ..ConnConfig::default()
    };
    ConnectParams::new(config, Some(live.token.clone()))
}

const OPTS: ExecOptions = ExecOptions { row_limit: 20, timeout_secs: 30 };

#[tokio::test]
async fn real_d1_read_only() {
    let Some(live) = live() else {
        eprintln!("skipped: set DATAQUERY_TEST_D1_ACCOUNT, DATAQUERY_TEST_D1_DATABASE and DATAQUERY_TEST_D1_TOKEN");
        return;
    };

    let info = D1Adapter.test(&params(&live, &live.database)).await.expect("test connection");
    println!("server: {} ({} ms), read-only: {}", info.version, info.latency_ms, info.read_only);

    let databases = D1Adapter.databases(&params(&live, &live.database)).await.expect("database list");
    println!("account databases: {}", databases.len());

    let session = D1Adapter.connect(&params(&live, &live.database)).await.expect("connect");
    let schemas = session.list_children(None).await.expect("explorer root");
    let main = schemas.first().expect("main").object.clone();
    let mut tables = Vec::new();
    for group in session.list_children(Some(&main)).await.expect("explorer groups") {
        let children = if group.object.kind == NodeKind::Table {
            vec![group]
        } else {
            session.list_children(Some(&group.object)).await.unwrap_or_default()
        };
        tables.extend(children.into_iter().filter(|n| matches!(n.object.kind, NodeKind::Table | NodeKind::View)));
    }
    println!("tables and views: {}", tables.len());
    assert!(tables.iter().all(|t| !t.object.name.starts_with("_cf_")), "internal tables are hidden");

    for table in tables.iter().take(25) {
        let structure = session
            .structure(&table.object)
            .await
            .unwrap_or_else(|e| panic!("structure of {}: {e:?}", table.object.name));
        let ddl = session.ddl(&table.object).await.unwrap_or_else(|e| panic!("ddl of {}: {e:?}", table.object.name));
        let name = session.dialect().quote_ident(&table.object.name);
        let counted = session.execute(&format!("SELECT count(*) FROM {name}"), &[], OPTS).await.expect("count");
        let count = counted.rows.first().and_then(|r| r.first()).map(|c| format!("{c:?}")).unwrap_or_default();
        let page = session.execute(&format!("SELECT * FROM {name}"), &[], OPTS).await.expect("first page");
        let shown = page.rows.len();
        assert!(shown <= OPTS.row_limit as usize, "results stay bounded");
        println!(
            "  {:<32} {:>2} columns, {:>2} keys, ddl {:>5} chars, rows {count}, first page {shown}",
            table.object.name,
            structure.columns.len(),
            structure.constraints.len(),
            ddl.len()
        );
    }

    let model = session.schema_model("main", None).await.expect("schema model");
    println!("schema model tables: {}", model.tables.len());
    let plan = session.explain("SELECT * FROM sqlite_master", &[], false, 30).await;
    println!("explain: {}", if plan.is_ok() { "ok" } else { "not available" });

    let refused = session.execute("CREATE TABLE dataquery_live_check (a)", &[], OPTS).await;
    assert!(refused.is_err(), "the read-only profile must refuse writes");
    println!("write refused: {}", refused.unwrap_err().message);

    session.close().await;
}

/// Writes, against a throwaway database (never one with real data):
/// `DATAQUERY_TEST_D1_WRITE_DATABASE=<id or name>` plus the account and token above.
/// Creates its tables, checks values, paging, edits, batches, scripts and imports,
/// then drops everything it created.
#[tokio::test]
async fn real_d1_writes() -> dq_core::DbResult<()> {
    use dq_core::ErrorKind;
    use dq_core::edit::{ColumnValue, RowChange};
    use dq_core::node::ObjectRef;
    use dq_core::ops::{self, ImportMapping};
    use dq_core::transfer::{CsvOptions, FieldRef, ImportOptions, ImportSource, TransferControl};
    use dq_core::value::Cell;

    let (Some(live), Ok(database)) = (live(), std::env::var("DATAQUERY_TEST_D1_WRITE_DATABASE")) else {
        eprintln!("skipped: set DATAQUERY_TEST_D1_WRITE_DATABASE (a throwaway database) with the account and token");
        return Ok(());
    };
    let mut writable = params(&live, &database);
    writable.config.read_only = false;
    let s = D1Adapter.connect(&writable).await?;
    let opts = ExecOptions { row_limit: 500, timeout_secs: 30 };
    let table = |name: &str| ObjectRef::new(NodeKind::Table, "main", None, name);
    let text = |c: &Cell| match c {
        Cell::Text { value, .. } | Cell::Number { value } => value.clone(),
        other => format!("{other:?}"),
    };
    let setup = [
        "DROP TABLE IF EXISTS dq_orders",
        "DROP TABLE IF EXISTS dq_customers",
        "DROP TABLE IF EXISTS dq_wide",
        "DROP TABLE IF EXISTS dq_types",
        "CREATE TABLE dq_customers (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE)",
        "CREATE TABLE dq_orders (id INTEGER PRIMARY KEY AUTOINCREMENT, customer_id INTEGER REFERENCES dq_customers (id), total NUMERIC, notes TEXT)",
        "CREATE TABLE dq_types (id INTEGER PRIMARY KEY, big INTEGER, ratio REAL, raw BLOB, label TEXT, doc TEXT)",
        "CREATE TABLE dq_wide (id INTEGER PRIMARY KEY, a TEXT, b TEXT, c TEXT, d TEXT, e TEXT, f TEXT)",
        "INSERT INTO dq_customers VALUES (1, 'ada@example.com'), (2, 'grace@example.com')",
        "INSERT INTO dq_types VALUES (1, 9223372036854775807, 1.5, X'89504E47', 'Zoë ✓', '{\"a\":1}')",
        "INSERT INTO dq_orders (customer_id, total, notes) WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1200) SELECT 1 + i % 2, i * 1.25, NULL FROM n",
    ];
    for sql in setup {
        s.execute(sql, &[], opts).await.unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }

    // Values as D1 sends them: exact big integers, reals, blobs, unicode.
    let r = s.execute("SELECT big, ratio, raw, label FROM dq_types", &[], opts).await?;
    // D1 sends JavaScript numbers: the value arrives rounded, and the result says so.
    assert_eq!(r.rows[0][0], Cell::number("9223372036854776000"));
    assert!(r.notices.iter().any(|n| n.contains("CAST(column AS TEXT)")), "{:?}", r.notices);
    let exact = s.execute("SELECT CAST(big AS TEXT) FROM dq_types", &[], opts).await?;
    assert_eq!(text(&exact.rows[0][0]), "9223372036854775807");
    assert_eq!(r.rows[0][1], Cell::number("1.5"));
    assert_eq!(r.rows[0][2], Cell::Binary { length: 4, head_hex: "89504e47".into() }, "BLOB format of /raw");
    assert_eq!(text(&r.rows[0][3]), "Zoë ✓");
    println!("values: ok");

    // Bounded results and fetch more.
    let r = s.execute("SELECT id FROM dq_orders ORDER BY id", &[], opts).await?;
    assert_eq!((r.rows.len(), r.has_more), (500, true));
    let page = s.fetch_more(1000).await?;
    assert_eq!((page.rows.len(), page.has_more), (700, false));
    println!("paging: ok");

    // Structure, keys and errors.
    let orders = ops::structure(&*s, &table("dq_orders")).await?;
    assert_eq!(orders.row_identity, ["id"]);
    assert_eq!(orders.foreign_keys[0].target.name, "dq_customers");
    let r = s.execute("UPDATE dq_orders SET notes = ?1 WHERE id <= 3", &[Some("it's".into())], opts).await?;
    assert_eq!(r.affected, Some(3));
    assert_eq!(s.execute("SELEC 1", &[], opts).await.unwrap_err().kind, ErrorKind::Syntax);
    assert_eq!(s.execute("SELECT * FROM dq_missing", &[], opts).await.unwrap_err().kind, ErrorKind::NotFound);
    let dup = s.execute("INSERT INTO dq_customers VALUES (1, 'x')", &[], opts).await.unwrap_err();
    assert_eq!(dup.kind, ErrorKind::Constraint);
    println!("structure and errors: ok");

    // Grid edits: one all-or-nothing batch; a row that no longer matches undoes the lot.
    let cv = |c: &str, v: Option<&str>| ColumnValue { column: c.into(), value: v.map(Into::into) };
    let changes = vec![
        RowChange::Update { key: vec![cv("id", Some("1"))], values: vec![cv("notes", Some("Zoë ✓"))] },
        RowChange::Insert { values: vec![cv("customer_id", Some("1")), cv("total", Some("9.99"))] },
        RowChange::Delete { key: vec![cv("id", Some("2"))] },
    ];
    let report = ops::apply_changes(&*s, &table("dq_orders"), &changes, false).await?;
    assert_eq!((report.updated, report.inserted, report.deleted), (1, 1, 1));
    let failing = vec![
        RowChange::Update { key: vec![cv("id", Some("3"))], values: vec![cv("notes", Some("lost"))] },
        RowChange::Delete { key: vec![cv("id", Some("404040"))] },
    ];
    assert_eq!(
        ops::apply_changes(&*s, &table("dq_orders"), &failing, false).await.unwrap_err().kind,
        ErrorKind::Constraint
    );
    let r = s.execute("SELECT notes FROM dq_orders WHERE id = 3", &[], opts).await?;
    assert_ne!(text(&r.rows[0][0]), "lost", "the whole batch rolled back");
    println!("edits: ok");

    // Scripts run as one batch: a failure applies nothing.
    let script = ["INSERT INTO dq_customers VALUES (3, 'c@example.com')", "SELECT count(*) FROM dq_customers"];
    let results = s.execute_script(&script, opts).await.expect("scripts run as a batch")?;
    assert_eq!(text(&results[1].rows[0][0]), "3");
    let broken = ["INSERT INTO dq_customers VALUES (4, 'd@example.com')", "INSERT INTO dq_missing VALUES (1)"];
    assert!(s.execute_script(&broken, opts).await.expect("batch").is_err());
    let r = s.execute("SELECT count(*) FROM dq_customers WHERE id = 4", &[], opts).await?;
    assert_eq!(text(&r.rows[0][0]), "0", "nothing of a failed script is applied");
    println!("scripts: ok");

    // Import: 300 rows × 7 columns within D1's 100 parameters per statement.
    let dir = std::env::temp_dir().join(format!("dq-d1-live-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let csv = dir.join("wide.csv");
    let lines: Vec<String> = (1..=300).map(|i| format!("{i},a{i},b,c,d,e,f")).collect();
    std::fs::write(&csv, format!("id,a,b,c,d,e,f\n{}\n", lines.join("\n"))).expect("csv");
    let source = ImportSource::Csv { path: csv.to_string_lossy().into_owned(), options: CsvOptions::default() };
    let map: Vec<ImportMapping> = ["id", "a", "b", "c", "d", "e", "f"]
        .iter()
        .enumerate()
        .map(|(i, c)| ImportMapping { column: (*c).into(), field: FieldRef::Index(i) })
        .collect();
    let report =
        ops::import(&*s, &table("dq_wide"), &source, &map, &ImportOptions::default(), &TransferControl::none()).await?;
    assert_eq!(report.rows, 300);
    std::fs::remove_dir_all(&dir).ok();
    let r = s.execute("SELECT count(*) FROM dq_wide", &[], opts).await?;
    assert_eq!(text(&r.rows[0][0]), "300");
    println!("import: ok");

    for sql in ["DROP TABLE dq_orders", "DROP TABLE dq_customers", "DROP TABLE dq_wide", "DROP TABLE dq_types"] {
        s.execute(sql, &[], opts).await?;
    }
    s.close().await;
    println!("cleaned up");
    Ok(())
}
