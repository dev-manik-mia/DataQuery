//! Catalog queries read as text rows. Every value is converted to text by the
//! decoder, so the catalog code stays simple and exact.

use dq_core::DbResult;
pub use dq_core::result::TextRow;
use dq_core::value::CellLimits;
use futures_util::TryStreamExt;
use tiberius::{QueryItem, ToSql};

use crate::connect::Conn;
use crate::error::map_error;
use crate::values::cell_text;

/// Runs a catalog query with `@P1…` text parameters and returns its first result set.
pub async fn text_rows(conn: &mut Conn, sql: &str, params: &[&str], secret: Option<&str>) -> DbResult<Vec<TextRow>> {
    let err = |e: tiberius::error::Error| map_error(&e, secret);
    let binds: Vec<&dyn ToSql> = params.iter().map(|p| p as &dyn ToSql).collect();
    let mut stream = if binds.is_empty() {
        conn.simple_query(sql).await.map_err(err)?
    } else {
        conn.query(sql, &binds).await.map_err(err)?
    };
    let mut out = Vec::new();
    let mut seen_result = false;
    while let Some(item) = stream.try_next().await.map_err(err)? {
        match item {
            QueryItem::Metadata(_) if seen_result => break,
            QueryItem::Metadata(_) => seen_result = true,
            QueryItem::Row(row) => {
                out.push(TextRow(row.cells().map(|(_, data)| cell_text(data, CellLimits::FULL)).collect()));
            }
        }
    }
    drop(stream);
    Ok(out)
}

pub async fn first_row(conn: &mut Conn, sql: &str, params: &[&str], secret: Option<&str>) -> DbResult<Option<TextRow>> {
    Ok(text_rows(conn, sql, params, secret).await?.into_iter().next())
}
