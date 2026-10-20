//! Backup and restore of `SQLite` files through the online backup API.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use dq_core::ErrorKind;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::backup::{self, BackupFormat, BackupOptions, RestoreOptions, ToolOverrides};
use dq_core::config::ConnConfig;
use dq_core::engine::Engine;
use dq_core::result::ExecOptions;
use dq_core::transfer::TransferControl;
use dq_sqlite::SqliteAdapter;

const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };

fn params(path: &std::path::Path, read_only: bool) -> ConnectParams {
    let config = ConnConfig {
        engine: Engine::Sqlite,
        host: String::new(),
        port: 0,
        database: path.to_string_lossy().into_owned(),
        username: String::new(),
        read_only,
        ..ConnConfig::default()
    };
    ConnectParams::new(config, None)
}

async fn count(s: &dyn Session, sql: &str) -> String {
    let r = s.execute(sql, &[], OPTS).await.unwrap();
    r.rows[0][0].to_text().unwrap()
}

#[tokio::test]
async fn backs_up_and_restores_a_database_file() {
    let dir = tempfile::tempdir().unwrap();
    let p = params(&dir.path().join("live.sqlite"), false);
    let s = SqliteAdapter.connect(&p).await.unwrap();
    s.execute("CREATE TABLE bk_items (id INTEGER PRIMARY KEY, label TEXT)", &[], OPTS).await.unwrap();
    s.execute(
        "WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 5000) \
         INSERT INTO bk_items SELECT n, printf('item %05d', n) FROM g",
        &[],
        OPTS,
    )
    .await
    .unwrap();

    let file = dir.path().join("copy.sqlite");
    let options = BackupOptions { database: "main".into(), format: BackupFormat::File, ..BackupOptions::default() };
    let progress = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = progress.clone();
    let control = TransferControl::with_progress(Arc::new(AtomicBool::new(false)), move |p| {
        seen.lock().unwrap().push((p.bytes, p.total_bytes));
    });
    let none = ToolOverrides::new();
    let report =
        backup::backup(&p, &s.info().version, s.native_backup(), &options, &file, &none, &control).await.unwrap();
    assert_eq!(report.bytes, std::fs::metadata(&file).unwrap().len());
    assert!(report.tool.is_none());
    let reports = progress.lock().unwrap().clone();
    assert!(!reports.is_empty() && reports.iter().all(|(b, t)| Some(*b) <= *t), "{reports:?}");
    let leftovers = std::fs::read_dir(dir.path()).unwrap().filter_map(Result::ok);
    assert!(leftovers.into_iter().all(|e| !e.file_name().to_string_lossy().ends_with(".partial")));

    // Change the live database, then restore the copy over it.
    s.execute("DELETE FROM bk_items WHERE id > 10", &[], OPTS).await.unwrap();
    s.execute("CREATE TABLE bk_extra (x)", &[], OPTS).await.unwrap();
    let restore = RestoreOptions { database: "main".into(), ..RestoreOptions::default() };
    let report =
        backup::restore(&p, &s.info().version, s.native_backup(), &restore, &file, &none, &TransferControl::none())
            .await
            .unwrap();
    assert_eq!(report.format, BackupFormat::File, "detected from the file");
    assert_eq!(count(&*s, "SELECT count(*) FROM bk_items").await, "5000");
    assert_eq!(count(&*s, "SELECT count(*) FROM sqlite_master WHERE name = 'bk_extra'").await, "0");

    // A cancelled backup leaves no file; a cancelled restore changes nothing.
    let cancelled = TransferControl::new(Arc::new(AtomicBool::new(true)), |_, _| {});
    let other = dir.path().join("cancelled.sqlite");
    let err = backup::backup(&p, "", s.native_backup(), &options, &other, &none, &cancelled).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2, "only the live file and the copy");
    s.execute("DELETE FROM bk_items", &[], OPTS).await.unwrap();
    let err = backup::restore(&p, "", s.native_backup(), &restore, &file, &none, &cancelled).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert_eq!(count(&*s, "SELECT count(*) FROM bk_items").await, "0");

    // Files that are not SQLite databases are refused before anything changes.
    let text = dir.path().join("dump.sql");
    std::fs::write(&text, "CREATE TABLE x (a);").unwrap();
    let err =
        backup::restore(&p, "", s.native_backup(), &restore, &text, &none, &TransferControl::none()).await.unwrap_err();
    assert!(err.message.contains("SQL script"), "{}", err.message);
    s.close().await;

    // Read-only profiles can back up but never restore.
    let ro = params(&dir.path().join("live.sqlite"), true);
    let s = SqliteAdapter.connect(&ro).await.unwrap();
    let again = dir.path().join("again.sqlite");
    backup::backup(&ro, "", s.native_backup(), &options, &again, &none, &TransferControl::none()).await.unwrap();
    let err = backup::restore(&ro, "", s.native_backup(), &restore, &file, &none, &TransferControl::none())
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Permission);
    s.close().await;
}
