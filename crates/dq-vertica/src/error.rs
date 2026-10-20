//! Maps server errors (`ErrorResponse` fields) and socket failures to
//! actionable [`DbError`]s.

use dq_core::{DbError, ErrorKind};

use crate::protocol::Fields;

/// An error the server reported. SQLSTATE codes are PostgreSQL's, plus
/// Vertica's own `42V01` (undefined table) and `V…` classes.
pub fn server_error(fields: &Fields) -> DbError {
    let code = fields.sqlstate();
    let read_only = fields.message().contains("read-only transaction");
    let kind = match code {
        _ if read_only => ErrorKind::Permission,
        "57014" => ErrorKind::Cancelled,
        "42501" | "25006" => ErrorKind::Permission,
        "3D000" | "42V01" | "42883" | "3F000" | "42704" => ErrorKind::NotFound,
        c if c.starts_with("28") => ErrorKind::Auth,
        c if c.starts_with("08") => ErrorKind::Network,
        c if c.starts_with("23") => ErrorKind::Constraint,
        c if c.starts_with("42") || c.starts_with("22") => ErrorKind::Syntax,
        _ => ErrorKind::Server,
    };
    let action = match code {
        "28000" | "28P01" => Some("Check the username and password. The saved password was not changed."),
        "3D000" => Some("Check the database name."),
        "42V01" => Some("Check the table name and the schema it lives in."),
        "42501" => Some("Your role lacks the privilege for this action."),
        _ if read_only => Some("This connection is read-only."),
        "53000" => Some("The server has too few resources for this statement. Narrow it or try later."),
        _ => None,
    };
    DbError::new(kind, fields.message())
        .with_code(code)
        .with_detail(fields.get(b'D'))
        .with_hint(fields.get(b'H'))
        .with_action(action)
        .with_position(fields.get(b'P').and_then(|p| p.parse().ok()))
}

pub fn io_error(e: &std::io::Error) -> DbError {
    let kind = match e.kind() {
        std::io::ErrorKind::TimedOut => ErrorKind::Timeout,
        _ if e.get_ref().is_some_and(|inner| inner.downcast_ref::<rustls::Error>().is_some()) => ErrorKind::Tls,
        _ => ErrorKind::Network,
    };
    DbError::new(kind, format!("Connection to the server failed: {e}"))
}

pub fn protocol_error(what: impl std::fmt::Display) -> DbError {
    DbError::new(ErrorKind::Network, format!("Unexpected reply from the server: {what}"))
        .with_action(Some("Check that host and port belong to a Vertica server, then reconnect."))
}

pub fn closed() -> DbError {
    DbError::new(ErrorKind::Network, "The connection to the server was lost.")
        .with_action(Some("Reconnect to run statements again."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlstates_pick_kind_and_action() {
        let fields = |code: &str| Fields(vec![(b'C', code.into()), (b'M', "m".into()), (b'P', "7".into())]);
        let e = server_error(&fields("42V01"));
        assert_eq!((e.kind, e.code.as_deref(), e.position), (ErrorKind::NotFound, Some("42V01"), Some(7)));
        assert!(e.action.is_some());
        assert_eq!(server_error(&fields("28000")).kind, ErrorKind::Auth);
        assert_eq!(server_error(&fields("42601")).kind, ErrorKind::Syntax);
        assert_eq!(server_error(&fields("57014")).kind, ErrorKind::Cancelled);
        assert_eq!(server_error(&fields("V2000")).kind, ErrorKind::Server);
    }
}
