//! Backup and restore round trips with `mysqldump` / `mariadb-dump` and the
//! `mysql` / `mariadb` client.
//!
//! Needs `DATAQUERY_TEST_MYSQL` (see `mysql.rs`; MariaDB works too) and the
//! client tools; skipped with a message otherwise. Tables are named
//! `bk_<run>_…` so concurrent test runs do not collide.

use std::sync::Arc;

use dq_core::ErrorKind;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::backup::{self, BackupContent, BackupFormat, BackupOptions, RestoreOptions, Tool, ToolOverrides};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::engine::Engine;
use dq_core::result::ExecOptions;
use dq_core::transfer::TransferControl;
use dq_mysql::MysqlAdapter;

const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };
const DB: &str = "dataquery_test";

fn params() -> Option<ConnectParams> {
    let target = std::env::var("DATAQUERY_TEST_MYSQL").ok()?;
    let (host, port) = target.split_once(':').unwrap_or((&target, "3306"));
    let config = ConnConfig {
        engine: Engine::Mysql,
        host: host.into(),
        port: port.parse().unwrap(),
        database: DB.into(),
        username: "dataquery".into(),
        ssl_mode: SslMode::Disable,
        connect_timeout_secs: 5,
        ..ConnConfig::default()
    };
    Some(ConnectParams::new(config, Some("dataquery".into())))
}

/// A session and its parameters (engine set from the server), or `None` with a message.
async fn require() -> Option<(ConnectParams, Arc<dyn Session>)> {
    let Some(mut p) = params() else {
        eprintln!("skipped: set DATAQUERY_TEST_MYSQL to run MySQL backup tests");
        return None;
    };
    let s = MysqlAdapter::MYSQL.connect(&p).await.unwrap();
    if s.info().version.contains("MariaDB") {
        p.config.engine = Engine::Mariadb;
    }
    for tool in [Tool::Mysqldump, Tool::Mysql] {
        if backup::tools::locate(tool, p.config.engine, backup::ServerVersion::parse(&s.info().version), None)
            .await
            .found
            .is_none()
        {
            eprintln!("skipped: {} is not installed (brew install mysql-client)", tool.id());
            return None;
        }
    }
    Some((p, s))
}

fn run_id() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
    format!("bk_{}_{nanos:x}", std::process::id())
}

async fn count(s: &dyn Session, sql: &str) -> String {
    s.execute(sql, &[], OPTS).await.unwrap().rows[0][0].to_text().unwrap()
}

#[tokio::test]
async fn round_trips_selected_tables() {
    let Some((p, s)) = require().await else { return };
    let version = s.info().version.clone();
    let none = ToolOverrides::new();
    let prefix = run_id();
    let (customers, orders) = (format!("{prefix}_customers"), format!("{prefix}_orders"));
    for sql in [
        format!("CREATE TABLE {customers} (id INT PRIMARY KEY, email VARCHAR(100) NOT NULL)"),
        format!(
            "CREATE TABLE {orders} (id INT PRIMARY KEY, customer_id INT, note TEXT, raw VARBINARY(8), FOREIGN KEY (customer_id) REFERENCES {customers} (id))"
        ),
        format!(
            "INSERT INTO {customers} SELECT n, CONCAT('u', n) FROM (SELECT 1 n UNION ALL SELECT 2 UNION ALL SELECT 3) g"
        ),
        format!(
            "INSERT INTO {orders} VALUES (1, 1, 'Zoë ✓', X'00FF10'), (2, 2, NULL, NULL), (3, 3, 'it''s \\\\ \"quoted\"', X'')"
        ),
    ] {
        s.execute(&sql, &[], OPTS).await.unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.sql");
    let options = BackupOptions {
        database: DB.into(),
        format: BackupFormat::Plain,
        tables: vec![customers.clone(), orders.clone()],
        clean: true,
        ..BackupOptions::default()
    };
    let details = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = details.clone();
    let control = TransferControl::with_progress(Arc::new(false.into()), move |p| {
        seen.lock().unwrap().push(p.rows);
    });
    let report = backup::backup(&p, &version, None, &options, &path, &none, &control).await.unwrap();
    assert_eq!(report.objects, 2);
    assert!(report.bytes > 0);
    let dump = std::fs::read_to_string(&path).unwrap();
    assert!(!dump.contains("CREATE DATABASE") && !dump.contains("USE `"), "restorable into any database");
    assert!(!dump.contains("dataquery\n") && !dump.contains("password"), "no credentials in the file");

    s.execute(&format!("DROP TABLE {orders}, {customers}"), &[], OPTS).await.unwrap();
    let restore = RestoreOptions { database: DB.into(), ..RestoreOptions::default() };
    let progress = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = progress.clone();
    let control = TransferControl::with_progress(Arc::new(false.into()), move |p| {
        seen.lock().unwrap().push((p.bytes, p.total_bytes));
    });
    let report = backup::restore(&p, &version, None, &restore, &path, &none, &control).await.unwrap();
    assert_eq!((report.format, report.bytes), (BackupFormat::Plain, dump.len() as u64));
    assert_eq!(count(&*s, &format!("SELECT COUNT(*) FROM {orders}")).await, "3");
    assert_eq!(count(&*s, &format!("SELECT note FROM {orders} WHERE id = 1")).await, "Zoë ✓");
    assert_eq!(count(&*s, &format!("SELECT HEX(raw) FROM {orders} WHERE id = 1")).await, "00FF10");
    assert_eq!(count(&*s, &format!("SELECT note FROM {orders} WHERE id = 3")).await, "it's \\ \"quoted\"");

    // Restoring again over existing tables works because the dump drops them first.
    backup::restore(&p, &version, None, &restore, &path, &none, &TransferControl::none()).await.unwrap();

    // Schema only: the tables come back empty.
    let schema = dir.path().join("schema.sql");
    let schema_only = BackupOptions { content: BackupContent::SchemaOnly, ..options };
    backup::backup(&p, &version, None, &schema_only, &schema, &none, &TransferControl::none()).await.unwrap();
    s.execute(&format!("DROP TABLE {orders}, {customers}"), &[], OPTS).await.unwrap();
    backup::restore(&p, &version, None, &restore, &schema, &none, &TransferControl::none()).await.unwrap();
    assert_eq!(count(&*s, &format!("SELECT COUNT(*) FROM {orders}")).await, "0");

    // A script that fails reports the client's error and its line.
    let broken = dir.path().join("broken.sql");
    std::fs::write(&broken, format!("INSERT INTO {orders} VALUES (1, NULL, 'a', NULL);\nNOT SQL;\n")).unwrap();
    let err =
        backup::restore(&p, &version, None, &restore, &broken, &none, &TransferControl::none()).await.unwrap_err();
    assert!(err.message.contains("ERROR 1064") && err.message.contains("at line 2"), "{}", err.message);

    s.execute(&format!("DROP TABLE {orders}, {customers}"), &[], OPTS).await.unwrap();
    s.close().await;
}

#[tokio::test]
async fn wrong_passwords_fail_readably() {
    let Some((mut p, s)) = require().await else { return };
    let version = s.info().version.clone();
    s.close().await;
    p.password = Some("not-the-password".into());
    let dir = tempfile::tempdir().unwrap();
    let options = BackupOptions { database: DB.into(), format: BackupFormat::Plain, ..BackupOptions::default() };
    let err = backup::backup(
        &p,
        &version,
        None,
        &options,
        &dir.path().join("x.sql"),
        &ToolOverrides::new(),
        &TransferControl::none(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth, "{err:?}");
    assert!(err.message.contains("Access denied"), "{}", err.message);
    assert!(!format!("{err:?}").contains("not-the-password"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "the partial file was removed");
}
