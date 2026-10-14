//! Integration tests against a real Elasticsearch. Set
//! `DATAQUERY_TEST_ELASTICSEARCH=localhost:59200` (user `elastic`, password
//! `dataquery`, plain HTTP) to run them; they skip otherwise. Each test uses
//! its own indices and deletes them.

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
use dq_elasticsearch::ElasticsearchAdapter;

const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };

fn config() -> Option<ConnConfig> {
    let target = std::env::var("DATAQUERY_TEST_ELASTICSEARCH").ok()?;
    let (host, port) = target.rsplit_once(':')?;
    Some(ConnConfig {
        engine: Engine::Elasticsearch,
        host: host.into(),
        port: port.parse().ok()?,
        database: String::new(),
        username: "elastic".into(),
        ssl_mode: SslMode::Disable,
        connect_timeout_secs: 10,
        ..ConnConfig::default()
    })
}

macro_rules! require_es {
    () => {
        match config() {
            Some(c) => c,
            None => {
                eprintln!("skipped: set DATAQUERY_TEST_ELASTICSEARCH");
                return Ok(());
            }
        }
    };
}

async fn session(cfg: &ConnConfig) -> Arc<dyn Session> {
    ElasticsearchAdapter.connect(&ConnectParams::new(cfg.clone(), Some("dataquery".into()))).await.unwrap()
}

/// A uniquely named index, deleted on drop of the guard's `drop` call.
struct Index {
    name: String,
    session: Arc<dyn Session>,
}

impl Index {
    async fn new(session: &Arc<dyn Session>, mapping: &str) -> DbResult<Self> {
        let name = format!("dq-test-{}", uuid::Uuid::new_v4().simple());
        session.execute(&format!("PUT /{name}\n{mapping}"), &[], OPTS).await?;
        Ok(Self { name, session: session.clone() })
    }

    fn object(&self) -> ObjectRef {
        ObjectRef::new(NodeKind::Table, self.session.info().database.clone(), None, self.name.clone())
    }

    async fn bulk(&self, docs: impl Iterator<Item = String>) -> DbResult<()> {
        let mut body = String::new();
        for doc in docs {
            body.push_str("{\"index\":{}}\n");
            body.push_str(&doc);
            body.push('\n');
        }
        self.session.execute(&format!("POST /{}/_bulk?refresh=true\n{body}", self.name), &[], OPTS).await?;
        Ok(())
    }

    async fn drop(self) {
        let _ = self.session.execute(&format!("DELETE /{}", self.name), &[], OPTS).await;
    }
}

const MAPPING: &str = r#"{ "settings": { "number_of_replicas": 0 }, "mappings": { "properties": {
  "n": { "type": "long" }, "s": { "type": "keyword" }, "t": { "type": "text" },
  "c": { "properties": { "a": { "type": "keyword" } } }, "created": { "type": "date" } } } }"#;

async fn seeded(session: &Arc<dyn Session>) -> DbResult<Index> {
    let index = Index::new(session, MAPPING).await?;
    index
        .bulk(
            [
                r#"{"n": 9007199254740993, "s": "paid", "t": "first order", "c": {"a": "x"}, "created": "2026-10-01"}"#,
                r#"{"n": 2, "s": "pending", "t": "second order", "c": {"a": "y"}, "created": "2026-10-02"}"#,
                r#"{"n": 3, "s": "paid", "t": "third", "created": "2026-10-03"}"#,
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .await?;
    Ok(index)
}

fn column<'a>(result: &'a dq_core::result::StatementResult, name: &str) -> Vec<&'a Cell> {
    let i = result.columns.iter().position(|c| c.name == name).unwrap_or_else(|| panic!("no column {name}"));
    result.rows.iter().map(|r| &r[i]).collect()
}

#[tokio::test]
async fn connects_and_reports_the_cluster() -> DbResult<()> {
    let cfg = require_es!();
    let info = ElasticsearchAdapter.test(&ConnectParams::new(cfg.clone(), Some("dataquery".into()))).await?;
    assert!(info.version.starts_with("Elasticsearch 8."), "{}", info.version);
    assert_eq!(info.user, "elastic");
    assert!(!info.database.is_empty());
    let bad = ElasticsearchAdapter.test(&ConnectParams::new(cfg, Some("wrong".into()))).await.unwrap_err();
    assert_eq!(bad.kind, ErrorKind::Auth);
    Ok(())
}

#[tokio::test]
async fn explorer_lists_indices_and_aliases() -> DbResult<()> {
    let cfg = require_es!();
    let s = session(&cfg).await;
    let index = seeded(&s).await?;
    let alias = format!("{}-alias", index.name);
    s.execute(
        &format!("POST _aliases\n{{\"actions\":[{{\"add\":{{\"index\":\"{}\",\"alias\":\"{alias}\"}}}}]}}", index.name),
        &[],
        OPTS,
    )
    .await?;
    let dbs = s.list_children(None).await?;
    assert_eq!(dbs.len(), 1);
    let children = s.list_children(Some(&dbs[0].object)).await?;
    let table = children.iter().find(|n| n.object.name == index.name).unwrap();
    assert_eq!((table.object.kind, table.row_estimate), (NodeKind::Table, Some(3)));
    assert!(children.iter().any(|n| n.object.name == alias && n.object.kind == NodeKind::View));
    let found = s.search(&index.name[..12], 50).await?;
    assert!(found.iter().any(|n| n.object.name == index.name));
    let deps = s.dependencies(&ObjectRef::new(NodeKind::View, "", None, alias)).await?;
    assert_eq!(deps.uses[0].object.name, index.name);
    index.drop().await;
    Ok(())
}

#[tokio::test]
async fn editor_requests_become_grids() -> DbResult<()> {
    let cfg = require_es!();
    let s = session(&cfg).await;
    let index = seeded(&s).await?;
    let r =
        s.execute(&format!("GET /{}/_search\n{{ \"sort\": [{{ \"n\": \"desc\" }}] }}", index.name), &[], OPTS).await?;
    assert_eq!(r.command, "GET _search");
    assert_eq!(r.columns[..3].iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["_index", "_id", "_score"]);
    assert_eq!(column(&r, "n")[0], &Cell::number("9007199254740993"));
    assert!(matches!(column(&r, "c")[0], Cell::Json { .. }));
    assert!(r.notices[0].starts_with("3 hits"), "{:?}", r.notices);
    assert_eq!(r.rows.len(), 3);

    // Rows beyond the row limit wait for fetch_more.
    let small = ExecOptions { row_limit: 2, timeout_secs: 30 };
    let r = s.execute(&format!("GET /{}/_search", index.name), &[], small).await?;
    assert!(r.has_more && r.rows.len() == 2);
    let more = s.fetch_more(10).await?;
    assert_eq!((more.rows.len(), more.has_more), (1, false));

    let cat = s.execute(&format!("GET _cat/indices/{}?format=json", index.name), &[], OPTS).await?;
    assert!(cat.columns.iter().any(|c| c.name == "index"));
    let text = s.execute(&format!("GET _cat/indices/{}", index.name), &[], OPTS).await?;
    assert_eq!((text.columns[0].name.as_str(), text.rows.len()), ("response", 1));
    let sql = s
        .execute(
            &format!("POST _sql?format=json\n{{\"query\": \"SELECT n, s FROM \\\"{}\\\" ORDER BY n\"}}", index.name),
            &[],
            OPTS,
        )
        .await?;
    assert_eq!(sql.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["n", "s"]);
    assert_eq!(sql.rows[0][0], Cell::number("2"));
    let deleted = s
        .execute(
            &format!(
                "POST /{}/_delete_by_query?refresh=true\n{{\"query\":{{\"term\":{{\"s\":\"pending\"}}}}}}",
                index.name
            ),
            &[],
            OPTS,
        )
        .await?;
    assert_eq!(deleted.affected, Some(1));
    let err = s.execute("GET /dq-test-no-such-index/_search", &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotFound);
    let bad = s
        .execute(&format!("GET /{}/_search\n{{\"query\": {{\"nope\": {{}}}}}}", index.name), &[], OPTS)
        .await
        .unwrap_err();
    assert!(matches!(bad.kind, ErrorKind::Syntax | ErrorKind::InvalidInput), "{bad:?}");
    index.drop().await;
    Ok(())
}

#[tokio::test]
async fn read_only_refuses_writes() -> DbResult<()> {
    let cfg = require_es!();
    let s = session(&cfg).await;
    let index = seeded(&s).await?;
    let ro = session(&ConnConfig { read_only: true, ..cfg }).await;
    let err = ro.execute(&format!("POST /{}/_doc\n{{\"n\": 1}}", index.name), &[], OPTS).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission);
    assert_eq!(ro.execute(&format!("POST /{}/_search", index.name), &[], OPTS).await?.rows.len(), 3);
    let store = ro.documents().unwrap();
    let err = store.insert_rows(&index.object(), &[], vec![vec![]]).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission);
    index.drop().await;
    Ok(())
}

#[tokio::test]
async fn documents_view_reads() -> DbResult<()> {
    let cfg = require_es!();
    let s = session(&cfg).await;
    let index = seeded(&s).await?;
    let store = s.documents().unwrap();
    assert_eq!(store.features().language, "Query DSL");
    let mut req = FindRequest::new(index.object(), 10);
    req.filter = r#"{ "term": { "s": "paid" } }"#.into();
    req.sort = r#"{ "n": "asc" }"#.into();
    let page = store.find(&req).await?;
    assert_eq!(page.documents.len(), 2);
    assert!(page.next.is_none());
    let first = &page.documents[0];
    assert_eq!(first.fields.iter().find(|f| f.key == "n").unwrap().type_label, "long");
    assert!(first.text.contains("\"s\""));

    req.filter = "s:paid AND n:<5".into();
    req.projection = "\"n\"".into();
    let page = store.find(&req).await?;
    assert_eq!(page.documents.len(), 1);
    assert!(page.documents[0].fields.iter().all(|f| f.key == "n"));

    let mut paged = FindRequest::new(index.object(), 1);
    paged.sort = "n asc".into();
    let p1 = store.find(&paged).await?;
    paged.cursor = p1.next.clone();
    let p2 = store.find(&paged).await?;
    assert_ne!(p1.documents[0].key, p2.documents[0].key);
    assert!(p2.next.is_some());

    let count = store.count(&index.object(), "s:paid").await?.unwrap();
    assert_eq!((count.count, count.exact), (2, true));
    let indexes = store.indexes(&index.object()).await?;
    assert!(indexes[0].keys.contains("shards"), "{:?}", indexes[0]);
    assert!(store.insert(&index.object(), "{}").await.is_err());
    let statement = store.find_statement(&req)?;
    assert!(statement.starts_with(&format!("POST /{}/_search", index.name)));
    assert_eq!(s.execute(&statement, &[], OPTS).await?.rows.len(), 1);
    index.drop().await;
    Ok(())
}

#[tokio::test]
async fn structure_ddl_and_plans() -> DbResult<()> {
    let cfg = require_es!();
    let s = session(&cfg).await;
    let index = seeded(&s).await?;
    let st = s.structure(&index.object()).await?;
    assert_eq!(st.columns[0].name, "_id");
    let n = st.columns.iter().find(|c| c.name == "n").unwrap();
    assert!(n.numeric && n.data_type == "long");
    assert!(st.columns.iter().any(|c| c.name == "c.a" && c.data_type == "keyword"));
    assert_eq!(st.row_estimate, Some(3));
    let ddl = s.ddl(&index.object()).await?;
    assert!(ddl.starts_with(&format!("PUT /{}", index.name)) && ddl.contains("\"mappings\"") && !ddl.contains("uuid"));
    let model = s.schema_model(&s.info().database, None).await?;
    assert!(model.tables.iter().any(|t| t.object.name == index.name && t.columns.len() >= 4));

    let search = format!("GET /{}/_search\n{{ \"query\": {{ \"term\": {{ \"s\": \"paid\" }} }} }}", index.name);
    let profiled = s.explain(&search, &[], true, 30).await?;
    let root = profiled.plan.unwrap();
    assert_eq!(root.label, "Search");
    assert!(!root.children[0].children.is_empty());
    assert!(profiled.analyzed);
    let validated = s.explain(&search, &[], false, 30).await?;
    let plan = validated.plan.unwrap();
    assert!(plan.children[0].props[0].1.contains("s:paid"), "{plan:?}");
    assert!(s.explain("GET _cat/indices", &[], false, 30).await.is_err());
    index.drop().await;
    Ok(())
}

#[tokio::test]
async fn export_pages_every_hit_and_import_adds_fields() -> DbResult<()> {
    let cfg = require_es!();
    let s = session(&cfg).await;
    let index = Index::new(
        &s,
        r#"{ "settings": { "number_of_replicas": 0 }, "mappings": { "properties": { "n": { "type": "long" } } } }"#,
    )
    .await?;
    index.bulk((0..1_250).map(|i| format!("{{\"n\": {i}, \"s\": \"v{}\"}}", i % 3))).await?;
    let dir = tempfile::tempdir().unwrap();
    let none = TransferControl::none();
    let csv = dir.path().join("all.csv");
    let source = ExportSource::Table { object: index.object(), rows: RowFilter::default(), sort: None };
    let report = ops::export(&*s, &source, ExportFormat::Csv, &ExportOptions::default(), &csv, &none).await?;
    assert_eq!(report.rows, 1_250);
    let mut filtered = FindRequest::new(index.object(), 50);
    filtered.filter = "s:v1".into();
    let json = dir.path().join("v1.json");
    let report = ops::export(
        &*s,
        &ExportSource::Documents { request: filtered },
        ExportFormat::Json,
        &ExportOptions::default(),
        &json,
        &none,
    )
    .await?;
    assert_eq!(report.rows, 417);

    let input = dir.path().join("in.csv");
    std::fs::write(&input, "n,label,flag\n5000,new,true\n5001,other,false\n").unwrap();
    let source = ImportSource::Csv { path: input.to_string_lossy().into_owned(), options: CsvOptions::default() };
    let mapping: Vec<ImportMapping> = [("n", 0), ("label", 1), ("flag", 2)]
        .iter()
        .map(|(c, i)| ImportMapping { column: (*c).into(), field: FieldRef::Index(*i) })
        .collect();
    let report = ops::import(&*s, &index.object(), &source, &mapping, &ImportOptions::default(), &none).await?;
    assert_eq!(report.rows, 2, "{report:?}");
    let back = s
        .execute(&format!("GET /{}/_search\n{{\"query\":{{\"term\":{{\"label\":\"new\"}}}}}}", index.name), &[], OPTS)
        .await?;
    assert_eq!(column(&back, "n")[0], &Cell::number("5000"));
    assert_eq!(column(&back, "flag")[0], &Cell::Bool { value: true });
    index.drop().await;
    Ok(())
}

#[tokio::test]
async fn monitor_and_kill() -> DbResult<()> {
    let cfg = require_es!();
    let s = session(&cfg).await;
    let index = seeded(&s).await?;
    let monitor = s.monitor().unwrap();
    let snap = monitor.snapshot(5).await?;
    assert!(snap.version.starts_with("Elasticsearch"));
    assert!(snap.replication.is_some() && snap.unavailable.is_empty(), "{:?}", snap.unavailable);
    let settings = monitor.server_settings(Some("cluster.name")).await?;
    assert!(settings.iter().any(|x| x.name == "cluster.name"));
    let largest = monitor.largest_tables(&s.info().database, 500).await?;
    assert!(largest.iter().any(|o| o.object.name == index.name));
    assert!(!monitor.kill(987_654_321, false).await?);
    index.drop().await;
    Ok(())
}

#[tokio::test]
async fn cancel_stops_a_slow_search() -> DbResult<()> {
    let cfg = require_es!();
    let s = session(&cfg).await;
    let index = Index::new(&s, r#"{ "settings": { "number_of_replicas": 0 } }"#).await?;
    index.bulk((0..12_000).map(|i| format!("{{\"n\": {i}}}"))).await?;
    // About 0.3 ms of script work per document: several seconds in all.
    let slow = format!(
        "POST /{}/_search\n{{\"size\": 0, \"query\": {{\"script\": {{\"script\": {{\"source\": \"long s = 0; for (int i = 0; i < 900000; i++) {{ s += i; }} return s > 0;\"}}}}}}}}",
        index.name
    );
    let runner = s.clone();
    let started = Instant::now();
    let task =
        tokio::spawn(async move { runner.execute(&slow, &[], ExecOptions { row_limit: 10, timeout_secs: 60 }).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(s.cancel().await?);
    let outcome = task.await.unwrap();
    assert_eq!(outcome.unwrap_err().kind, ErrorKind::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(3), "cancel took {:?}", started.elapsed());
    // Nothing is running any more, and the session still works.
    assert!(!s.cancel().await?);
    assert_eq!(s.execute(&format!("GET /{}/_count", index.name), &[], OPTS).await?.rows.len(), 1);

    let timeout = format!(
        "POST /{}/_search\n{{\"size\": 0, \"query\": {{\"script\": {{\"script\": {{\"source\": \"long s = 0; for (int i = 0; i < 900000; i++) {{ s += i; }} return s > 0;\"}}}}}}}}",
        index.name
    );
    let err = s.execute(&timeout, &[], ExecOptions { row_limit: 10, timeout_secs: 1 }).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout);
    index.drop().await;
    Ok(())
}
