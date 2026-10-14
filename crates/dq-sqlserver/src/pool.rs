//! A small pool of catalog connections. Each checkout switches the connection
//! to the database it is needed for, so one pool serves every database.

use std::sync::Mutex;

use dq_core::adapter::ConnectParams;
use dq_core::sql::{Dialect, SqlServerDialect};
use dq_core::{DbError, DbResult, ErrorKind};
use tokio::sync::{Semaphore, SemaphorePermit};

use crate::connect::{self, Conn};
use crate::error::map_error;

struct Idle {
    conn: Conn,
    database: String,
}

pub struct Pool {
    params: ConnectParams,
    /// Database of new connections when the caller names none.
    default_database: String,
    idle: Mutex<Vec<Idle>>,
    permits: Semaphore,
}

/// A checked-out connection; it returns to the pool when dropped, unless marked broken.
pub struct Pooled<'a> {
    pool: &'a Pool,
    conn: Option<Idle>,
    _permit: SemaphorePermit<'a>,
}

impl Pooled<'_> {
    pub fn conn(&mut self) -> &mut Conn {
        &mut self.conn.as_mut().expect("checked out").conn
    }

    /// Passes `result` through; a network failure closes the connection
    /// instead of returning it to the pool.
    pub fn check<T>(mut self, result: DbResult<T>) -> DbResult<T> {
        if result.as_ref().is_err_and(|e| matches!(e.kind, ErrorKind::Network | ErrorKind::Tls | ErrorKind::Timeout)) {
            self.conn = None;
        }
        result
    }
}

impl Drop for Pooled<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take()
            && let Ok(mut idle) = self.pool.idle.lock()
        {
            idle.push(conn);
        }
    }
}

impl Pool {
    pub fn new(params: ConnectParams, default_database: String) -> Self {
        let size = params.config.options.pool_size.max(1) as usize;
        Self { params, default_database, idle: Mutex::new(Vec::new()), permits: Semaphore::new(size) }
    }

    pub fn params(&self) -> &ConnectParams {
        &self.params
    }

    /// A connection whose current database is `database` (the session's when empty).
    pub async fn get(&self, database: &str) -> DbResult<Pooled<'_>> {
        Box::pin(self.checkout(database)).await
    }

    async fn checkout(&self, database: &str) -> DbResult<Pooled<'_>> {
        let permit = self.permits.acquire().await.map_err(|_| DbError::invalid("The session is closed."))?;
        let database = if database.is_empty() { self.default_database.as_str() } else { database };
        let idle = self.idle.lock().ok().and_then(|mut idle| {
            let i =
                idle.iter().position(|c| c.database == database).or(if idle.is_empty() { None } else { Some(0) })?;
            Some(idle.swap_remove(i))
        });
        let mut idle = match idle {
            Some(idle) => idle,
            None => Idle { conn: connect::open(&self.params, Some(database)).await?, database: database.to_owned() },
        };
        if idle.database != database {
            let sql = format!("USE {}", SqlServerDialect.quote_ident(database));
            let err = |e: tiberius::error::Error| map_error(&e, self.params.password());
            idle.conn.simple_query(sql).await.map_err(err)?.into_results().await.map_err(err)?;
            idle.database = database.to_owned();
        }
        Ok(Pooled { pool: self, conn: Some(idle), _permit: permit })
    }

    pub async fn close(&self) {
        let idle = self.idle.lock().map(|mut i| std::mem::take(&mut *i)).unwrap_or_default();
        for c in idle {
            let _ = c.conn.close().await;
        }
    }
}
