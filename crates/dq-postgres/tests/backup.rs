//! Backup and restore round trips with `pg_dump`, `pg_restore` and `psql`.
//!
//! Needs `DATAQUERY_TEST_PG` (see `postgres.rs`) and the PostgreSQL client
//! tools; skipped with a message otherwise. Tables are named `bk_<run>_…` so
//! concurrent test runs on the same database do not collide.

use std::sync::Arc;

use dq_core::ErrorKind;
use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::backup::{self, BackupContent, BackupFormat, BackupOptions, RestoreOptions, Tool, ToolOverrides};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::engine::Engine;
use dq_core::result::ExecOptions;
use dq_core::transfer::TransferControl;
use dq_postgres::PostgresAdapter;

const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };
const DB: &str = "dataquery_test";

fn params() -> Option<ConnectParams> {
    let target = std::env::var("DATAQUERY_TEST_PG").ok()?;
    let (host, port) = target.split_once(':').unwrap_or((&target, "5432"));
    let config = ConnConfig {
        engine: Engine::Postgres,
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

/// Connection parameters, or `None` (with a message) when the server or the tools are missing.
async fn require() -> Option<ConnectParams> {
    let Some(params) = params() else {
        eprintln!("skipped: set DATAQUERY_TEST_PG to run PostgreSQL backup tests");
        return None;
    };
    for tool in [Tool::PgDump, Tool::PgRestore, Tool::Psql] {
        if backup::tools::locate(tool, Engine::Postgres, None, None).await.found.is_none() {
            eprintln!("skipped: {} is not installed (brew install libpq)", tool.id());
            return None;
        }
    }
    Some(params)
}

fn run_id() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
    format!("bk_{}_{nanos:x}", std::process::id())
}

async fn count(s: &dyn Session, sql: &str) -> String {
    s.execute(sql, &[], OPTS).await.unwrap().rows[0][0].to_text().unwrap()
}

async fn seed(s: &dyn Session, prefix: &str) {
    for sql in [
        format!("CREATE TABLE public.{prefix}_customers (id int PRIMARY KEY, email text NOT NULL)"),
        format!(
            "CREATE TABLE public.{prefix}_orders (id int PRIMARY KEY, customer_id int REFERENCES public.{prefix}_customers, note text)"
        ),
        format!(
            "INSERT INTO public.{prefix}_customers SELECT n, 'u' || n || '@example.com' FROM generate_series(1, 300) n"
        ),
        format!(
            "INSERT INTO public.{prefix}_orders SELECT n, n % 300 + 1, repeat('ü', n % 7) FROM generate_series(1, 4000) n"
        ),
    ] {
        s.execute(&sql, &[], OPTS).await.unwrap();
    }
}

async fn drop_tables(s: &dyn Session, prefix: &str) {
    s.execute(&format!("DROP TABLE IF EXISTS public.{prefix}_orders, public.{prefix}_customers"), &[], OPTS)
        .await
        .unwrap();
}

#[tokio::test]
async fn round_trips_every_format() {
    let Some(p) = require().await else { return };
    let s: Arc<dyn Session> = PostgresAdapter::POSTGRES.connect(&p).await.unwrap();
    let version = s.info().version.clone();
    let none = ToolOverrides::new();
    let prefix = run_id();
    seed(&*s, &prefix).await;
    let dir = tempfile::tempdir().unwrap();

    let plan = backup::plan(Engine::Postgres, &version, &none).await.unwrap();
    assert_eq!(plan.formats[0], BackupFormat::Custom);
    assert!(plan.tools.iter().all(|t| t.path.is_some() && t.version.is_some()), "{:?}", plan.tools);

    for (format, name) in [
        (BackupFormat::Custom, "app.dump"),
        (BackupFormat::Plain, "app.sql"),
        (BackupFormat::Directory, "app-dir"),
        (BackupFormat::Tar, "app.tar"),
    ] {
        let path = dir.path().join(name);
        let options = BackupOptions {
            database: DB.into(),
            format,
            tables: vec![format!("public.{prefix}_*")],
            no_owner: true,
            clean: true,
            ..BackupOptions::default()
        };
        let progress = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = progress.clone();
        let control = TransferControl::with_progress(Arc::new(false.into()), move |p| {
            seen.lock().unwrap().push(p.detail.map(str::to_owned));
        });
        let report = backup::backup(&p, &version, None, &options, &path, &none, &control).await.unwrap();
        assert_eq!(report.objects, 2, "{format:?}: both tables were dumped");
        assert!(report.bytes > 0 && report.tool.as_deref().unwrap().starts_with("pg_dump "));
        assert!(path.exists());

        drop_tables(&*s, &prefix).await;
        let restore = RestoreOptions {
            database: DB.into(),
            no_owner: true,
            single_transaction: format != BackupFormat::Directory,
            ..RestoreOptions::default()
        };
        let report = backup::restore(&p, &version, None, &restore, &path, &none, &TransferControl::none())
            .await
            .unwrap_or_else(|e| panic!("{format:?}: {e:?}"));
        assert_eq!(report.format, format, "detected");
        assert_eq!(count(&*s, &format!("SELECT count(*) FROM public.{prefix}_orders")).await, "4000");
        assert_eq!(count(&*s, &format!("SELECT sum(length(note)) FROM public.{prefix}_orders")).await, "11997");
    }

    // A plain dump with --clean restores over existing tables; schema-only restores create no rows.
    let custom = dir.path().join("app.dump");
    let clean =
        RestoreOptions { database: DB.into(), clean: true, single_transaction: true, ..RestoreOptions::default() };
    s.execute(&format!("DELETE FROM public.{prefix}_orders WHERE id > 10"), &[], OPTS).await.unwrap();
    backup::restore(&p, &version, None, &clean, &custom, &none, &TransferControl::none()).await.unwrap();
    assert_eq!(count(&*s, &format!("SELECT count(*) FROM public.{prefix}_orders")).await, "4000");
    drop_tables(&*s, &prefix).await;
    let schema_only = RestoreOptions { content: BackupContent::SchemaOnly, ..clean.clone() };
    backup::restore(&p, &version, None, &schema_only, &custom, &none, &TransferControl::none()).await.unwrap();
    assert_eq!(count(&*s, &format!("SELECT count(*) FROM public.{prefix}_orders")).await, "0");
    drop_tables(&*s, &prefix).await;
    s.close().await;
}

#[tokio::test]
async fn failures_are_readable_and_leave_no_files() {
    let Some(p) = require().await else { return };
    let none = ToolOverrides::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.dump");

    let mut wrong = p.clone();
    wrong.password = Some("not-the-password".into());
    let options = BackupOptions { database: DB.into(), ..BackupOptions::default() };
    let err = backup::backup(&wrong, "", None, &options, &path, &none, &TransferControl::none()).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth, "{err:?}");
    assert!(err.message.starts_with("pg_dump ") && err.message.contains("password authentication failed"));
    assert!(!err.message.contains("not-the-password") && !err.detail.unwrap_or_default().contains("not-the-password"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "the partial file was removed");

    let missing = BackupOptions { database: "bk_no_such_database".into(), ..options };
    let err = backup::backup(&p, "", None, &missing, &path, &none, &TransferControl::none()).await.unwrap_err();
    assert!(err.message.contains("bk_no_such_database"), "{}", err.message);

    // Cancelling before the tool starts removes nothing it did not create.
    let cancelled = TransferControl::new(Arc::new(true.into()), |_, _| {});
    std::fs::write(&path, b"keep").unwrap();
    let options = BackupOptions { database: DB.into(), ..BackupOptions::default() };
    let err = backup::backup(&p, "", None, &options, &path, &none, &cancelled).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert_eq!(std::fs::read(&path).unwrap(), b"keep", "an existing file is only replaced by a finished backup");
}

#[tokio::test]
async fn reports_an_old_pg_dump() {
    let Some(_) = require().await else { return };
    // A Settings path to an old pg_dump is used as chosen, and its age is reported.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("pg_dump");
        std::fs::write(&fake, "#!/bin/sh\necho 'pg_dump (PostgreSQL) 12.1'\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let overrides = ToolOverrides::from([("pg_dump".to_owned(), fake.to_string_lossy().into_owned())]);
        let plan = backup::plan(Engine::Postgres, "PostgreSQL 17.4", &overrides).await.unwrap();
        let tool = plan.tools.iter().find(|t| t.tool == Tool::PgDump).unwrap();
        assert!(tool.custom && tool.version.as_deref() == Some("12.1"));
        assert!(plan.warnings.iter().any(|w| w.contains("refuses")), "{:?}", plan.warnings);
    }
}
