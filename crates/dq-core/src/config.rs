//! Connection settings. Secrets are never part of these types.

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use url::Url;

use crate::engine::Engine;
use crate::error::{DbError, DbResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export)]
pub enum Environment {
    #[default]
    Local,
    Development,
    Staging,
    Production,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum SslMode {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    #[default]
    VerifyFull,
}

impl SslMode {
    pub fn verifies_chain(self) -> bool {
        matches!(self, Self::VerifyCa | Self::VerifyFull)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Prefer => "prefer",
            Self::Require => "require",
            Self::VerifyCa => "verify-ca",
            Self::VerifyFull => "verify-full",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().replace('_', "-").as_str() {
            "disable" | "disabled" | "false" => Self::Disable,
            "prefer" | "preferred" | "allow" => Self::Prefer,
            "require" | "required" | "true" => Self::Require,
            "verify-ca" => Self::VerifyCa,
            "verify-full" | "verify-identity" => Self::VerifyFull,
            _ => return None,
        })
    }
}

/// Everything needed to open a connection, except the password.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ConnConfig {
    pub engine: Engine,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub ssl_mode: SslMode,
    pub connect_timeout_secs: u32,
}

impl ConnConfig {
    pub const MAX_TIMEOUT_SECS: u32 = 300;

    /// Validates every field before an adapter sees it.
    pub fn validate(&self) -> DbResult<()> {
        let host = self.host.trim();
        if host.is_empty() {
            return Err(DbError::invalid("Host is required."));
        }
        if host.len() > 255 || host.chars().any(|c| c.is_whitespace() || c.is_control() || c == '/') {
            return Err(DbError::invalid("Host contains characters that are not allowed."));
        }
        if self.port == 0 {
            return Err(DbError::invalid("Port must be between 1 and 65535."));
        }
        if self.username.trim().is_empty() {
            return Err(DbError::invalid("Username is required."));
        }
        if self.username.len() > 128 || self.database.len() > 128 {
            return Err(DbError::invalid("Username and database must be at most 128 characters."));
        }
        if self.username.chars().chain(self.database.chars()).any(char::is_control) {
            return Err(DbError::invalid("Username and database cannot contain control characters."));
        }
        if self.connect_timeout_secs == 0 || self.connect_timeout_secs > Self::MAX_TIMEOUT_SECS {
            return Err(DbError::invalid("Connect timeout must be between 1 and 300 seconds."));
        }
        Ok(())
    }

    /// `host:port/database` for display.
    pub fn address(&self) -> String {
        if self.database.is_empty() {
            format!("{}:{}", self.host, self.port)
        } else {
            format!("{}:{}/{}", self.host, self.port, self.database)
        }
    }
}

/// Result of parsing a connection URL. The password is returned separately so
/// the caller can move it straight into the keychain.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ParsedUrl {
    pub config: ConnConfig,
    pub password: Option<String>,
    pub name_suggestion: String,
}

/// Parses `postgres://`, `postgresql://`, `mysql://` and `mariadb://` URLs.
pub fn parse_connection_url(input: &str) -> DbResult<ParsedUrl> {
    let url = Url::parse(input.trim()).map_err(|_| DbError::invalid("This is not a valid connection URL."))?;
    let engine = match url.scheme() {
        "postgres" | "postgresql" => Engine::Postgres,
        "mysql" => Engine::Mysql,
        "mariadb" => Engine::Mariadb,
        "mongodb" | "mongodb+srv" => {
            return Err(DbError::unsupported("MongoDB connections are planned (P2)."));
        }
        other => {
            return Err(DbError::unsupported(format!("Unsupported URL scheme `{other}`.")));
        }
    };
    let decode = |s: &str| {
        url::form_urlencoded::parse(format!("v={s}").as_bytes()).next().map(|(_, v)| v.into_owned()).unwrap_or_default()
    };
    let host = url.host_str().ok_or_else(|| DbError::invalid("The URL has no host."))?;
    let host = host.trim_start_matches('[').trim_end_matches(']').to_owned();
    let database = decode(url.path().trim_start_matches('/'));
    let mut ssl_mode = SslMode::default();
    let mut connect_timeout_secs = 10;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "sslmode" | "ssl-mode" | "ssl_mode" => {
                ssl_mode =
                    SslMode::parse(&value).ok_or_else(|| DbError::invalid(format!("Unknown sslmode `{value}`.")))?;
            }
            "connect_timeout" => {
                connect_timeout_secs =
                    value.parse().map_err(|_| DbError::invalid("connect_timeout must be a number."))?;
            }
            _ => {}
        }
    }
    let config = ConnConfig {
        engine,
        port: url.port().or_else(|| engine.default_port()).unwrap_or(0),
        host: host.clone(),
        database: database.clone(),
        username: decode(url.username()),
        ssl_mode,
        connect_timeout_secs,
    };
    config.validate()?;
    let name_suggestion = if database.is_empty() { host } else { format!("{database}@{host}") };
    Ok(ParsedUrl { password: url.password().map(decode).filter(|p| !p.is_empty()), config, name_suggestion })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_postgres_url_and_splits_secret() {
        let p = parse_connection_url("postgres://app%40corp:p%40ss@db.local:6543/app_dev?sslmode=require").unwrap();
        assert_eq!(p.config.engine, Engine::Postgres);
        assert_eq!(p.config.username, "app@corp");
        assert_eq!(p.config.port, 6543);
        assert_eq!(p.config.database, "app_dev");
        assert_eq!(p.config.ssl_mode, SslMode::Require);
        assert_eq!(p.password.as_deref(), Some("p@ss"));
    }

    #[test]
    fn defaults_port_and_tls() {
        let p = parse_connection_url("mysql://root@localhost/shop").unwrap();
        assert_eq!(p.config.port, 3306);
        assert_eq!(p.config.ssl_mode, SslMode::VerifyFull);
        assert_eq!(p.password, None);
    }

    #[test]
    fn rejects_unknown_scheme() {
        assert!(parse_connection_url("redis://localhost").is_err());
        assert!(parse_connection_url("not a url").is_err());
    }

    #[test]
    fn validates_fields() {
        let mut c = parse_connection_url("postgres://u@h/db").unwrap().config;
        c.host = "bad host".into();
        assert!(c.validate().is_err());
    }
}
