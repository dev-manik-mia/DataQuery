//! Schema changes from the structure forms, schema diff between connections
//! and framework migration inspection. Generated SQL is always shown for
//! review first; applying rebuilds it here from the same changes.

use std::time::Instant;

use dq_core::adapter::Session;
use dq_core::diff::{Container, DiffTable, SchemaDiff};
use dq_core::edit::BoundStatement;
use dq_core::migrations::FrameworkMigrations;
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::sql::ddl::{self, SchemaChange};
use dq_core::sql::split_statements;
use dq_core::{DbError, DbResult, ops};
use futures_util::stream::{self, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use tauri::State;
use ts_rs::TS;

use super::query::record_history;
use super::{CmdResult, Guarded};
use crate::connector;
use crate::safety::{self, Confirmation, Intent};
use crate::state::AppState;

const MAX_CHANGES: usize = 200;
/// Tables compared per side; each needs a structure read.
const MAX_DIFF_TABLES: usize = 500;
/// Tables whose structure is read at the same time while snapshotting a side.
const DIFF_READ_CONCURRENCY: usize = 8;

async fn build(session: &dyn Session, changes: &[SchemaChange]) -> DbResult<Vec<String>> {
    if changes.is_empty() || changes.len() > MAX_CHANGES {
        return Err(DbError::invalid(format!("Between 1 and {MAX_CHANGES} changes can be applied at once.")));
    }
    let d = session.dialect();
    let mut out = Vec::new();
    for change in changes {
        let structure =
            if change.needs_structure(d.flavor()) { Some(session.structure(change.table()).await?) } else { None };
        out.extend(ddl::statements(d, change, structure.as_ref())?);
    }
    Ok(out)
}

/// SQL the changes would run, for review.
#[tauri::command]
pub async fn schema_change_preview(
    state: State<'_, AppState>,
    session_id: String,
    changes: Vec<SchemaChange>,
) -> CmdResult<String> {
    let open = state.session(&session_id).await?;
    Ok(ddl::preview(&build(&*open.session, &changes).await?))
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct SchemaApplyReport {
    pub statements: u32,
    #[ts(type = "number")]
    pub duration_ms: u64,
}

/// Applies reviewed schema changes. PostgreSQL runs them in one transaction;
/// MySQL commits each DDL statement on its own (server behaviour).
#[tauri::command]
pub async fn apply_schema_changes(
    state: State<'_, AppState>,
    session_id: String,
    changes: Vec<SchemaChange>,
    confirmation: Option<Confirmation>,
) -> CmdResult<Guarded<SchemaApplyReport>> {
    let open = state.session(&session_id).await?;
    let statements = build(&*open.session, &changes).await?;
    let sql = ddl::preview(&statements);
    let (connection, settings) = tokio::try_join!(state.connection(&open.connection_id), state.settings())?;
    let analysed = split_statements(&sql, open.session.dialect().flavor());
    if let Some(request) =
        safety::check(&connection, &settings, &Intent::from_statements(&analysed), confirmation.as_ref())?
    {
        return Ok(Guarded::NeedsConfirmation { request });
    }
    let started = Instant::now();
    let database = changes.first().map(|c| c.table().database.clone()).unwrap_or_default();
    let bound = statements.iter().map(|s| Ok(BoundStatement { sql: s.clone(), params: Vec::new(), expect_rows: None }));
    let result = open.session.run_in_transaction(&database, &mut bound.collect::<Vec<_>>().into_iter()).await;
    // Also after a failure: engines without transactional DDL keep what ran.
    open.structures.invalidate(None);
    record_history(
        &state,
        &settings,
        &connection,
        &open,
        &sql,
        started.elapsed(),
        result.as_ref().map(|_| None).map_err(Clone::clone),
    );
    result?;
    Ok(Guarded::Done {
        value: SchemaApplyReport {
            statements: statements.len() as u32,
            duration_ms: started.elapsed().as_millis() as u64,
        },
    })
}

/// One side of a schema diff: a saved connection and a schema (PostgreSQL), database (MySQL) or keyspace (CQL).
#[derive(Debug, Clone, Deserialize, TS)]
#[ts(export)]
pub struct DiffSide {
    pub connection_id: String,
    pub database: String,
    pub schema: Option<String>,
}

async fn snapshot(session: &dyn Session, side: &DiffSide) -> DbResult<Vec<DiffTable>> {
    let model = session.schema_model(&side.database, side.schema.as_deref()).await?;
    let tables: Vec<_> = model.tables.iter().filter(|t| t.object.kind == NodeKind::Table).collect();
    if model.truncated || tables.len() > MAX_DIFF_TABLES {
        return Err(DbError::invalid(format!("Schema diff supports up to {MAX_DIFF_TABLES} tables per side.")));
    }
    // Futures are built up front: a mapping closure inside the stream trips
    // the `Send` check of the command future (higher-ranked lifetimes).
    let reads: Vec<_> = tables.iter().map(|t| diff_table(session, &t.object)).collect();
    stream::iter(reads).buffered(DIFF_READ_CONCURRENCY).try_collect().await
}

async fn diff_table(session: &dyn Session, object: &ObjectRef) -> DbResult<DiffTable> {
    let (structure, keys) = tokio::try_join!(ops::structure(session, object), session.key_roles(object))?;
    Ok(DiffTable { structure, keys })
}

/// Compares two schemas, possibly on different connections, and plans the SQL
/// that makes `target` match `source`. Opens its own sessions and closes them.
#[tauri::command]
pub async fn schema_diff(state: State<'_, AppState>, source: DiffSide, target: DiffSide) -> CmdResult<SchemaDiff> {
    let open_side = |side: DiffSide| {
        let state = &state;
        async move {
            let connection = state.connection(&side.connection_id).await?;
            let secrets = connector::saved_secrets(state, &connection).await?;
            let opened = connector::open(state, &connection.config, &secrets).await?;
            let result = snapshot(&*opened.session, &side).await;
            let flavor = opened.session.dialect().flavor();
            opened.session.close().await;
            DbResult::Ok((result?, flavor))
        }
    };
    let ((source_tables, flavor), (target_tables, target_flavor)) =
        tokio::try_join!(open_side(source.clone()), open_side(target.clone()))?;
    if flavor != target_flavor {
        return Err(DbError::invalid(
            "Both sides must use the same query language (e.g. PostgreSQL, MySQL/MariaDB or CQL for Cassandra/ScyllaDB).",
        ));
    }
    let container = |side: &DiffSide| Container { database: side.database.clone(), schema: side.schema.clone() };
    dq_core::diff::diff(
        dq_core::sql::dialect::dialect_for(flavor),
        &source_tables,
        &container(&source),
        &target_tables,
        &container(&target),
    )
}

/// Migration tables of known frameworks in a database or schema.
#[tauri::command]
pub async fn framework_migrations(
    state: State<'_, AppState>,
    session_id: String,
    database: String,
    schema: Option<String>,
) -> CmdResult<Vec<FrameworkMigrations>> {
    let open = state.session(&session_id).await?;
    let model = open.session.schema_model(&database, schema.as_deref()).await?;
    dq_core::migrations::inspect(&*open.session, &model).await
}
