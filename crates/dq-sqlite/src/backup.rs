//! Backup and restore with `SQLite`'s online backup API: page by page, while
//! the database stays usable, without external tools.

use std::path::Path;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dq_core::backup::NativeBackup;
use dq_core::transfer::{Progress, TransferControl};
use dq_core::{DbError, DbResult, ErrorKind};
use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags};

use crate::error::map_error;
use crate::session::{SqliteSession, read_only_error};

/// Pages copied per step; the source stays readable and writable between steps.
const PAGES_PER_STEP: i32 = 256;
/// How long a step may keep finding the database locked by another connection.
const BUSY_LIMIT: Duration = Duration::from_secs(30);

/// `main` when no database is named.
fn name(database: &str) -> &str {
    if database.trim().is_empty() { "main" } else { database }
}

/// Copies every page of `backup`, reporting progress. Returning early (cancel,
/// error) drops the backup, which rolls the destination back.
fn copy(backup: &Backup<'_, '_>, page_size: u64, control: &TransferControl) -> DbResult<u64> {
    let mut busy_since: Option<Instant> = None;
    loop {
        control.check()?;
        let step = backup.step(PAGES_PER_STEP).map_err(|e| map_error(&e))?;
        let p = backup.progress();
        let (done, total) = ((p.pagecount - p.remaining).max(0) as u64, p.pagecount.max(0) as u64);
        let detail = format!("Page {done} of {total}");
        control.report(&Progress {
            rows: 0,
            bytes: done * page_size,
            total_bytes: Some(total * page_size),
            detail: Some(&detail),
        });
        match step {
            StepResult::Done => break,
            StepResult::Busy | StepResult::Locked => {
                if busy_since.get_or_insert_with(Instant::now).elapsed() > BUSY_LIMIT {
                    return Err(DbError::new(ErrorKind::Timeout, "The database stayed locked by another connection.")
                        .with_action(Some("Commit or roll back open transactions, then try again.")));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => busy_since = None,
        }
    }
    let total = backup.progress().pagecount.max(0) as u64;
    Ok(total * page_size)
}

fn page_size(conn: &Connection, database: &str) -> DbResult<u64> {
    let quoted = database.replace('"', "\"\"");
    conn.query_row(&format!("PRAGMA \"{quoted}\".page_size"), [], |r| r.get::<_, i64>(0))
        .map(|s| s.max(0) as u64)
        .map_err(|e| map_error(&e))
}

#[async_trait]
impl NativeBackup for SqliteSession {
    async fn backup_to(&self, database: &str, path: &Path, control: &TransferControl) -> DbResult<u64> {
        let source = self.export_connection().await?;
        let (database, path, control) = (name(database).to_owned(), path.to_owned(), control.clone());
        tokio::task::spawn_blocking(move || {
            let page_size = page_size(&source, &database)?;
            let mut target = Connection::open(&path).map_err(|e| map_error(&e))?;
            let backup =
                Backup::new_with_names(&source, database.as_str(), &mut target, "main").map_err(|e| map_error(&e))?;
            copy(&backup, page_size, &control)
        })
        .await
        .map_err(|e| DbError::internal(format!("backup task failed: {e}")))?
    }

    async fn restore_from(&self, database: &str, path: &Path, control: &TransferControl) -> DbResult<u64> {
        if self.read_only {
            return Err(read_only_error());
        }
        let (database, path, control) = (name(database).to_owned(), path.to_owned(), control.clone());
        // Restores through the edit connection, so they wait for its grid saves and imports.
        let writer = self.writer.clone();
        tokio::task::spawn_blocking(move || {
            let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
            let source = Connection::open_with_flags(&path, flags).map_err(|e| map_error(&e))?;
            // Reading the schema rejects a file that is not a database before anything changes.
            source
                .query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0))
                .map_err(|e| map_error(&e))?;
            let page_size = page_size(&source, "main")?;
            let mut target = writer.lock().map_err(|_| DbError::internal("connection lock poisoned"))?;
            let backup =
                Backup::new_with_names(&source, "main", &mut target, database.as_str()).map_err(|e| map_error(&e))?;
            copy(&backup, page_size, &control)
        })
        .await
        .map_err(|e| DbError::internal(format!("restore task failed: {e}")))?
    }
}
