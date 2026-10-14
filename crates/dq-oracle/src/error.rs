//! Maps driver errors (`ORA-`, `DPI-`) to actionable [`DbError`]s.

use dq_core::redact::redact_known;
use dq_core::{DbError, ErrorKind};

pub fn map_error(e: &oracle::Error, secret: Option<&str>) -> DbError {
    let clean = |s: &str| secret.map_or_else(|| s.to_owned(), |p| redact_known(s, p));
    // A DPI error (code 0) can carry an ORA- cause in its text.
    let Some(db) = e.db_error().filter(|d| d.code() != 0) else {
        let message = e.to_string();
        // DPI-1047: no client library; DPI-1067: call timeout.
        let (kind, action) = if message.contains("DPI-1047") {
            (ErrorKind::Unsupported, Some("Install Oracle Instant Client and restart DataQuery."))
        } else if message.contains("DPI-1067") || message.contains("ORA-03156") {
            (ErrorKind::Timeout, Some("Narrow the query or raise the statement timeout in Settings."))
        } else if message.contains("DPI-1080") || message.contains("DPI-1010") {
            (ErrorKind::Network, Some("The connection was lost. Run the statement again to reconnect."))
        } else {
            (ErrorKind::Server, None)
        };
        return DbError::new(kind, clean(&message)).with_action(action);
    };
    let code = db.code();
    let (kind, action) = match code {
        1017 | 1005 | 28000 | 28001 | 1045 => {
            (ErrorKind::Auth, Some("Check the username and password. The saved password was not changed."))
        }
        12514 | 12505 => (ErrorKind::NotFound, Some("Check the service name (for example FREEPDB1 or ORCLPDB1).")),
        12541 | 12543 | 12545 | 12547 | 3113 | 3114 | 3135 | 12537 => (ErrorKind::Network, None),
        12170 | 3136 => (ErrorKind::Timeout, None),
        1013 => (ErrorKind::Cancelled, None),
        942 | 4043 | 2289 | 4080 => (ErrorKind::NotFound, Some("Check the object name and its schema.")),
        1031 | 1749 | 1720 | 1924 | 41_400 => (ErrorKind::Permission, Some("Your account lacks the privilege for this action.")),
        16000 | 1456 => (ErrorKind::Permission, Some("This database or transaction is read-only.")),
        1 | 1400 | 1407 | 2290 | 2291 | 2292 | 12899 | 1438 => (ErrorKind::Constraint, None),
        54 | 30006 => (ErrorKind::Timeout, Some("Another session holds a lock on this data. Retry, or check Monitoring.")),
        60 => (ErrorKind::Server, Some("A deadlock was detected and this statement was rolled back. Run it again.")),
        900..=999 | 1722 | 1830 | 1840 | 1841 | 1843 | 1847 | 1858 | 1861 | 6550 | 24344 => (ErrorKind::Syntax, None),
        _ => (ErrorKind::Server, None),
    };
    // The message starts with `ORA-00942: `; the code travels separately.
    let message = db.message().split_once(": ").filter(|(head, _)| head.starts_with("ORA-")).map_or(db.message(), |(_, m)| m);
    let mut error = DbError::new(kind, clean(message.trim_end())).with_code(format!("ORA-{code:05}")).with_action(action);
    if kind == ErrorKind::Syntax && db.offset() > 0 {
        error.position = Some(db.offset() + 1);
    }
    error
}
