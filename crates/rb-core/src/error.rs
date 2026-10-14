use serde::Serialize;
use ts_rs::TS;

use crate::redact::redact;

pub type DbResult<T> = Result<T, DbError>;

/// Coarse error category. The UI uses it to pick wording and recovery actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ErrorKind {
    Auth,
    Network,
    Tls,
    Timeout,
    Cancelled,
    Syntax,
    Permission,
    NotFound,
    Constraint,
    InvalidInput,
    Unsupported,
    Server,
    Storage,
    Keychain,
    Internal,
}

/// An actionable error that is safe to show to the user.
///
/// Messages are redacted on construction, so connection strings or passwords
/// that a driver echoes back never reach the UI, the logs or history.
#[derive(Debug, Clone, Serialize, TS, thiserror::Error)]
#[ts(export)]
#[error("{message}")]
pub struct DbError {
    pub kind: ErrorKind,
    /// SQLSTATE or driver-specific error code, e.g. `28P01` or `1045`.
    pub code: Option<String>,
    pub message: String,
    pub detail: Option<String>,
    /// Hint returned by the server.
    pub hint: Option<String>,
    /// Suggested next step written for the user.
    pub action: Option<String>,
    /// 1-based character offset into the statement, when the server reports one.
    pub position: Option<u32>,
}

impl DbError {
    pub fn new(kind: ErrorKind, message: impl AsRef<str>) -> Self {
        Self {
            kind,
            code: None,
            message: redact(message.as_ref()),
            detail: None,
            hint: None,
            action: default_action(kind).map(str::to_owned),
            position: None,
        }
    }

    pub fn invalid(message: impl AsRef<str>) -> Self {
        Self::new(ErrorKind::InvalidInput, message)
    }

    pub fn unsupported(message: impl AsRef<str>) -> Self {
        Self::new(ErrorKind::Unsupported, message)
    }

    pub fn internal(message: impl AsRef<str>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }

    pub fn not_found(message: impl AsRef<str>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    #[must_use]
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    #[must_use]
    pub fn with_detail(mut self, detail: Option<impl AsRef<str>>) -> Self {
        self.detail = detail.map(|d| redact(d.as_ref()));
        self
    }

    #[must_use]
    pub fn with_hint(mut self, hint: Option<impl AsRef<str>>) -> Self {
        self.hint = hint.map(|h| redact(h.as_ref()));
        self
    }

    #[must_use]
    pub fn with_action(mut self, action: Option<impl Into<String>>) -> Self {
        if let Some(action) = action {
            self.action = Some(action.into());
        }
        self
    }

    #[must_use]
    pub fn with_position(mut self, position: Option<u32>) -> Self {
        self.position = position;
        self
    }
}

fn default_action(kind: ErrorKind) -> Option<&'static str> {
    Some(match kind {
        ErrorKind::Auth => "Check the username and password. The saved password was not changed.",
        ErrorKind::Network => "Check the host, port and that the server is reachable from this machine.",
        ErrorKind::Tls => "Check the SSL mode and the server certificate.",
        ErrorKind::Timeout => "The server did not answer in time. Try again or raise the timeout.",
        ErrorKind::Keychain => "Check that the system keychain is unlocked and RustBase may access it.",
        _ => return None,
    })
}
