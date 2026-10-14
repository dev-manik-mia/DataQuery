//! Maps `mysql_async` errors to actionable [`DbError`]s.

use dq_core::redact::redact_known;
use dq_core::{DbError, ErrorKind};
use mysql_async::{Error, IoError};

pub fn map_error(e: &Error, secret: Option<&str>) -> DbError {
    let clean = |s: &str| secret.map_or_else(|| s.to_owned(), |p| redact_known(s, p));
    match e {
        Error::Server(s) => {
            let (kind, action) = match s.code {
                1045 | 1698 => {
                    (ErrorKind::Auth, Some("Check the username and password. The saved password was not changed."))
                }
                1049 => (ErrorKind::NotFound, Some("Check the database name.")),
                1146 => (ErrorKind::NotFound, Some("Check the table name and the database it lives in.")),
                1044 | 1142 | 1143 | 1227 | 1370 => {
                    (ErrorKind::Permission, Some("Your account lacks the privilege for this action."))
                }
                1792 | 1290 => (ErrorKind::Permission, Some("This connection or server is read-only.")),
                1317 => (ErrorKind::Cancelled, None),
                3024 | 1969 => {
                    (ErrorKind::Timeout, Some("Narrow the query or raise the statement timeout in Settings."))
                }
                1040 => {
                    (ErrorKind::Server, Some("The server has too many connections. Close idle sessions and retry."))
                }
                1048 | 1062 | 1216 | 1217 | 1451 | 1452 | 3819 | 4025 => (ErrorKind::Constraint, None),
                1054 | 1064 | 1065 | 1149 | 1166 | 1292 | 1366 | 1406 => (ErrorKind::Syntax, None),
                _ => (ErrorKind::Server, None),
            };
            DbError::new(kind, clean(&s.message))
                .with_code(s.code.to_string())
                .with_detail(Some(format!("SQLSTATE {}", s.state)))
                .with_action(action)
        }
        Error::Io(IoError::Tls(t)) => DbError::new(ErrorKind::Tls, clean(&format!("TLS error: {t}"))),
        // Certificate failures surface as I/O errors that wrap a rustls error.
        Error::Io(IoError::Io(io)) if io.get_ref().and_then(|e| e.downcast_ref::<rustls::Error>()).is_some() => {
            DbError::new(ErrorKind::Tls, clean(&format!("TLS error: {io}")))
        }
        Error::Io(IoError::Io(io)) => {
            let kind = if io.kind() == std::io::ErrorKind::TimedOut { ErrorKind::Timeout } else { ErrorKind::Network };
            DbError::new(kind, clean(&format!("Network error: {io}")))
        }
        other => {
            let message = other.to_string();
            let kind =
                if message.contains("TLS") || message.contains("SSL") { ErrorKind::Tls } else { ErrorKind::Network };
            DbError::new(kind, clean(&message))
        }
    }
}
