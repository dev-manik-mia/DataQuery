//! Integration tests against a real `MongoDB` replica set.
//!
//! Start it with `docker compose -p dataquery up -d mongodb` and run
//! `DATAQUERY_TEST_MONGODB=localhost:57017 cargo test -p dq-mongodb`.
//! Without the variable every test is skipped. Tests that write use their
//! own database and drop it afterwards; `dataquery_test` is the seed.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::document::FindRequest;
use dq_core::engine::Engine;
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::ops::{self, ImportMapping};
use dq_core::result::ExecOptions;
use dq_core::table::RowFilter;
use dq_core::transfer::{
    CsvOptions, ExportFormat, ExportOptions, ExportSource, FieldRef, ImportOptions, ImportSource, TransferControl,
};
use dq_core::value::Cell;
use dq_core::{DbResult, ErrorKind};
use dq_mongodb::MongodbAdapter;

const DB: &str = "dataquery_test";
const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };

fn config() -> Option<ConnConfig> {
    let target = std::env::var("DATAQUERY_TEST_MONGODB").ok()?;
    let (host, port) = target.split_once(':').unwrap_or((&target, "27017"));
    Some(ConnConfig {
        engine: Engine::Mongodb,
        host: host.into(),
        port: port.parse().unwrap(),
        database: DB.into(),
        username: "dataquery".into(),
        ssl_mode: SslMode::Disable,
        connect_timeout_secs: 5,
        ..ConnConfig::default()
    })
}

macro_rules! require_db {
    () => {
        match config() {
            Some(c) => c,
            None => {
                eprintln!("skipped: set DATAQUERY_TEST_MONGODB to run MongoDB integration tests");
                return Ok(());
            }
        }
    };
}

async fn session(cfg: &ConnConfig) -> Arc<dyn Session> {
    MongodbAdapter.connect(&ConnectParams::new(cfg.clone(), Some("dataquery".into()))).await.expect("connect")
}

fn coll(db: &str, name: &str) -> ObjectRef {
    ObjectRef::new(NodeKind::Table, db, None, name)
}

/// A database of its own, dropped when the test ends.
struct Scratch {
    session: Arc<dyn Session>,
    name: String,
}

impl Scratch {
    async fn new(cfg: &ConnConfig) -> DbResult<Self> {
        let name = format!("dq_it_{}", uuid_suffix());
        let mut cfg = cfg.clone();
        cfg.database.clone_from(&name);
        Ok(Self { session: session(&cfg).await, name })
    }

    async fn run(&self, sql: &str) -> DbResult<dq_core::result::StatementResult> {
        self.session.execute(sql, &[], OPTS).await
    }

    async fn drop(self) {
        let _ = self.run("db.dropDatabase()").await;
        self.session.close().await;
    }
}

fn uuid_suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_owned()
}

fn column(result: &dq_core::result::StatementResult, name: &str) -> Vec<Cell> {
    let i = result.columns.iter().position(|c| c.name == name).unwrap_or_else(|| panic!("no column {name}"));
    result.rows.iter().map(|r| r[i].clone()).collect()
}

#[tokio::test]
async fn connects_and_reports_server() -> DbResult<()> {
    let cfg = require_db!();
    let info = MongodbAdapter.test(&ConnectParams::new(cfg.clone(), Some("dataquery".into()))).await?;
    assert!(info.version.starts_with("MongoDB 8"), "{}", info.version);
    assert_eq!((info.user.as_str(), info.database.as_str()), ("dataquery", DB));
    let wrong = MongodbAdapter.test(&ConnectParams::new(cfg, Some("nope".into()))).await.unwrap_err();
    assert_eq!(wrong.kind, ErrorKind::Auth, "{wrong:?}");
    Ok(())
}

#[tokio::test]
async fn explorer_lists_databases_collections_and_views() -> DbResult<()> {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let dbs = s.list_children(None).await?;
    assert!(dbs.iter().any(|d| d.object.name == DB && d.current));
    let db = ObjectRef::new(NodeKind::Database, DB, None, DB);
    let children = s.list_children(Some(&db)).await?;
    let find = |n: &str| children.iter().find(|c| c.object.name == n).unwrap();
    assert_eq!((find("orders").object.kind, find("orders").row_estimate), (NodeKind::Table, Some(3)));
    assert_eq!(find("paid_orders").object.kind, NodeKind::View);
    assert!(s.search("ORDER", 10).await?.iter().any(|n| n.object.name == "orders"));
    let deps = s.dependencies(&coll(DB, "orders")).await?;
    assert_eq!(deps.used_by[0].object.name, "paid_orders");
    let ddl = s.ddl(&coll(DB, "customers")).await?;
    assert!(ddl.starts_with("db.createCollection('customers', {") && ddl.contains("email_unique"), "{ddl}");
    Ok(())
}

#[tokio::test]
async fn find_keeps_exact_numbers_and_pages() -> DbResult<()> {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let opts = ExecOptions { row_limit: 2, timeout_secs: 30 };
    let r = s.execute("db.orders.find({}, { items: 0 })\n  .sort({ _id: 1 })", &[], opts).await?;
    assert_eq!(r.rows.len(), 2);
    assert!(r.has_more);
    assert_eq!(column(&r, "big")[0], Cell::number("9007199254740993"));
    assert_eq!(column(&r, "total")[0], Cell::number("120.50"));
    assert_eq!(column(&r, "createdAt")[0], Cell::Temporal { value: "2026-10-01T10:00:00.000Z".into(), zoned: true });
    let more = s.fetch_more(10).await?;
    assert_eq!((more.rows.len(), more.has_more), (1, false));
    assert_eq!(more.rows[0].len(), r.columns.len());
    let count = s.execute("db.orders.find({ status: 'paid' }).count()", &[], OPTS).await?;
    assert_eq!(count.rows[0][0], Cell::number("2"));
    let agg = s
        .execute("db.getCollection('orders').aggregate([{ $group: { _id: '$status', n: { $sum: 1 } } }, { $sort: { _id: 1 } }])", &[], OPTS)
        .await?;
    assert_eq!(
        column(&agg, "_id"),
        [Cell::text("paid", dq_core::value::CellLimits::GRID), Cell::text("pending", dq_core::value::CellLimits::GRID)]
    );
    let shown = s.execute("show collections", &[], OPTS).await?;
    assert!(column(&shown, "name").contains(&Cell::text("orders", dq_core::value::CellLimits::GRID)));
    let err = s.execute("db.orders.find({ status: paid })", &[], OPTS).await.unwrap_err();
    assert_eq!((err.kind, err.position), (ErrorKind::Syntax, Some(26)));
    Ok(())
}

#[tokio::test]
async fn writes_report_counts_and_read_only_refuses() -> DbResult<()> {
    let cfg = require_db!();
    let x = Scratch::new(&cfg).await?;
    let r = x.run("db.items.insertMany([{ n: 1 }, { n: 2 }, { n: 3 }])").await?;
    assert_eq!((r.affected, r.command.as_str()), (Some(3), "INSERTMANY"));
    assert!(r.notices[0].starts_with("Inserted ids: ObjectId('"));
    assert_eq!(x.run("db.items.updateMany({ n: { $gte: 2 } }, { $inc: { n: 10 } })").await?.affected, Some(2));
    assert_eq!(x.run("db.items.deleteOne({ n: 1 })").await?.affected, Some(1));
    let r = x.run("db.items.updateOne({ n: 99 }, { $set: { n: 99 } }, { upsert: true })").await?;
    assert!(r.notices.iter().any(|n| n.starts_with("Upserted ids")));
    x.run("db.items.createIndex({ n: 1 }, { unique: true })").await?;
    let dup = x.run("db.items.insertOne({ n: 99 })").await.unwrap_err();
    assert_eq!(dup.kind, ErrorKind::Constraint);
    let found =
        x.run("db.items.findOneAndUpdate({ n: 12 }, { $set: { tag: 'x' } }, { returnDocument: 'after' })").await?;
    assert_eq!(column(&found, "tag")[0], Cell::text("x", dq_core::value::CellLimits::GRID));

    let mut ro = cfg.clone();
    ro.database.clone_from(&x.name);
    ro.read_only = true;
    let s = session(&ro).await;
    let refused = s.execute("db.items.insertOne({ n: 5 })", &[], OPTS).await.unwrap_err();
    assert_eq!(refused.kind, ErrorKind::Permission);
    assert_eq!(s.execute("db.items.countDocuments({})", &[], OPTS).await?.rows[0][0], Cell::number("3"));
    let store = s.documents().unwrap();
    assert_eq!(store.insert(&coll(&x.name, "items"), "{ n: 7 }").await.unwrap_err().kind, ErrorKind::Permission);
    x.drop().await;
    Ok(())
}

#[tokio::test]
async fn documents_view_reads() -> DbResult<()> {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let store = s.documents().unwrap();
    assert_eq!(store.features().language, "MQL");
    let mut req = FindRequest::new(coll(DB, "orders"), 2);
    req.filter = "{ status: 'paid' }".into();
    req.sort = "{ total: -1 }".into();
    req.projection = "{ total: 1, big: 1 }".into();
    let page = store.find(&req).await?;
    assert_eq!(page.documents.len(), 2);
    assert_eq!(page.next, None);
    let first = &page.documents[0];
    assert_eq!(first.key, "ObjectId('66f000000000000000000001')");
    assert!(first.text.contains("total: Decimal128('120.50')") && first.text.contains("big: Long('9007199254740993')"));
    assert_eq!(
        first.fields.iter().map(|f| f.type_label.as_str()).collect::<Vec<_>>(),
        ["ObjectId", "Decimal128", "Int64"]
    );

    let mut all = FindRequest::new(coll(DB, "orders"), 2);
    let p1 = store.find(&all).await?;
    all.cursor.clone_from(&p1.next);
    let p2 = store.find(&all).await?;
    assert_eq!((p1.documents.len(), p2.documents.len(), p2.next.clone()), (2, 1, None));

    let count = store.count(&coll(DB, "orders"), "{ status: 'paid' }").await?.unwrap();
    assert_eq!((count.count, count.exact), (2, true));
    let agg = store.aggregate(&coll(DB, "orders"), "[{ $group: { _id: '$customer', n: { $sum: 1 } } }]", 10).await?;
    assert_eq!(agg.documents.len(), 2);
    let indexes = store.indexes(&coll(DB, "customers")).await?;
    let email = indexes.iter().find(|i| i.name == "email_unique").unwrap();
    assert!(email.unique && email.keys == "{ email: 1 }");
    let rules = store.validation(&coll(DB, "customers")).await?.unwrap();
    assert!(rules.starts_with("{\n  validator: {\n    $jsonSchema: {"), "{rules}");
    assert_eq!(store.validation(&coll(DB, "orders")).await?, None);
    let stmt = store.find_statement(&req)?;
    assert_eq!(stmt, "db.getCollection('orders').find({ status: 'paid' }, { total: 1, big: 1 }).sort({ total: -1 })");
    assert_eq!(s.execute(&stmt, &[], OPTS).await?.rows.len(), 2);
    Ok(())
}

#[tokio::test]
async fn document_edits_keep_types_and_detect_changes() -> DbResult<()> {
    let cfg = require_db!();
    let x = Scratch::new(&cfg).await?;
    x.run("db.createCollection('things')").await?;
    let store = x.session.documents().unwrap();
    let things = coll(&x.name, "things");
    let key = store.insert(&things, "{ name: 'a', big: Long('9007199254740993'), price: Decimal128('1.10') }").await?;
    assert!(key.starts_with("ObjectId('"));
    let doc = store.find(&FindRequest::new(things.clone(), 10)).await?.documents.remove(0);
    let edited = doc.text.replace("name: 'a'", "name: 'b'");
    store.replace(&things, &doc.key, &edited).await?;
    let doc = store.find(&FindRequest::new(things.clone(), 10)).await?.documents.remove(0);
    assert!(
        doc.text.contains("name: 'b'")
            && doc.text.contains("Long('9007199254740993')")
            && doc.text.contains("Decimal128('1.10')")
    );
    let changed_id = store.replace(&things, &doc.key, "{ _id: 5, name: 'c' }").await.unwrap_err();
    assert_eq!(changed_id.kind, ErrorKind::InvalidInput);
    store.delete(&things, &doc.key).await?;
    assert_eq!(store.delete(&things, &doc.key).await.unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(store.replace(&things, &doc.key, "{ name: 'd' }").await.unwrap_err().kind, ErrorKind::NotFound);
    let custom = store.insert(&things, "{ _id: 'k1', n: 1 }").await?;
    assert_eq!(custom, "'k1'");
    let view = ObjectRef::new(NodeKind::View, DB, None, "paid_orders");
    assert!(store.delete(&view, "1").await.is_err());
    x.drop().await;
    Ok(())
}

#[tokio::test]
async fn transactions_commit_and_roll_back() -> DbResult<()> {
    let cfg = require_db!();
    let x = Scratch::new(&cfg).await?;
    x.run("db.createCollection('tx')").await?;
    x.run("BEGIN").await?;
    x.run("db.tx.insertOne({ n: 1 })").await?;
    assert_eq!(x.run("db.tx.countDocuments({})").await?.rows[0][0], Cell::number("1"));
    x.run("ROLLBACK").await?;
    assert_eq!(x.run("db.tx.countDocuments({})").await?.rows[0][0], Cell::number("0"));
    x.run("begin").await?;
    x.run("db.tx.insertMany([{ n: 2 }, { n: 3 }])").await?;
    assert!(x.run("BEGIN").await.is_err());
    x.run("COMMIT").await?;
    assert_eq!(x.run("db.tx.countDocuments({})").await?.rows[0][0], Cell::number("2"));
    assert!(x.run("COMMIT").await.is_err());
    // A failing statement in run_in_transaction leaves nothing behind.
    let statements = vec![
        Ok(dq_core::edit::BoundStatement {
            sql: "db.tx.insertOne({ n: 4 })".into(),
            params: vec![],
            expect_rows: Some(1),
        }),
        Ok(dq_core::edit::BoundStatement {
            sql: "db.tx.deleteOne({ n: 404 })".into(),
            params: vec![],
            expect_rows: Some(1),
        }),
    ];
    let err = x.session.run_in_transaction(&x.name, &mut statements.into_iter()).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Constraint);
    assert_eq!(x.run("db.tx.countDocuments({})").await?.rows[0][0], Cell::number("2"));
    x.run("use admin").await?;
    assert_eq!(
        x.run(&format!("db.getSiblingDB('{}').tx.countDocuments({{}})", x.name)).await?.rows[0][0],
        Cell::number("2")
    );
    x.run(&format!("use {}", x.name)).await?;
    x.drop().await;
    Ok(())
}

#[tokio::test]
async fn cancel_and_timeout_stop_the_server_operation() -> DbResult<()> {
    let cfg = require_db!();
    let x = Scratch::new(&cfg).await?;
    x.run("db.slow.insertMany([{ a: 1 }, { a: 2 }, { a: 3 }, { a: 4 }, { a: 5 }, { a: 6 }, { a: 7 }, { a: 8 }, { a: 9 }, { a: 10 }])").await?;
    let slow = "db.slow.find({ $where: 'sleep(1000) || true' })";
    let session = x.session.clone();
    let started = Instant::now();
    let running = tokio::spawn(async move { session.execute(slow, &[], OPTS).await });
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(x.session.cancel().await?);
    let err = running.await.unwrap().unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled, "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
    assert!(!x.session.cancel().await?);
    // killOp stopped the server operation too.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let ops = x.run(&format!("db.adminCommand({{ currentOp: 1, ns: '{}.slow' }})", x.name)).await?;
    assert_eq!(column(&ops, "inprog")[0], Cell::json("[]", dq_core::value::CellLimits::GRID));
    let started = Instant::now();
    let err = x.session.execute(slow, &[], ExecOptions { row_limit: 100, timeout_secs: 1 }).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout, "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    // The session still works.
    assert_eq!(x.run("db.slow.countDocuments({})").await?.rows[0][0], Cell::number("10"));
    x.drop().await;
    Ok(())
}

#[tokio::test]
async fn explain_shows_the_index() -> DbResult<()> {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let plan = s.explain("db.orders.find({ status: 'paid' }).sort({ createdAt: -1 })", &[], false, 30).await?;
    let mut labels = Vec::new();
    let mut stack = vec![plan.plan.clone().unwrap()];
    while let Some(n) = stack.pop() {
        labels.push((n.label.clone(), n.detail.clone()));
        stack.extend(n.children);
    }
    assert!(
        labels.iter().any(|(l, d)| l == "IXSCAN" && d.as_deref().is_some_and(|d| d.contains("status_created"))),
        "{labels:?}"
    );
    let analyzed = s.explain("db.orders.find({ status: 'paid' })", &[], true, 30).await?;
    assert!(analyzed.analyzed && analyzed.execution_ms.is_some());
    assert!(s.explain("db.orders.insertOne({})", &[], false, 30).await.is_err());
    let raw = s.execute("db.orders.find({ status: 'paid' }).explain()", &[], OPTS).await?;
    assert!(raw.columns.iter().any(|c| c.name == "queryPlanner"));
    Ok(())
}

#[tokio::test]
async fn structure_import_and_export() -> DbResult<()> {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let structure = ops::structure(&*s, &coll(DB, "orders")).await?;
    let cols: Vec<(&str, &str)> = structure.columns.iter().map(|c| (c.name.as_str(), c.data_type.as_str())).collect();
    assert_eq!(cols[0], ("_id", "ObjectId"));
    assert!(cols.contains(&("total", "Decimal128")) && cols.contains(&("big", "Int64")));
    assert_eq!(structure.row_identity, ["_id"]);
    assert!(structure.indexes.iter().any(|i| i.name == "status_created"));

    let dir = tempfile::tempdir().unwrap();
    let none = TransferControl::none();
    let csv = dir.path().join("orders.csv");
    let source = ExportSource::Table { object: coll(DB, "orders"), rows: RowFilter::default(), sort: None };
    assert_eq!(ops::export(&*s, &source, ExportFormat::Csv, &ExportOptions::default(), &csv, &none).await?.rows, 3);
    let text = std::fs::read_to_string(&csv).unwrap();
    assert!(text.contains("9007199254740993") && text.contains("120.50"), "{text}");
    // The Documents view exports its filter, as JSON.
    let json = dir.path().join("paid.json");
    let mut paid = FindRequest::new(coll(DB, "orders"), 20);
    paid.filter = "{ status: 'paid' }".into();
    let source = ExportSource::Documents { request: paid };
    assert_eq!(ops::export(&*s, &source, ExportFormat::Json, &ExportOptions::default(), &json, &none).await?.rows, 2);

    let x = Scratch::new(&cfg).await?;
    x.run("db.imported.insertOne({ _id: 0, qty: 1, price: Decimal128('0.10'), status: 'seed' })").await?;
    let input = dir.path().join("in.csv");
    std::fs::write(&input, "qty,price,status,note\n7,12.50,new,{ a: 1 }\n9000000000,3.10,old,plain\n").unwrap();
    let import = ImportSource::Csv { path: input.to_string_lossy().into_owned(), options: CsvOptions::default() };
    let map: Vec<ImportMapping> = [("qty", 0), ("price", 1), ("status", 2), ("note", 3)]
        .iter()
        .map(|(c, i)| ImportMapping { column: (*c).into(), field: FieldRef::Index(*i) })
        .collect();
    let report =
        ops::import(&*x.session, &coll(&x.name, "imported"), &import, &map, &ImportOptions::default(), &none).await?;
    assert_eq!(report.rows, 2, "{report:?}");
    let back = x.run("db.imported.find({ status: 'new' })").await?;
    assert_eq!(column(&back, "qty")[0], Cell::number("7"));
    let store = x.session.documents().unwrap();
    let mut req = FindRequest::new(coll(&x.name, "imported"), 10);
    req.filter = "{ status: 'old' }".into();
    let old = store.find(&req).await?.documents.remove(0);
    let label = |k: &str| old.fields.iter().find(|f| f.key == k).unwrap().type_label.clone();
    // qty was sampled as Int32; a value too large for it is widened to Int64.
    assert_eq!(label("qty"), "Int64");
    assert_eq!(label("price"), "Decimal128");
    assert_eq!(label("note"), "String");
    let new = x.run("db.imported.find({ status: 'new' })").await?;
    assert_eq!(column(&new, "note")[0], Cell::json("{\"a\":1}", dq_core::value::CellLimits::GRID));
    x.drop().await;
    Ok(())
}

#[tokio::test]
async fn monitor_lists_operations_and_settings() -> DbResult<()> {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let m = s.monitor().unwrap();
    let snap = m.snapshot(5).await?;
    assert!(snap.connections.is_some_and(|c| c.used > 0 && c.max > c.used));
    assert_eq!(snap.replication.as_ref().map(|r| r.role.as_str()), Some("primary"));
    let settings = m.server_settings(Some("featureCompatibility")).await?;
    assert!(!settings.is_empty());
    assert!(m.largest_tables(DB, 10).await?.iter().any(|t| t.object.name == "orders"));
    assert_eq!(m.kill(987_654_321, false).await.unwrap_err().kind, ErrorKind::NotFound);
    Ok(())
}
