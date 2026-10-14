//! Integration tests against Neo4j 5 Community over the Query API.
//!
//! Start it with `docker compose -p dataquery up -d neo4j` and run
//! `DATAQUERY_TEST_NEO4J=localhost:57474 cargo test -p dq-neo4j`. Without the
//! variable every test is skipped. Each test uses labels with its own prefix in
//! the `neo4j` database and deletes them afterwards.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::document::FindRequest;
use dq_core::engine::Engine;
use dq_core::node::{GROUP_INDEXES, GROUP_LABELS, GROUP_RELATIONSHIPS, NodeKind, ObjectRef};
use dq_core::ops::{self, ImportMapping};
use dq_core::result::{ExecOptions, StatementResult};
use dq_core::transfer::{
    CsvOptions, ExportFormat, ExportOptions, ExportSource, FieldRef, ImportOptions, ImportSource, TransferControl,
};
use dq_core::value::Cell;
use dq_core::{DbResult, ErrorKind};
use dq_neo4j::Neo4jAdapter;

const DB: &str = "neo4j";
const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };

fn config() -> Option<ConnConfig> {
    let target = std::env::var("DATAQUERY_TEST_NEO4J").ok()?;
    let (host, port) = target.split_once(':').unwrap_or((&target, "7474"));
    Some(ConnConfig {
        engine: Engine::Neo4j,
        host: host.into(),
        port: port.parse().unwrap(),
        database: DB.into(),
        username: "neo4j".into(),
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
                eprintln!("skipped: set DATAQUERY_TEST_NEO4J to run Neo4j integration tests");
                return Ok(());
            }
        }
    };
}

async fn session(cfg: &ConnConfig) -> Arc<dyn Session> {
    Neo4jAdapter.connect(&ConnectParams::new(cfg.clone(), Some("dataquery123".into()))).await.expect("connect")
}

/// Labels of their own, deleted when the test ends.
struct Scratch {
    session: Arc<dyn Session>,
    prefix: String,
}

impl Scratch {
    async fn new(cfg: &ConnConfig) -> Self {
        Self { session: session(cfg).await, prefix: format!("Dq{}", &uuid::Uuid::new_v4().simple().to_string()[..10]) }
    }

    fn label(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }

    async fn run(&self, sql: &str) -> DbResult<StatementResult> {
        self.session.execute(&sql.replace("$P", &self.prefix), &[], OPTS).await
    }

    async fn drop(self) {
        let labels = self.run("CALL db.labels() YIELD label WHERE label STARTS WITH '$P' RETURN label").await;
        for row in labels.map(|r| r.rows).unwrap_or_default() {
            if let Some(Cell::Text { value, .. }) = row.first() {
                let _ = self.session.execute(&format!("MATCH (n:`{value}`) DETACH DELETE n"), &[], OPTS).await;
            }
        }
        let indexes = self.run("SHOW INDEXES YIELD name WHERE name STARTS WITH 'idx_$P' RETURN name").await;
        for row in indexes.map(|r| r.rows).unwrap_or_default() {
            if let Some(Cell::Text { value, .. }) = row.first() {
                let _ = self.session.execute(&format!("DROP INDEX `{value}` IF EXISTS"), &[], OPTS).await;
            }
        }
        self.session.close().await;
    }
}

fn column(result: &StatementResult, name: &str) -> Vec<Cell> {
    let i = result.columns.iter().position(|c| c.name == name).unwrap_or_else(|| panic!("no column {name}"));
    result.rows.iter().map(|r| r[i].clone()).collect()
}

fn number(cell: &Cell) -> i64 {
    match cell {
        Cell::Number { value } => value.parse().unwrap(),
        other => panic!("not a number: {other:?}"),
    }
}

async fn seed(x: &Scratch) -> DbResult<()> {
    x.run(
        "CREATE (a:$PPerson {name: 'Ann', age: 41, big: 9007199254740993}), (b:$PPerson {name: 'Bob', age: 25}), \
         (c:$PPerson {name: 'Cid', age: 33}), (a)-[:$PKNOWS {since: 2020}]->(b), (b)-[:$PKNOWS {since: 2021}]->(c)",
    )
    .await?;
    x.run("CREATE INDEX idx_$P_name IF NOT EXISTS FOR (p:$PPerson) ON (p.name)").await?;
    x.run("CALL db.awaitIndexes(30)").await?;
    Ok(())
}

#[tokio::test]
async fn connects_and_reports_server() -> DbResult<()> {
    let cfg = require_db!();
    let info = Neo4jAdapter.test(&ConnectParams::new(cfg.clone(), Some("dataquery123".into()))).await?;
    assert!(info.version.starts_with("Neo4j 5."), "{}", info.version);
    assert!(info.version.contains("community"));
    assert_eq!((info.database.as_str(), info.user.as_str()), (DB, "neo4j"));
    let bad = Neo4jAdapter.test(&ConnectParams::new(cfg, Some("wrong-password".into()))).await.unwrap_err();
    assert_eq!(bad.kind, ErrorKind::Auth);
    assert!(!bad.message.contains("wrong-password"));
    Ok(())
}

#[tokio::test]
async fn explorer_lists_groups_labels_types_and_indexes() -> DbResult<()> {
    let cfg = require_db!();
    let x = Scratch::new(&cfg).await;
    seed(&x).await?;
    let dbs = x.session.list_children(None).await?;
    assert!(dbs.iter().any(|d| d.object.name == DB && d.current));
    assert_eq!(dbs.last().map(|d| d.object.name.as_str()), Some("system"));
    let groups = x.session.list_children(Some(&dbs[0].object)).await?;
    let names: Vec<&str> = groups.iter().map(|g| g.object.name.as_str()).collect();
    assert_eq!(names, [GROUP_LABELS, GROUP_RELATIONSHIPS, GROUP_INDEXES]);
    let labels = x.session.list_children(Some(&groups[0].object)).await?;
    let person = labels.iter().find(|n| n.object.name == x.label("Person")).expect("label");
    assert_eq!((person.object.kind, person.row_estimate), (NodeKind::Table, Some(3)));
    let types = x.session.list_children(Some(&groups[1].object)).await?;
    let knows = types.iter().find(|n| n.object.name == x.label("KNOWS")).expect("type");
    assert_eq!((knows.object.kind, knows.row_estimate), (NodeKind::View, Some(2)));
    let indexes = x.session.list_children(Some(&groups[2].object)).await?;
    let idx = indexes.iter().find(|n| n.object.name == format!("idx_{}_name", x.prefix)).expect("index");
    assert_eq!(idx.object.parent.as_deref(), Some(x.label("Person").as_str()));
    let found = x.session.search(&x.prefix.to_lowercase(), 10).await?;
    assert_eq!(found.len(), 2);
    x.drop().await;
    Ok(())
}

#[tokio::test]
async fn statements_bind_params_and_keep_values_exact() -> DbResult<()> {
    let cfg = require_db!();
    let x = Scratch::new(&cfg).await;
    seed(&x).await?;
    let params = [Some("Ann".to_owned()), Some("40".to_owned()), Some("[1, 2]".to_owned())];
    let r = x
        .session
        .execute(
            &format!(
                "MATCH p = (a:{0}Person {{name: $name}})-[k:{0}KNOWS]->(b) WHERE a.age > $age \
                 RETURN a.big AS big, a, k, p, $list AS list, size($list) AS n",
                x.prefix
            ),
            &params,
            OPTS,
        )
        .await?;
    assert_eq!(r.rows.len(), 1);
    assert_eq!(column(&r, "big")[0], Cell::number("9007199254740993"));
    assert_eq!(
        r.columns.iter().map(|c| c.type_name.as_str()).collect::<Vec<_>>(),
        ["Integer", "Node", "Relationship", "Path", "List", "Integer"]
    );
    let Cell::Json { value: node, .. } = &column(&r, "a")[0] else { panic!("node as json") };
    let node: serde_json::Value = serde_json::from_str(node).unwrap();
    assert_eq!(node["properties"]["big"], serde_json::json!(9_007_199_254_740_993_i64));
    assert_eq!(node["labels"][0], x.label("Person"));
    let Cell::Json { value: path, .. } = &column(&r, "p")[0] else { panic!("path as json") };
    assert_eq!(serde_json::from_str::<serde_json::Value>(path).unwrap().as_array().unwrap().len(), 3);
    assert_eq!(number(&column(&r, "n")[0]), 2);

    // More rows than the limit wait for fetch_more.
    let small = ExecOptions { row_limit: 10, timeout_secs: 30 };
    let r = x.session.execute("UNWIND range(1, 25) AS x RETURN x", &[], small).await?;
    assert_eq!((r.rows.len(), r.has_more), (10, true));
    let more = x.session.fetch_more(10).await?;
    assert_eq!((number(&more.rows[0][0]), more.has_more), (11, true));
    let last = x.session.fetch_more(10).await?;
    assert_eq!((last.rows.len(), last.has_more), (5, false));

    let err = x.session.execute("MATCH (n RETURN n", &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Syntax);
    assert!(err.position.is_some());
    x.drop().await;
    Ok(())
}

#[tokio::test]
async fn writes_report_counters_and_read_only_refuses() -> DbResult<()> {
    let cfg = require_db!();
    let x = Scratch::new(&cfg).await;
    let r = x.run("CREATE (:$PThing {a: 1, b: 2})").await?;
    assert_eq!(r.affected, Some(3));
    assert!(r.notices.iter().any(|n| n.contains("1 nodes created")), "{:?}", r.notices);
    let r = x.run("MATCH (a:$PThing), (b:$PThing) RETURN count(*)").await?;
    assert!(r.notices.iter().any(|n| n.to_lowercase().contains("cartesian")), "{:?}", r.notices);

    let ro = session(&ConnConfig { read_only: true, ..cfg.clone() }).await;
    let err = ro.execute(&format!("CREATE (:{}Thing)", x.prefix), &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission);
    // Statements the classifier misses are still refused by the server's read mode.
    let err = ro.execute(&format!("CALL {{ CREATE (:{}Thing) }}", x.prefix), &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission);
    assert_eq!(
        number(&ro.execute(&format!("MATCH (n:{}Thing) RETURN count(n)", x.prefix), &[], OPTS).await?.rows[0][0]),
        1
    );
    ro.close().await;
    x.drop().await;
    Ok(())
}

#[tokio::test]
async fn transactions_commit_and_roll_back() -> DbResult<()> {
    let cfg = require_db!();
    let x = Scratch::new(&cfg).await;
    let count = || async { number(&x.run("MATCH (n:$PTx) RETURN count(n)").await.unwrap().rows[0][0]) };
    x.run("BEGIN").await?;
    x.run("CREATE (:$PTx)").await?;
    assert_eq!(number(&x.run("MATCH (n:$PTx) RETURN count(n)").await?.rows[0][0]), 1);
    let other = session(&cfg).await;
    let outside = other.execute(&format!("MATCH (n:{}Tx) RETURN count(n)", x.prefix), &[], OPTS).await?;
    assert_eq!(number(&outside.rows[0][0]), 0, "uncommitted work is not visible elsewhere");
    x.run("ROLLBACK").await?;
    assert_eq!(count().await, 0);

    x.run("BEGIN").await?;
    x.run("CREATE (:$PTx)").await?;
    x.run("COMMIT").await?;
    assert_eq!(count().await, 1);
    assert_eq!(x.run("COMMIT").await.unwrap_err().kind, ErrorKind::InvalidInput);

    // A failing statement ends the transaction.
    x.run("BEGIN").await?;
    x.run("CREATE (:$PTx)").await?;
    assert!(x.run("RETURN 1/0").await.is_err());
    assert_eq!(x.run("COMMIT").await.unwrap_err().kind, ErrorKind::InvalidInput);
    assert_eq!(count().await, 1);

    // run_in_transaction binds $p1, $p2, …
    let statements = vec![
        Ok(dq_core::edit::BoundStatement {
            sql: format!("CREATE (:{}Tx {{v: $p1}})", x.prefix),
            params: vec![Some("5".into())],
            expect_rows: None,
        }),
        Ok(dq_core::edit::BoundStatement { sql: "RETURN 1/0".into(), params: vec![], expect_rows: None }),
    ];
    assert!(x.session.run_in_transaction(DB, &mut statements.into_iter()).await.is_err());
    assert_eq!(count().await, 1, "the whole transaction rolled back");
    other.close().await;
    x.drop().await;
    Ok(())
}

/// Every operator of a plan tree, depth first.
fn labels(n: &dq_core::explain::PlanNode, out: &mut Vec<String>) {
    out.push(n.label.clone());
    for c in &n.children {
        labels(c, out);
    }
}

#[tokio::test]
async fn explain_and_profile_plans() -> DbResult<()> {
    let cfg = require_db!();
    let x = Scratch::new(&cfg).await;
    seed(&x).await?;
    let sql = format!("MATCH (p:{}Person) WHERE p.name = 'Ann' RETURN p", x.prefix);
    let plan = x.session.explain(&sql, &[], false, 30).await?;
    let root = plan.plan.expect("plan");
    assert_eq!(root.label, "ProduceResults");
    assert!(!plan.analyzed);
    assert!(plan.raw.contains("Estimated Rows"));
    let mut ops = Vec::new();
    labels(&root, &mut ops);
    assert!(ops.iter().any(|o| o.contains("NodeIndexSeek")), "{ops:?}");
    let profile = x.session.explain(&sql, &[], true, 30).await?;
    assert!(profile.analyzed);
    assert_eq!(profile.plan.unwrap().actual_rows, Some(1.0));
    x.drop().await;
    Ok(())
}

#[tokio::test]
async fn documents_view_reads_nodes_and_relationships() -> DbResult<()> {
    let cfg = require_db!();
    let x = Scratch::new(&cfg).await;
    seed(&x).await?;
    let store = x.session.documents().expect("documents");
    let person = ObjectRef::new(NodeKind::Table, DB, None, x.label("Person"));
    let mut req = FindRequest::new(person.clone(), 2);
    req.sort = "n.name".into();
    let page = store.find(&req).await?;
    assert_eq!(page.documents.len(), 2);
    assert_eq!(page.next.as_deref(), Some("2"));
    let ann = &page.documents[0];
    assert!(ann.label.starts_with(&format!("(:{})", x.label("Person"))));
    let big = ann.fields.iter().find(|f| f.key == "big").unwrap();
    assert_eq!((big.type_label.as_str(), &big.value), ("Integer", &Cell::number("9007199254740993")));
    req.cursor = page.next;
    assert_eq!(store.find(&req).await?.documents.len(), 1);

    let mut filtered = FindRequest::new(person.clone(), 10);
    filtered.filter = "n.age > 30".into();
    filtered.projection = "n.name, n.age".into();
    filtered.sort = "n.age DESC".into();
    let docs = store.find(&filtered).await?.documents;
    let names: Vec<String> =
        docs.iter().map(|d| d.fields.iter().find(|f| f.key == "name").unwrap().value.to_text().unwrap()).collect();
    assert_eq!(names, ["Ann", "Cid"]);
    assert_eq!(store.count(&person, "n.age > 30").await?.unwrap().count, 2);
    assert_eq!(store.count(&person, "").await?.unwrap().count, 3);
    filtered.filter = "n.age > 1 SET n.age = 0".into();
    assert_eq!(store.find(&filtered).await.unwrap_err().kind, ErrorKind::InvalidInput);

    let knows = ObjectRef::new(NodeKind::View, DB, None, x.label("KNOWS"));
    let rels = store.find(&FindRequest::new(knows, 10)).await?.documents;
    assert_eq!(rels.len(), 2);
    assert!(rels[0].fields.iter().any(|f| f.key == "start") && rels[0].fields.iter().any(|f| f.key == "since"));

    let indexes = store.indexes(&person).await?;
    assert!(indexes.iter().any(|i| i.keys == "(name)" && i.props.iter().any(|(k, v)| k == "type" && v == "RANGE")));
    assert!(store.insert(&person, "{}").await.is_err());
    assert!(store.find_statement(&req)?.contains("SKIP 2 LIMIT 2"));
    x.drop().await;
    Ok(())
}

#[tokio::test]
async fn structure_and_definitions() -> DbResult<()> {
    let cfg = require_db!();
    let x = Scratch::new(&cfg).await;
    seed(&x).await?;
    let person = ObjectRef::new(NodeKind::Table, DB, None, x.label("Person"));
    let s = x.session.structure(&person).await?;
    let cols: Vec<(&str, &str)> = s.columns.iter().map(|c| (c.name.as_str(), c.data_type.as_str())).collect();
    assert_eq!(cols, [("elementId", "String"), ("age", "Integer"), ("big", "Integer"), ("name", "String")]);
    assert_eq!((s.row_identity.as_slice(), s.row_estimate), (["elementId".to_owned()].as_slice(), Some(3)));
    assert!(s.indexes.iter().any(|i| i.definition.starts_with("CREATE RANGE INDEX")));
    let ddl = x.session.ddl(&person).await?;
    assert!(ddl.contains(&format!("idx_{}_name", x.prefix)), "{ddl}");
    let knows = ObjectRef::new(NodeKind::View, DB, None, x.label("KNOWS"));
    let r = x.session.structure(&knows).await?;
    assert_eq!(r.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["elementId", "start", "end", "since"]);
    assert!(x.session.ddl(&knows).await?.starts_with("// No indexes"));
    let deps = x.session.dependencies(&person).await?;
    assert!(deps.uses.iter().any(|d| d.object.name == x.label("KNOWS")), "{deps:?}");
    x.drop().await;
    Ok(())
}

#[tokio::test]
async fn export_and_import() -> DbResult<()> {
    let cfg = require_db!();
    let x = Scratch::new(&cfg).await;
    seed(&x).await?;
    let dir = tempfile::tempdir().unwrap();
    let none = TransferControl::none();
    let person = ObjectRef::new(NodeKind::Table, DB, None, x.label("Person"));
    let json = dir.path().join("people.json");
    let mut req = FindRequest::new(person.clone(), 100);
    req.filter = "n.age > 30".into();
    let report = ops::export(
        &*x.session,
        &ExportSource::Documents { request: req },
        ExportFormat::Json,
        &ExportOptions::default(),
        &json,
        &none,
    )
    .await?;
    assert_eq!(report.rows, 2);
    assert!(std::fs::read_to_string(&json).unwrap().contains("9007199254740993"));
    let csv = dir.path().join("q.csv");
    let sql = format!("MATCH (p:{}Person) RETURN p.name AS name ORDER BY name", x.prefix);
    let report = ops::export(
        &*x.session,
        &ExportSource::Query { sql },
        ExportFormat::Csv,
        &ExportOptions::default(),
        &csv,
        &none,
    )
    .await?;
    assert_eq!(report.rows, 3);

    let input = dir.path().join("in.csv");
    std::fs::write(&input, "name,age,vip\nDee,52,true\nEve,19,false\n").unwrap();
    let source = ImportSource::Csv { path: input.to_string_lossy().into_owned(), options: CsvOptions::default() };
    let mapping: Vec<ImportMapping> = [("name", 0), ("age", 1), ("vip", 2)]
        .iter()
        .map(|(c, i)| ImportMapping { column: (*c).into(), field: FieldRef::Index(*i) })
        .collect();
    let report = ops::import(&*x.session, &person, &source, &mapping, &ImportOptions::default(), &none).await?;
    assert_eq!(report.rows, 2, "{report:?}");
    let r = x.run("MATCH (p:$PPerson {name: 'Dee'}) RETURN p.age AS age, p.vip AS vip").await?;
    assert_eq!(
        (column(&r, "age")[0].clone(), column(&r, "vip")[0].clone()),
        (Cell::number("52"), Cell::Bool { value: true })
    );
    x.drop().await;
    Ok(())
}

const SLOW: &str = "UNWIND range(1, 300000000) AS x WITH x WHERE x < 0 RETURN count(x)";

#[tokio::test]
async fn cancel_and_timeout_stop_the_server_transaction() -> DbResult<()> {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let runner = s.clone();
    let started = Instant::now();
    let task = tokio::spawn(async move { runner.execute(SLOW, &[], OPTS).await });
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(s.cancel().await?);
    let err = task.await.unwrap().unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(5));
    // The server transaction is gone, and the session still works.
    let left = s
        .execute(
            "SHOW TRANSACTIONS YIELD currentQuery WHERE currentQuery CONTAINS $q RETURN count(*)",
            &[Some("\"range(1, 300000000)\"".into())],
            OPTS,
        )
        .await?;
    assert_eq!(number(&left.rows[0][0]), 0);
    assert_eq!(number(&s.execute("RETURN 1", &[], OPTS).await?.rows[0][0]), 1);

    let err = s.execute(SLOW, &[], ExecOptions { row_limit: 10, timeout_secs: 1 }).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout);
    s.close().await;
    Ok(())
}

#[tokio::test]
async fn monitor_lists_and_terminates_transactions() -> DbResult<()> {
    let cfg = require_db!();
    let s = session(&cfg).await;
    let busy = session(&cfg).await;
    // Its own query, so the cancel test's transaction is never the one terminated here.
    let busy_sql = SLOW.replace("300000000", "300000007");
    let task = tokio::spawn(async move { busy.execute(&busy_sql, &[], OPTS).await });
    let monitor = s.monitor().expect("monitor");
    let mut pid = None;
    for _ in 0..30 {
        let snap = monitor.snapshot(1).await?;
        pid =
            snap.processes.iter().find(|p| p.query.as_deref().is_some_and(|q| q.contains("300000007"))).map(|p| p.pid);
        if pid.is_some() {
            assert!(snap.active_queries.unwrap() >= 1);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let pid = pid.expect("the running transaction is listed");
    assert!(monitor.kill(pid, true).await?);
    assert!(task.await.unwrap().is_err());
    assert!(!monitor.kill(999_999_999, true).await?);
    assert!(!monitor.server_settings(Some("heap")).await?.is_empty());
    assert!(monitor.largest_tables(DB, 5).await?.len() <= 5);
    s.close().await;
    Ok(())
}
