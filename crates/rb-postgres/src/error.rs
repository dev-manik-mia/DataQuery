//! Maps driver errors to actionable [`DbError`]s.

use std::error::Error as _;

use rb_core::redact::redact_known;
use rb_core::{DbError, ErrorKind};
use tokio_postgres::error::{DbError as PgDbError, ErrorPosition};

pub fn map_error(e: &tokio_postgres::Error, secret: Option<&str>) -> DbError {
    let clean = |s: &str| secret.map_or_else(|| s.to_owned(), |p| redact_known(s, p));
    if let Some(db) = e.as_db_error() {
        return map_db_error(db, &clean);
    }
    let mut message = e.to_string();
    let mut kind = if e.is_closed() { ErrorKind::Network } else { ErrorKind::Server };
    let mut source = e.source();
    while let Some(err) = source {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            kind = if io.kind() == std::io::ErrorKind::TimedOut { ErrorKind::Timeout } else { ErrorKind::Network };
        }
        if err.downcast_ref::<rustls::Error>().is_some() {
            kind = ErrorKind::Tls;
        }
        message = format!("{message}: {err}");
        source = err.source();
    }
    if message.contains("timed out") {
        kind = ErrorKind::Timeout;
    }
    if message.contains("server does not support TLS") {
        kind = ErrorKind::Tls;
        return DbError::new(kind, clean(&message)).with_action(Some(
            "The server does not accept TLS. Choose SSL mode `disable` only if this network is trusted.",
        ));
    }
    DbError::new(kind, clean(&message))
}

fn map_db_error(db: &PgDbError, clean: &dyn Fn(&str) -> String) -> DbError {
    let code = db.code().code();
    let timeout = code == "57014" && db.message().contains("statement timeout");
    let kind = match code {
        _ if timeout => ErrorKind::Timeout,
        "57014" => ErrorKind::Cancelled,
        "42501" | "25006" => ErrorKind::Permission,
        "3D000" | "42P01" | "42883" | "3F000" => ErrorKind::NotFound,
        c if c.starts_with("28") => ErrorKind::Auth,
        c if c.starts_with("08") => ErrorKind::Network,
        c if c.starts_with("23") => ErrorKind::Constraint,
        c if c.starts_with("42") || c.starts_with("22") => ErrorKind::Syntax,
        _ => ErrorKind::Server,
    };
    let action = match code {
        _ if timeout => Some("Narrow the query or raise the statement timeout in Settings."),
        "28P01" | "28000" => Some("Check the username and password. The saved password was not changed."),
        "3D000" => Some("Check the database name."),
        "42P01" => Some("Check the table name and the schema it lives in."),
        "42501" => Some("Your role lacks the privilege for this action."),
        "25006" => Some("This connection is read-only."),
        "53300" => Some("The server has no free connection slots. Close idle sessions and retry."),
        _ => None,
    };
    let position = match db.position() {
        Some(ErrorPosition::Original(p)) => Some(*p),
        _ => None,
    };
    DbError::new(kind, clean(db.message()))
        .with_code(code)
        .with_detail(db.detail().map(clean))
        .with_hint(db.hint().map(clean))
        .with_action(action)
        .with_position(position)
}
