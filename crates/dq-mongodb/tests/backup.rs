//! Backup and restore round trips with `mongodump --archive` and `mongorestore`.
//!
//! Needs `DATAQUERY_TEST_MONGODB` (see `mongodb.rs`) and the MongoDB Database
//! Tools; skipped with a message otherwise. Each run uses its own `bk_…` databases.

use std::sync::Arc;

use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::backup::{self, BackupFormat, BackupOptions, RestoreOptions, Tool, ToolOverrides};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::engine::Engine;
use dq_core::result::ExecOptions;
use dq_core::transfer::TransferControl;
use dq_mongodb::MongodbAdapter;

const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };

fn params(database: &str) -> Option<ConnectParams> {
    let target = std::env::var("DATAQUERY_TEST_MONGODB").ok()?;
    let (host, port) = target.split_once(':').unwrap_or((&target, "27017"));
    let config = ConnConfig {
        engine: Engine::Mongodb,
        host: host.into(),
        port: port.parse().unwrap(),
        database: database.into(),
        username: "dataquery".into(),
        ssl_mode: SslMode::Disable,
        connect_timeout_secs: 5,
        ..ConnConfig::default()
    };
    Some(ConnectParams::new(config, Some("dataquery".into())))
}

async fn session(p: &ConnectParams) -> Arc<dyn Session> {
    MongodbAdapter.connect(p).await.unwrap()
}

async fn count(s: &dyn Session, collection: &str) -> String {
    let r = s.execute(&format!("db.{collection}.countDocuments({{}})"), &[], OPTS).await.unwrap();
    r.rows[0][0].to_text().unwrap()
}

#[tokio::test]
async fn round_trips_a_database_into_another() {
    let name = format!("bk_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let Some(p) = params(&name) else {
        eprintln!("skipped: set DATAQUERY_TEST_MONGODB to run MongoDB backup tests");
        return;
    };
    for tool in [Tool::Mongodump, Tool::Mongorestore] {
        if backup::tools::locate(tool, Engine::Mongodb, None, None).await.found.is_none() {
            eprintln!("skipped: {} is not installed (brew install mongodb/brew/mongodb-database-tools)", tool.id());
            return;
        }
    }
    let s = session(&p).await;
    let version = s.info().version.clone();
    s.execute(
        "db.items.insertMany([{ _id: 1, name: 'Zoë', price: NumberDecimal('12.50') }, { _id: 2, tags: ['a', 'b'] }])",
        &[],
        OPTS,
    )
    .await
    .unwrap();
    s.execute("db.items.createIndex({ name: 1 })", &[], OPTS).await.unwrap();
    s.execute("db.logs.insertOne({ at: ISODate('2025-01-02T03:04:05Z') })", &[], OPTS).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.archive.gz");
    let none = ToolOverrides::new();
    let options = BackupOptions { database: name.clone(), format: BackupFormat::Archive, ..BackupOptions::default() };
    let report = backup::backup(&p, &version, None, &options, &path, &none, &TransferControl::none()).await.unwrap();
    assert_eq!(report.objects, 2, "two collections");
    assert!(report.bytes > 0);

    // Into a new database: detected as a gzip archive, collections renamed into it.
    let copy = format!("{name}_copy");
    let restore = RestoreOptions { database: copy.clone(), clean: true, ..RestoreOptions::default() };
    let progress = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = progress.clone();
    let control = TransferControl::with_progress(Arc::new(false.into()), move |p| {
        seen.lock().unwrap().push((p.bytes, p.total_bytes));
    });
    let report = backup::restore(&p, &version, None, &restore, &path, &none, &control).await.unwrap();
    assert_eq!((report.format, report.objects), (BackupFormat::Archive, 2));
    let target = session(&params(&copy).unwrap()).await;
    assert_eq!(count(&*target, "items").await, "2");
    assert_eq!(count(&*target, "logs").await, "1");
    let indexes = target.execute("db.items.getIndexes()", &[], OPTS).await.unwrap();
    assert_eq!(indexes.rows.len(), 2, "the index came back too");

    // Restoring again with --drop replaces instead of failing on duplicate _ids.
    backup::restore(&p, &version, None, &restore, &path, &none, &TransferControl::none()).await.unwrap();
    assert_eq!(count(&*target, "items").await, "2");

    // One collection only.
    let one = dir.path().join("items.archive");
    let options = BackupOptions { tables: vec!["items".into()], compress: false, ..options };
    let report = backup::backup(&p, &version, None, &options, &one, &none, &TransferControl::none()).await.unwrap();
    assert_eq!(report.objects, 1);
    assert_eq!(backup::detect_format(&one).unwrap(), (BackupFormat::Archive, false));

    for (s, _) in [(&s, &name), (&target, &copy)] {
        s.execute("db.dropDatabase()", &[], OPTS).await.unwrap();
        s.close().await;
    }
}
