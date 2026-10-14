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

/// Prepares `sql` and binds `params` by name: `:1` takes the first value
/// wherever it appears, as the parameter dialog promises. (Oracle binds
/// positional parameters in order of appearance instead.)
pub fn prepare(conn: &Connection, sql: &str, params: &[Option<String>]) -> oracle::Result<oracle::Statement> {
    let mut statement = conn.statement(sql).build()?;
    let names: Vec<String> = statement.bind_names().iter().map(|n| (*n).to_owned()).collect();
    for name in names {
        let value = name.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|i| params.get(i));
        // Named binds without a value stay unbound and the server reports them.
        if let Some(value) = value {
            statement.bind(name.as_str(), value)?;
        }
    }
    Ok(statement)
}

pub fn first_row(conn: &Connection, sql: &str, params: &[&dyn ToSql]) -> DbResult<Option<TextRow>> {
    Ok(text_rows(conn, sql, params)?.into_iter().next())
}
