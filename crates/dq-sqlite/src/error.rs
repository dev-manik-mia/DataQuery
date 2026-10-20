//! Maps `SQLite` errors to actionable [`DbError`]s.

use dq_core::{DbError, ErrorKind};
use rusqlite::ErrorCode;

const BUSY: &str = "Another connection or program is writing to this file. Try again in a moment.";

/// Kind and advice from an `SQLite` error message alone, for errors that
/// arrive as text (remote services) or carry no primary code.
pub fn classify_message(message: &str) -> (ErrorKind, Option<&'static str>) {
    let lower = message.to_ascii_lowercase();
    if lower.contains("constraint failed") || lower.contains("sqlite_constraint") {
        (ErrorKind::Constraint, None)
    } else if lower.contains("no such ") {
        (ErrorKind::NotFound, Some("Check the name and its database."))
    } else if lower.contains("syntax error") || lower.contains("incomplete input") || lower.contains("parse error") {
        (ErrorKind::Syntax, None)
    } else if lower.contains("readonly") || lower.contains("read-only") || lower.contains("not authorized") {
        (ErrorKind::Permission, None)
    } else if lower.contains("interrupted") {
        (ErrorKind::Cancelled, None)
    } else if lower.contains("database is locked") || lower.contains("database is busy") {
        (ErrorKind::Server, Some(BUSY))
    } else {
        (ErrorKind::Server, None)
    }
}

pub fn map_error(e: &rusqlite::Error) -> DbError {
    let message = e.to_string();
    let (kind, action) = match e.sqlite_error_code() {
        Some(ErrorCode::OperationInterrupted) => (ErrorKind::Cancelled, None),
        Some(ErrorCode::ConstraintViolation) => (ErrorKind::Constraint, None),
        Some(ErrorCode::ReadOnly) => {
            (ErrorKind::Permission, Some("The database file is read-only, or the connection is."))
        }
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => (ErrorKind::Server, Some(BUSY)),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_messages() {
        let kind = |m: &str| classify_message(m).0;
        assert_eq!(kind("UNIQUE constraint failed: t.id: SQLITE_CONSTRAINT"), ErrorKind::Constraint);
        assert_eq!(kind("SQLite error: no such table: nope"), ErrorKind::NotFound);
        assert_eq!(kind("near \"SELEC\": syntax error"), ErrorKind::Syntax);
        assert_eq!(kind("attempt to write a readonly database"), ErrorKind::Permission);
        assert_eq!(kind("something else"), ErrorKind::Server);
    }
}
