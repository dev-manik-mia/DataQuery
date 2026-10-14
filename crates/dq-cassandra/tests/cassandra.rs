//! Integration tests against Cassandra 5 and `ScyllaDB` from `docker-compose.yml`.
//!
//! `DATAQUERY_TEST_CASSANDRA=localhost:59042` (user cassandra / cassandra) and
//! `DATAQUERY_TEST_SCYLLA=localhost:59043` (no auth). Each test runs on every
//! server that is configured, in its own keyspace, which it drops afterwards.

use std::sync::Arc;

use dq_cassandra::CassandraAdapter;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::edit::{ColumnValue, RowChange};
use dq_core::engine::Engine;
use dq_core::node::{GROUP_TYPES, NodeKind, ObjectRef};
use dq_core::ops::{self, ImportMapping};
use dq_core::result::{ExecOptions, StatementResult};
use dq_core::schema::KeyRoleKind;
use dq_core::sql::ddl::{ColumnDef, SchemaChange, statements};
use dq_core::sql::{CqlDialect, StatementKind};
use dq_core::table::{RowFilter, TablePageRequest};
use dq_core::transfer::{
    CsvOptions, ExportFormat, ExportOptions, ExportSource, FieldRef, ImportOptions, ImportSource, TransferControl,
};
use dq_core::value::Cell;
use dq_core::{DbResult, ErrorKind};

const OPTS: ExecOptions = ExecOptions { row_limit: 100, timeout_secs: 30 };

struct Server {
    label: &'static str,
    config: ConnConfig,
    password: Option<String>,
    adapter: CassandraAdapter,
}

fn servers() -> Vec<Server> {
    let make = |var: &str, label: &'static str, engine: Engine, user: &str| {
        let addr = std::env::var(var).ok()?;
        let (host, port) = addr.rsplit_once(':')?;
        Some(Server {
            label,
            config: ConnConfig {
                engine,
                host: host.into(),
                port: port.parse().ok()?,
                database: String::new(),
                username: user.into(),
                ssl_mode: SslMode::Disable,
                connect_timeout_secs: 30,
                ..ConnConfig::default()
            },
            password: (!user.is_empty()).then(|| "cassandra".to_owned()),
            adapter: if engine == Engine::Cassandra { CassandraAdapter::CASSANDRA } else { CassandraAdapter::SCYLLA },
        })
    };
    [
        make("DATAQUERY_TEST_CASSANDRA", "cassandra", Engine::Cassandra, "cassandra"),
        make("DATAQUERY_TEST_SCYLLA", "scylla", Engine::Scylladb, ""),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// A session with its own keyspace holding the seed schema and rows.
struct Fixture {
    session: Arc<dyn Session>,
    ks: String,
    label: &'static str,
}

impl Fixture {
    async fn new(server: &Server) -> DbResult<Self> {
        let session =
            server.adapter.connect(&ConnectParams::new(server.config.clone(), server.password.clone())).await?;
        let ks = format!("dq_test_{}", uuid::Uuid::new_v4().simple().to_string().get(..12).unwrap_or_default());
        let f = Self { session, ks, label: server.label };
        f.run(&format!(
            "CREATE KEYSPACE {} WITH replication = {{'class': 'SimpleStrategy', 'replication_factor': 1}}",
            f.ks
        ))
        .await?;
        let seed = include_str!("../../../docker/cassandra-seed.cql").replace("telemetry", &f.ks);
        for statement in seed.split(';').map(str::trim).filter(|s| !s.is_empty() && !s.starts_with("--")) {
            if statement.starts_with("CREATE KEYSPACE") {
                continue;
            }
            f.run(statement).await?;
        }
        Ok(f)
    }

    async fn run(&self, cql: &str) -> DbResult<StatementResult> {
        self.session.execute(cql, &[], OPTS).await
    }

    fn table(&self, name: &str) -> ObjectRef {
        ObjectRef::new(NodeKind::Table, self.ks.clone(), None, name)
    }

    async fn drop(self) {
        let _ = self.run(&format!("DROP KEYSPACE IF EXISTS {}", self.ks)).await;
        self.session.close().await;
    }
}

fn column(result: &StatementResult, name: &str) -> Vec<Cell> {
    let i = result.columns.iter().position(|c| c.name == name).unwrap_or_else(|| panic!("no column {name}"));
    result.rows.iter().map(|r| r[i].clone()).collect()
}

fn text(c: &Cell) -> String {
    c.to_text().unwrap_or_default()
}

/// Runs `body` for every configured server.
macro_rules! each_server {
    (|$f:ident| $body:block) => {{
        for server in servers() {
            let $f = Fixture::new(&server).await.unwrap_or_else(|e| panic!("{}: {e:?}", server.label));
            let outcome: DbResult<()> = async { $body }.await;
            let label = $f.label;
            $f.drop().await;
            outcome.unwrap_or_else(|e| panic!("{label}: {e:?}"));
        }
    }};
}

#[tokio::test]
async fn connects_and_detects_the_server() {
    for server in servers() {
        let info =
            server.adapter.test(&ConnectParams::new(server.config.clone(), server.password.clone())).await.unwrap();
        let expected = if server.label == "scylla" { "ScyllaDB 20" } else { "Cassandra 5" };
        assert!(info.version.starts_with(expected), "{}", info.version);
        assert_eq!(server.adapter.engine(), server.config.engine);
    }
}

#[tokio::test]
async fn explorer_lists_keyspaces_tables_views_and_types() {
    each_server!(|f| {
        // Materialized views are off by default on Cassandra 5; list one when the server allows it.
        let view = f
            .run(&format!(
                "CREATE MATERIALIZED VIEW {ks}.devices_by_name AS SELECT * FROM {ks}.devices WHERE name IS NOT NULL AND device_id IS NOT NULL PRIMARY KEY (name, device_id)",
                ks = f.ks
            ))
            .await
            .is_ok();
        let keyspaces = f.session.list_children(None).await?;
        let pos = |n: &str| keyspaces.iter().position(|k| k.object.name == n);
        assert!(pos(&f.ks).is_some());
        assert!(pos(&f.ks) < pos("system_schema"), "user keyspaces first");
        let ks = ObjectRef::new(NodeKind::Database, f.ks.clone(), None, f.ks.clone());
        let children = f.session.list_children(Some(&ks)).await?;
        let names: Vec<(NodeKind, &str)> = children.iter().map(|n| (n.object.kind, n.object.name.as_str())).collect();
        assert!(
            names.contains(&(NodeKind::Table, "readings_by_device")) && names.contains(&(NodeKind::Table, "devices"))
        );
        assert_eq!(names.contains(&(NodeKind::MaterializedView, "devices_by_name")), view);
        let types = children.iter().find(|n| n.object.name == GROUP_TYPES).expect("types group");
        let udts = f.session.list_children(Some(&types.object)).await?;
        assert_eq!(
            udts.iter().map(|n| (n.object.kind, n.object.name.as_str())).collect::<Vec<_>>(),
            [(NodeKind::Type, "location")]
        );
        let ddl = f.session.ddl(&udts[0].object).await?;
        assert!(ddl.contains("CREATE TYPE") && ddl.contains("lat double"), "{ddl}");
        let found = f.session.search("READINGS", 10).await?;
        assert!(found.iter().any(|n| n.object.database == f.ks));
        Ok(())
    });
}

#[tokio::test]
async fn values_are_exact_and_results_page() {
    each_server!(|f| {
        let r = f
            .run(&format!(
                "SELECT * FROM {}.readings_by_device WHERE device_id = 'dev-88f2' AND bucket = '2026-10-14'",
                f.ks
            ))
            .await?;
        assert_eq!(r.rows.len(), 2);
        assert_eq!(text(&column(&r, "big")[0]), "9007199254740993");
        assert_eq!(text(&column(&r, "exact")[0]), "120.50");
        assert_eq!(column(&r, "raw")[0], Cell::Binary { length: 2, head_hex: "cafe".into() });
        assert_eq!(column(&r, "ts")[0], Cell::Temporal { value: "2026-10-14T09:59:12.000Z".into(), zoned: true });
        assert_eq!(column(&r, "bucket")[0], Cell::Temporal { value: "2026-10-14".into(), zoned: false });
        assert_eq!(column(&r, "big")[1], Cell::Null);
        let big = r.columns.iter().find(|c| c.name == "big").unwrap();
        assert!(big.numeric && big.type_name == "varint");

        let d = f.run(&format!("SELECT location, tags, attrs FROM {}.devices", f.ks)).await?;
        let loc: serde_json::Value = serde_json::from_str(&text(&d.rows[0][0])).unwrap();
        assert_eq!(loc["label"], "Paris");
        // Result metadata does not say whether a UDT is frozen.
        assert!(d.columns[0].type_name.ends_with("location"), "{}", d.columns[0].type_name);
        assert_eq!(text(&d.rows[0][1]), r#"["eu","heat"]"#);
        assert_eq!(text(&d.rows[0][2]), r#"{"fw":"1.2"}"#);

        f.run(&format!("CREATE TABLE {}.many (p int, c int, v text, PRIMARY KEY (p, c))", f.ks)).await?;
        for c in 0..250 {
            f.run(&format!("INSERT INTO {}.many (p, c, v) VALUES (1, {c}, 'x')", f.ks)).await?;
        }
        let first = f.run(&format!("SELECT c FROM {}.many WHERE p = 1", f.ks)).await?;
        assert_eq!((first.rows.len(), first.has_more), (100, true));
        let second = f.session.fetch_more(100).await?;
        assert_eq!((second.rows.len(), second.has_more), (100, true));
        assert_eq!(text(&second.rows[0][0]), "100");
        let third = f.session.fetch_more(100).await?;
        assert_eq!((third.rows.len(), third.has_more), (50, false));
        assert!(f.session.fetch_more(10).await.is_err());
        Ok(())
    });
}

#[tokio::test]
async fn parameters_take_the_prepared_types() {
    each_server!(|f| {
        let sql = format!("SELECT metric, value FROM {}.readings_by_device WHERE device_id = ? AND bucket = ?", f.ks);
        let r = f.session.execute(&sql, &[Some("dev-88f2".into()), Some("2026-10-14".into())], OPTS).await?;
        assert_eq!(r.rows.len(), 2);
        let bad = f.session.execute(&sql, &[Some("dev-88f2".into()), Some("tomorrow".into())], OPTS).await.unwrap_err();
        assert!(bad.message.contains("Parameter 2 (date)"), "{}", bad.message);
        let count = f.session.execute(&sql, &[Some("dev-88f2".into())], OPTS).await.unwrap_err();
        assert_eq!(count.kind, ErrorKind::InvalidInput);
        let insert = format!(
            "INSERT INTO {}.devices (device_id, tags, attrs, location, installed) VALUES (?, ?, ?, ?, ?)",
            f.ks
        );
        f.session
            .execute(
                &insert,
                &[
                    Some("dev-2".into()),
                    Some(r#"["a","b"]"#.into()),
                    Some(r#"{"k":"v"}"#.into()),
                    Some(r#"{"lat": 1.5, "label": "Lyon"}"#.into()),
                    Some("2026-10-14T08:00:00Z".into()),
                ],
                OPTS,
            )
            .await?;
        let d = f.run(&format!("SELECT installed, location FROM {}.devices WHERE device_id = 'dev-2'", f.ks)).await?;
        assert_eq!(text(&d.rows[0][0]), "2026-10-14T08:00:00.000Z");
        assert!(text(&d.rows[0][1]).contains("Lyon"));
        Ok(())
    });
}

#[tokio::test]
async fn key_roles_and_structure() {
    each_server!(|f| {
        let table = f.table("readings_by_device");
        let roles = f.session.key_roles(&table).await?;
        let shape: Vec<(&str, KeyRoleKind, bool)> =
            roles.iter().map(|r| (r.column.as_str(), r.role, r.descending)).collect();
        assert_eq!(
            shape,
            [
                ("device_id", KeyRoleKind::Partition, false),
                ("bucket", KeyRoleKind::Partition, false),
                ("ts", KeyRoleKind::Clustering, true)
            ]
        );
        let s = f.session.structure(&table).await?;
        assert_eq!(s.row_identity, ["device_id", "bucket", "ts"]);
        assert_eq!(s.constraints[0].definition, "PRIMARY KEY ((device_id, bucket), ts DESC)");
        assert_eq!(&s.columns[0].name, "device_id");
        assert!(s.columns.iter().find(|c| c.name == "value").is_some_and(|c| c.numeric && c.nullable));
        let ddl = f.session.ddl(&table).await?;
        assert!(ddl.contains("CREATE TABLE") && ddl.contains("CLUSTERING ORDER BY"), "{ddl}");
        let model = f.session.schema_model(&f.ks, None).await?;
        assert!(model.tables.iter().any(|t| t.object.name == "devices"));
        Ok(())
    });
}

#[tokio::test]
async fn grid_edits_are_lightweight_transactions() {
    each_server!(|f| {
        let table = f.table("readings_by_device");
        let key = |ts: &str| {
            vec![
                ColumnValue { column: "device_id".into(), value: Some("dev-88f2".into()) },
                ColumnValue { column: "bucket".into(), value: Some("2026-10-14".into()) },
                ColumnValue { column: "ts".into(), value: Some(ts.into()) },
            ]
        };
        let update = |ts: &str| RowChange::Update {
            key: key(ts),
            values: vec![ColumnValue { column: "metric".into(), value: Some("renamed".into()) }],
        };
        let report = ops::apply_changes(&*f.session, &table, &[update("2026-10-14T09:59:12.000Z")], false).await?;
        assert_eq!(report.updated, 1);
        let r = f.run(&format!("SELECT metric FROM {}.readings_by_device WHERE device_id = 'dev-88f2' AND bucket = '2026-10-14' AND ts = '2026-10-14 09:59:12+0000'", f.ks)).await?;
        assert_eq!(text(&r.rows[0][0]), "renamed");
        // A row deleted meanwhile is not recreated by the update.
        let gone =
            ops::apply_changes(&*f.session, &table, &[update("2026-10-14T01:00:00.000Z")], false).await.unwrap_err();
        assert_eq!(gone.kind, ErrorKind::Constraint);
        let insert = RowChange::Insert {
            values: vec![
                ColumnValue { column: "device_id".into(), value: Some("dev-9".into()) },
                ColumnValue { column: "bucket".into(), value: Some("2026-10-15".into()) },
                ColumnValue { column: "ts".into(), value: Some("2026-10-15 10:00:00".into()) },
                ColumnValue { column: "value".into(), value: Some("1.5".into()) },
            ],
        };
        ops::apply_changes(&*f.session, &table, std::slice::from_ref(&insert), false).await?;
        let dup = ops::apply_changes(&*f.session, &table, &[insert], false).await.unwrap_err();
        assert_eq!(dup.kind, ErrorKind::Constraint);
        // Several changes go in one logged batch.
        let delete = RowChange::Delete { key: key("2026-10-14T09:58:01.000Z") };
        let report =
            ops::apply_changes(&*f.session, &table, &[delete, update("2026-10-14T09:59:12.000Z")], false).await?;
        assert_eq!((report.deleted, report.updated), (1, 1));
        let left = f
            .run(&format!(
                "SELECT ts FROM {}.readings_by_device WHERE device_id = 'dev-88f2' AND bucket = '2026-10-14'",
                f.ks
            ))
            .await?;
        assert_eq!(left.rows.len(), 1);
        Ok(())
    });
}

#[tokio::test]
async fn table_pages_skip_rows_and_count() {
    each_server!(|f| {
        f.run(&format!("CREATE TABLE {}.many (p int, c int, v text, PRIMARY KEY (p, c))", f.ks)).await?;
        for c in 0..250 {
            f.run(&format!("INSERT INTO {}.many (p, c, v) VALUES ({}, {c}, 'v{c}')", f.ks, c % 3)).await?;
        }
        let table = f.table("many");
        let request = |offset, limit| TablePageRequest {
            object: table.clone(),
            rows: RowFilter::default(),
            sort: None,
            offset,
            limit,
        };
        let first = ops::table_page(&*f.session, &request(0, 100)).await?;
        assert_eq!((first.rows.len(), first.has_more), (100, true));
        let last = ops::table_page(&*f.session, &request(200, 100)).await?;
        assert_eq!((last.rows.len(), last.has_more), (50, false));
        let mut all = first.rows.clone();
        all.extend(ops::table_page(&*f.session, &request(100, 100)).await?.rows);
        all.extend(last.rows);
        let mut keys: Vec<String> = all.iter().map(|r| format!("{}/{}", text(&r[0]), text(&r[1]))).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), 250, "pages do not overlap");
        assert_eq!(ops::row_count(&*f.session, &table, &RowFilter::default()).await?.count, 250);
        let filtered = RowFilter { filter: Some("v = 'v7'".into()), ..RowFilter::default() };
        let page = ops::table_page(&*f.session, &TablePageRequest { rows: filtered, ..request(0, 10) }).await?;
        assert_eq!(page.rows.len(), 1);
        Ok(())
    });
}

#[tokio::test]
async fn imports_and_exports() {
    each_server!(|f| {
        f.run(&format!(
            "CREATE TABLE {}.imported (id int PRIMARY KEY, name text, amount decimal, seen timestamp)",
            f.ks
        ))
        .await?;
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("in.csv");
        let mut body = String::from("id,name,amount,seen\n");
        for i in 0..30 {
            body.push_str(&format!("{i},name {i},{i}.25,2026-10-14 09:00:{:02}\n", i % 60));
        }
        std::fs::write(&csv, body).unwrap();
        let mapping: Vec<ImportMapping> = ["id", "name", "amount", "seen"]
            .iter()
            .enumerate()
            .map(|(i, c)| ImportMapping { column: (*c).into(), field: FieldRef::Index(i) })
            .collect();
        let source = ImportSource::Csv { path: csv.to_string_lossy().into_owned(), options: CsvOptions::default() };
        let none = TransferControl::none();
        let report =
            ops::import(&*f.session, &f.table("imported"), &source, &mapping, &ImportOptions::default(), &none).await?;
        assert_eq!((report.rows, report.error_count), (30, 0), "{report:?}");
        let check = f.run(&format!("SELECT amount FROM {}.imported WHERE id = 7", f.ks)).await?;
        assert_eq!(text(&check.rows[0][0]), "7.25");
        let out = dir.path().join("out.csv");
        let export = ExportSource::Table { object: f.table("imported"), rows: RowFilter::default(), sort: None };
        let report =
            ops::export(&*f.session, &export, ExportFormat::Csv, &ExportOptions::default(), &out, &none).await?;
        assert_eq!(report.rows, 30);
        assert_eq!(std::fs::read_to_string(&out).unwrap().lines().count(), 31);
        Ok(())
    });
}

#[tokio::test]
async fn options_and_cqlsh_commands() {
    each_server!(|f| {
        let consistency = || f.session.options().into_iter().find(|o| o.name == "consistency").unwrap().value;
        assert_eq!(consistency(), "LOCAL_QUORUM");
        f.session.set_option("consistency", "one").await?;
        assert_eq!(consistency(), "ONE");
        assert!(f.session.set_option("consistency", "MOST").await.is_err());
        assert_eq!(f.session.set_option("colour", "red").await.unwrap_err().kind, ErrorKind::Unsupported);
        f.session.set_option("page_size", "500").await?;
        let r = f.run("CONSISTENCY QUORUM").await?;
        assert!(text(&r.rows[0][0]).contains("QUORUM"));
        assert_eq!(consistency(), "QUORUM");
        assert!(f.run("CONSISTENCY MOST").await.is_err());
        f.run(&format!("USE {}", f.ks)).await?;
        let r = f.run("SELECT * FROM devices").await?;
        assert_eq!(r.rows.len(), 1);
        Ok(())
    });
}

#[tokio::test]
async fn read_only_connections_refuse_writes() {
    for server in servers() {
        let config = ConnConfig { read_only: true, ..server.config.clone() };
        let s = server.adapter.connect(&ConnectParams::new(config, server.password.clone())).await.unwrap();
        let e = s
            .execute(
                "CREATE KEYSPACE never WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}",
                &[],
                OPTS,
            )
            .await
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Permission);
        assert!(s.execute("SELECT release_version FROM system.local", &[], OPTS).await.is_ok());
        s.close().await;
    }
}

#[tokio::test]
async fn explain_analyses_and_traces() {
    each_server!(|f| {
        let sql =
            format!("SELECT * FROM {}.readings_by_device WHERE device_id = 'dev-88f2' AND bucket = '2026-10-14'", f.ks);
        let plan = f.session.explain(&sql, &[], false, 30).await?;
        let node = plan.plan.unwrap();
        assert_eq!(node.label, "Single-partition read");
        assert!(node.props.iter().any(|(k, v)| k == "Partition key" && v == "device_id, bucket"));
        let scan = f.session.explain(&format!("SELECT * FROM {}.devices", f.ks), &[], false, 30).await?;
        assert_eq!(scan.plan.unwrap().label, "Scan");
        let traced = f.session.explain(&sql, &[], true, 30).await?;
        assert!(traced.analyzed && !traced.plan.unwrap().children.is_empty(), "{}", traced.raw);
        Ok(())
    });
}

#[tokio::test]
async fn schema_changes_run_through_the_editor() {
    each_server!(|f| {
        let table = f.table("made");
        let col = |name: &str, ty: &str| ColumnDef {
            name: name.into(),
            data_type: ty.into(),
            nullable: true,
            default: None,
            identity: false,
        };
        let changes = [
            SchemaChange::CreateTable {
                table: table.clone(),
                columns: vec![col("id", "int"), col("ts", "timestamp"), col("note", "text")],
                primary_key: vec!["id".into(), "ts".into()],
            },
            SchemaChange::AddColumn { table: table.clone(), column: col("extra", "bigint") },
            SchemaChange::DropColumn { table: table.clone(), column: "note".into() },
        ];
        for change in &changes {
            for statement in statements(&CqlDialect, change, None)? {
                let r = f.session.execute(&statement, &[], OPTS).await?;
                assert!(r.columns.is_empty(), "{statement}");
            }
        }
        let s = f.session.structure(&table).await?;
        let names: Vec<&str> = s.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "ts", "extra"]);
        assert_eq!(
            dq_core::sql::analyze("DROP TABLE x", dq_core::sql::SqlFlavor::Cql).unwrap().kind,
            StatementKind::Drop
        );
        Ok(())
    });
}

#[tokio::test]
async fn monitoring_reads_system_tables() {
    each_server!(|f| {
        let monitor = f.session.monitor().unwrap();
        let snap = monitor.snapshot(30).await?;
        assert!(!snap.processes.is_empty() || !snap.unavailable.is_empty());
        assert!(snap.replication.is_some());
        let settings = monitor.server_settings(Some("cache")).await?;
        assert!(!settings.is_empty());
        monitor.largest_tables(&f.ks, 10).await?;
        assert_eq!(monitor.kill(1, false).await.unwrap_err().kind, ErrorKind::Unsupported);
        assert!(!f.session.cancel().await?, "nothing is running");
        Ok(())
    });
}
