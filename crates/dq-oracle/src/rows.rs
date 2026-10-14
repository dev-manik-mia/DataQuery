//! Catalog queries read as text rows (blocking; call from a blocking thread).

use dq_core::DbResult;
pub use dq_core::result::TextRow;
use oracle::Connection;
use oracle::sql_type::ToSql;

use crate::error::map_error;
use crate::values::cell_text;

pub fn text_rows(conn: &Connection, sql: &str, params: &[&dyn ToSql]) -> DbResult<Vec<TextRow>> {
    let rows = conn.query(sql, params).map_err(|e| map_error(&e, None))?;
    let mut out = Vec::new();
    for row in rows {
        let row = row.map_err(|e| map_error(&e, None))?;
        out.push(TextRow(row.sql_values().iter().map(cell_text).collect()));
    }
    Ok(out)
}

pub fn first_row(conn: &Connection, sql: &str, params: &[&dyn ToSql]) -> DbResult<Option<TextRow>> {
    Ok(text_rows(conn, sql, params)?.into_iter().next())
}
