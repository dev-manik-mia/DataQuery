//! Catalog queries read as text rows (blocking; call from a blocking thread).

use dq_core::DbResult;
pub use dq_core::result::TextRow;
use oracle::Connection;
use oracle::sql_type::{OracleType, ToSql};

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

/// Rows per fetch round trip the driver uses by default.
const DEFAULT_FETCH: u32 = 100;
const MAX_FETCH: u32 = 1_000;
/// Fetch buffers hold each column at its widest, for every row of a round
/// trip; rows per round trip keep them near this size.
const FETCH_BUFFER_BYTES: i64 = 4 << 20;

/// Describes a query's columns without running it: the widest a row can be,
/// and whether it has LONG, LOB, object or JSON columns (read whole, so
/// their size is unknown).
const DESCRIBE: &str = "DECLARE \
  c INTEGER := DBMS_SQL.OPEN_CURSOR; n INTEGER; d DBMS_SQL.DESC_TAB3; \
  width PLS_INTEGER := 0; whole PLS_INTEGER := 0; \
BEGIN \
  DBMS_SQL.PARSE(c, :1, DBMS_SQL.NATIVE); \
  DBMS_SQL.DESCRIBE_COLUMNS3(c, n, d); \
  DBMS_SQL.CLOSE_CURSOR(c); \
  FOR i IN 1 .. n LOOP \
    width := width + GREATEST(NVL(d(i).col_max_len, 0), 16); \
    IF d(i).col_type IN (8, 24, 109, 112, 113, 114, 119) THEN whole := 1; END IF; \
  END LOOP; \
  :2 := width; :3 := whole; \
EXCEPTION WHEN OTHERS THEN \
  IF DBMS_SQL.IS_OPEN(c) THEN DBMS_SQL.CLOSE_CURSOR(c); END IF; \
  RAISE; \
END;";

fn describe(conn: &Connection, sql: &str) -> oracle::Result<(i64, bool)> {
    let mut statement = conn.statement(DESCRIBE).build()?;
    statement.bind(1, &sql)?;
    statement.bind(2, &OracleType::Int64)?;
    statement.bind(3, &OracleType::Int64)?;
    statement.execute(&[])?;
    Ok((statement.bind_value::<_, i64>(2)?, statement.bind_value::<_, i64>(3)? != 0))
}

/// Rows per round trip, and rows sent with the execute, for a query read up
/// to `wanted` rows. Past the driver's 100 rows the columns are described
/// first (one round trip); results with whole-read columns, or that cannot
/// be described, keep the driver's defaults.
fn fetch_size(conn: &Connection, sql: &str, wanted: u32) -> (u32, Option<u32>) {
    if wanted <= DEFAULT_FETCH {
        return (wanted.max(1), None);
    }
    match describe(conn, sql) {
        Ok((width, false)) => {
            let rows = (FETCH_BUFFER_BYTES / width.max(1)).clamp(DEFAULT_FETCH.into(), MAX_FETCH.into()) as u32;
            let rows = rows.min(wanted);
            (rows, Some(rows))
        }
        _ => (DEFAULT_FETCH, None),
    }
}

/// Prepares `sql` and binds `params` by name: `:1` takes the first value
/// wherever it appears, as the parameter dialog promises. (Oracle binds
/// positional parameters in order of appearance instead.) A query that will
/// be read up to `rows` rows gets a fetch sized for that.
pub fn prepare(
    conn: &Connection,
    sql: &str,
    params: &[Option<String>],
    rows: Option<u32>,
) -> oracle::Result<oracle::Statement> {
    let mut statement = conn.statement(sql).build()?;
    if let Some(wanted) = rows.filter(|_| statement.is_query()) {
        let (fetch, prefetch) = fetch_size(conn, sql, wanted);
        let mut builder = conn.statement(sql);
        builder.fetch_array_size(fetch);
        if let Some(prefetch) = prefetch {
            builder.prefetch_rows(prefetch);
        }
        statement = builder.build()?;
    }
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
