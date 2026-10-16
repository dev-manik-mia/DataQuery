//! Maps `SQLite` errors to actionable [`DbError`]s.

use dq_core::{DbError, ErrorKind};
use rusqlite::ErrorCode;

pub fn map_error(e: &rusqlite::Error) -> DbError {
    let message = e.to_string();
    let (kind, action) = match e.sqlite_error_code() {
        Some(ErrorCode::OperationInterrupted) => (ErrorKind::Cancelled, None),
        Some(ErrorCode::ConstraintViolation) => (ErrorKind::Constraint, None),
        Some(ErrorCode::ReadOnly) => {
            (ErrorKind::Permission, Some("The database file is read-only, or the connection is."))
        }
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => {
            (ErrorKind::Server, Some("Another connection or program is writing to this file. Try again in a moment."))
        }
        Some(ErrorCode::CannotOpen | ErrorCode::PermissionDenied) => {
            (ErrorKind::Storage, Some("Check that the file exists and that you may read (and write) it."))
        }
        Some(ErrorCode::NotADatabase) => (ErrorKind::Storage, Some("Choose a SQLite database file.")),
        Some(ErrorCode::DiskFull | ErrorCode::SystemIoFailure | ErrorCode::DatabaseCorrupt) => {
            (ErrorKind::Storage, None)
        }
        _ if message.starts_with("no such ") => (ErrorKind::NotFound, Some("Check the name and its database.")),
        _ if message.contains("syntax error") || message.contains("incomplete input") => (ErrorKind::Syntax, None),
        Some(ErrorCode::TooBig | ErrorCode::TypeMismatch) | None => (ErrorKind::InvalidInput, None),
        _ => (ErrorKind::Server, None),
    };
    let mut error = DbError::new(kind, message).with_action(action);
    if let rusqlite::Error::SqliteFailure(f, _) = e {
        error = error.with_code(f.extended_code.to_string());
    }
    error
}
