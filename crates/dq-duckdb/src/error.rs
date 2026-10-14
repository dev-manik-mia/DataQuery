//! Maps `DuckDB` errors to actionable [`DbError`]s.

use dq_core::{DbError, ErrorKind};

pub fn map_error(e: &duckdb::Error) -> DbError {
    let message = e.to_string();
    // DuckDB prefixes messages with their category: `Catalog Error: …`.
    let (kind, action) = match message.split(':').next().unwrap_or_default().trim() {
        "Catalog Error" => (ErrorKind::NotFound, Some("Check the name and its schema.")),
        "Parser Error" | "Binder Error" | "Conversion Error" => (ErrorKind::Syntax, None),
        "Constraint Error" => (ErrorKind::Constraint, None),
        "INTERRUPT Error" => (ErrorKind::Cancelled, None),
        "Permission Error" => (ErrorKind::Permission, Some("The database was opened read-only.")),
        "IO Error" => {
            (ErrorKind::Storage, Some("Check that the file exists and that no other program holds a write lock on it."))
        }
        "TransactionContext Error" => (ErrorKind::Server, Some("Roll back the failed transaction to continue.")),
        "Invalid Input Error" => (ErrorKind::InvalidInput, None),
        _ => (ErrorKind::Server, None),
    };
    DbError::new(kind, message).with_action(action)
}
