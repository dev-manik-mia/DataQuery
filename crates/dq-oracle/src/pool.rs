//! A small pool of catalog connections. Oracle calls block, so work runs on
//! a blocking thread with a connection checked out of the pool.

use std::sync::{Arc, Mutex};

use dq_core::adapter::ConnectParams;
use dq_core::{DbError, DbResult, ErrorKind};
use oracle::Connection;
use oracle::oci_attr::TransactionInProgress;
use tokio::sync::Semaphore;

use crate::connect;

/// Sets `DBMS_METADATA` output to plain, re-runnable DDL.
const METADATA_SETUP: &str = "BEGIN \
  DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'SEGMENT_ATTRIBUTES', FALSE); \
  DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'STORAGE', FALSE); \
  DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'SQLTERMINATOR', TRUE); \
  DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'PRETTY', TRUE); \
END;";

pub struct Pool {
    params: ConnectParams,
    idle: Arc<Mutex<Vec<Connection>>>,
    permits: Semaphore,
}

impl Pool {
    pub fn new(params: ConnectParams) -> Self {
        let size = params.config.options.pool_size.max(1) as usize;
        Self { params, idle: Arc::new(Mutex::new(Vec::new())), permits: Semaphore::new(size) }
    }

    pub fn params(&self) -> &ConnectParams {
        &self.params
    }

    /// Runs `work` with a catalog connection on a blocking thread. A
    /// connection that failed with a network error is closed, not reused.
    pub async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Connection) -> DbResult<T> + Send + 'static,
    ) -> DbResult<T> {
        let _permit = self.permits.acquire().await.map_err(|_| DbError::invalid("The session is closed."))?;
        let idle = self.idle.clone();
        let params = self.params.clone();
        tokio::task::spawn_blocking(move || {
            let reused = idle.lock().ok().and_then(|mut i| i.pop());
            let conn = if let Some(conn) = reused {
                conn
            } else {
                let conn = connect::open_blocking(&params)?;
                // Plain DDL without storage clauses; failing only loses formatting.
                let _ = conn.execute(METADATA_SETUP, &[]);
                conn
            };
            let result = work(&conn);
            let broken = result.as_ref().is_err_and(|e| matches!(e.kind, ErrorKind::Network | ErrorKind::Timeout));
            if !broken {
                // Catalog work never leaves a transaction open. The client
                // library tracks whether one is, so the usual case skips the round trip.
                if conn.oci_attr::<TransactionInProgress>().unwrap_or(true) {
                    let _ = conn.rollback();
                }
                if let Ok(mut i) = idle.lock() {
                    i.push(conn);
                }
            }
            result
        })
        .await
        .map_err(|e| DbError::internal(format!("catalog task failed: {e}")))?
    }

    pub async fn close(&self) {
        let idle = self.idle.lock().map(|mut i| std::mem::take(&mut *i)).unwrap_or_default();
        let _ = tokio::task::spawn_blocking(move || {
            for c in idle {
                let _ = c.close();
            }
        })
        .await;
    }
}
