//! Maps `tiberius` errors to actionable [`DbError`]s.

use std::fmt::Write;

use dq_core::redact::redact_known;
use dq_core::{DbError, ErrorKind};
use tiberius::error::{Error, IoErrorKind};

pub fn map_error(e: &Error, secret: Option<&str>) -> DbError {
    let clean = |s: &str| secret.map_or_else(|| s.to_owned(), |p| redact_known(s, p));
    match e {
        Error::Server(t) => {
            let (kind, action) = match t.code() {
                18456 => {
                    (ErrorKind::Auth, Some("Check the username and password. The saved password was not changed."))
                }
                4060 | 911 => (ErrorKind::NotFound, Some("Check the database name and that this login may use it.")),
                208 | 3701 | 15151 => (ErrorKind::NotFound, Some("Check the object name and its schema.")),
                229 | 230 | 262 | 297 | 300 | 916 | 1088 | 15247 | 6102 => {
                    (ErrorKind::Permission, Some("Your login lacks the permission for this action."))
                }
                3906 | 3908 => (ErrorKind::Permission, Some("The database is read-only.")),
                1222 => {
                    (ErrorKind::Timeout, Some("Another session holds a lock on this data. Retry, or check Monitoring."))
                }
                1205 => (ErrorKind::Server, Some("Chosen as a deadlock victim. Run the statement again.")),
                3980 | 3617 => (ErrorKind::Cancelled, None),
                515 | 547 | 2601 | 2627 | 2628 | 8152 => (ErrorKind::Constraint, None),
                102 | 105 | 156 | 170 | 207 | 241 | 245 | 8114 | 8115 | 4145 | 137 => (ErrorKind::Syntax, None),
                _ => (ErrorKind::Server, None),
            };
            let mut detail = format!("Msg {}, Level {}, State {}", t.code(), t.class(), t.state());
            if !t.procedure().is_empty() {
                let _ = write!(detail, ", Procedure {}", t.procedure());
            }
            if t.line() > 0 {
                let _ = write!(detail, ", Line {}", t.line());
            }
            DbError::new(kind, clean(t.message()))
                .with_code(t.code().to_string())
                .with_detail(Some(detail))
                .with_action(action)
        }
        Error::Tls(message) => DbError::new(ErrorKind::Tls, clean(&format!("TLS error: {message}"))).with_action(Some(
            "Check the server certificate, or choose SSL mode `require` to encrypt without verifying it.",
        )),
        Error::Io { kind, message } => {
            let kind = if *kind == IoErrorKind::TimedOut { ErrorKind::Timeout } else { ErrorKind::Network };
            DbError::new(kind, clean(&format!("Network error: {message}")))
        }
        Error::Routing { host, port } => DbError::new(
            ErrorKind::Network,
            format!("The server redirects connections to {host}:{port}. Connect to that address instead."),
        ),
        other => {
            let message = other.to_string();
            let kind = if message.contains("TLS") || message.contains("certificate") {
                ErrorKind::Tls
            } else {
                ErrorKind::Server
            };
            DbError::new(kind, clean(&message))
        }
    }
}
