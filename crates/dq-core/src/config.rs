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

/// Certificate files for advanced TLS. Only the paths are stored; the files are
/// read when a connection opens.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(default)]
#[ts(export)]
pub struct TlsFiles {
    /// PEM bundle of trusted CA certificates, used instead of the system roots.
    pub ca_file: Option<String>,
    /// PEM client certificate chain, for servers that require client certificates.
    pub client_cert_file: Option<String>,
    /// PEM private key for the client certificate.
    pub client_key_file: Option<String>,
}

impl TlsFiles {
    pub fn is_empty(&self) -> bool {
        self.ca_file.is_none() && self.client_cert_file.is_none() && self.client_key_file.is_none()
    }

    fn validate(&self) -> DbResult<()> {
        for path in [&self.ca_file, &self.client_cert_file, &self.client_key_file].into_iter().flatten() {
            validate_path(path, "Certificate files")?;
        }
        if self.client_cert_file.is_some() != self.client_key_file.is_some() {
            return Err(DbError::invalid("A client certificate needs both the certificate and the key file."));
        }
        Ok(())
    }
}

/// How an SSH hop authenticates. Secrets (password, key passphrase) live in the keychain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "method", rename_all = "snake_case")]
#[ts(export)]
pub enum SshAuth {
    Password,
    /// OpenSSH or PEM private key file; an optional passphrase is kept in the keychain.
    PrivateKey {
        path: String,
    },
    /// Keys offered by the running SSH agent (`SSH_AUTH_SOCK`).
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SshHop {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuth,
}

/// An SSH tunnel to the database: `jumps` are visited in order, then `host`
/// forwards the connection to the database host and port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SshTunnel {
    pub host: SshHop,
    #[serde(default)]
    pub jumps: Vec<SshHop>,
}

impl SshTunnel {
    pub const MAX_JUMPS: usize = 4;

    /// Hops in connection order: jump hosts first, the tunnel host last.
    pub fn hops(&self) -> impl Iterator<Item = &SshHop> {
        self.jumps.iter().chain(std::iter::once(&self.host))
    }

    fn validate(&self) -> DbResult<()> {
        if self.jumps.len() > Self::MAX_JUMPS {
            return Err(DbError::invalid(format!("At most {} jump hosts are supported.", Self::MAX_JUMPS)));
        }
        for hop in self.hops() {
            validate_host(&hop.host, "SSH host")?;
            if hop.port == 0 {
                return Err(DbError::invalid("SSH port must be between 1 and 65535."));
            }
            if hop.username.trim().is_empty() || hop.username.len() > 128 || hop.username.chars().any(char::is_control)
            {
                return Err(DbError::invalid("Each SSH host needs a username of at most 128 characters."));
            }
            if let SshAuth::PrivateKey { path } = &hop.auth {
                validate_path(path, "SSH key files")?;
            }
        }
        Ok(())
    }
}

/// Where the database password comes from.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(tag = "method", rename_all = "snake_case")]
#[ts(export)]
pub enum AuthMethod {
    /// The password saved in the keychain (or none).
    #[default]
    Password,
    /// A short-lived RDS IAM token from the AWS CLI credential chain.
    AwsIam { region: String, profile: Option<String> },
    /// An Azure AD access token from the Azure CLI (`az login`).
    AzureAd,
    /// A Cloud SQL IAM login token from the Google Cloud CLI (`gcloud auth login`).
    GcpIam,
}

impl AuthMethod {
    pub fn is_cloud(&self) -> bool {
        !matches!(self, Self::Password)
    }
}

/// Network and pool behaviour of a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(default)]
#[ts(export)]
pub struct ConnOptions {
    /// TCP keepalive idle time in seconds; 0 turns keepalives off.
    pub keepalive_secs: u32,
    /// Extra connect attempts after a network error or timeout, with backoff.
    pub connect_retries: u32,
    /// Disconnect after this many idle minutes; 0 keeps the session open.
    pub idle_timeout_mins: u32,
    /// Maximum catalog connections used by the explorer, table views and exports.
    pub pool_size: u32,
}

impl Default for ConnOptions {
    fn default() -> Self {
        Self { keepalive_secs: 60, connect_retries: 0, idle_timeout_mins: 0, pool_size: 4 }
    }
}

impl ConnOptions {
    fn validate(self) -> DbResult<()> {
        if self.keepalive_secs > 7200 {
            return Err(DbError::invalid("Keepalive must be at most 7200 seconds."));
        }
        if self.connect_retries > 10 {
            return Err(DbError::invalid("At most 10 connect retries are allowed."));
        }
        if self.idle_timeout_mins > 24 * 60 {
            return Err(DbError::invalid("Idle timeout must be at most 1440 minutes."));
        }
        if !(1..=32).contains(&self.pool_size) {
            return Err(DbError::invalid("Pool size must be between 1 and 32."));
        }
        Ok(())
    }
}

/// Everything needed to open a connection, except secrets.
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
    #[serde(default)]
    pub tls_files: TlsFiles,
    #[serde(default)]
    pub ssh: Option<SshTunnel>,
    /// Enforced by the server session (read-only transactions), not only the UI.
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub options: ConnOptions,
    #[serde(default)]
    pub auth: AuthMethod,
}

impl Default for ConnConfig {
    fn default() -> Self {
        Self {
            engine: Engine::Postgres,
            host: "localhost".into(),
            port: 5432,
            database: String::new(),
            username: String::new(),
            ssl_mode: SslMode::default(),
            connect_timeout_secs: 10,
            tls_files: TlsFiles::default(),
            ssh: None,
            read_only: false,
            options: ConnOptions::default(),
            auth: AuthMethod::default(),
        }
    }
}

fn validate_host(host: &str, what: &str) -> DbResult<()> {
    let host = host.trim();
    if host.is_empty() {
        return Err(DbError::invalid(format!("{what} is required.")));
    }
    if host.len() > 255 || host.chars().any(|c| c.is_whitespace() || c.is_control() || c == '/') {
        return Err(DbError::invalid(format!("{what} contains characters that are not allowed.")));
    }
    Ok(())
}

fn validate_path(path: &str, what: &str) -> DbResult<()> {
    let p = std::path::Path::new(path);
    if path.len() > 4096 || path.chars().any(char::is_control) || !p.is_absolute() {
        return Err(DbError::invalid(format!("{what} must be chosen with the file dialog (absolute paths).")));
    }
    Ok(())
}

impl ConnConfig {
    pub const MAX_TIMEOUT_SECS: u32 = 300;

    /// Validates every field before an adapter sees it.
    pub fn validate(&self) -> DbResult<()> {
        validate_host(&self.host, "Host")?;
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
        self.tls_files.validate()?;
        if let Some(ssh) = &self.ssh {
            ssh.validate()?;
        }
        if let AuthMethod::AwsIam { region, profile } = &self.auth {
            let ok = |s: &str| {
                !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
            };
            if !ok(region) || profile.as_deref().is_some_and(|p| !ok(p)) {
                return Err(DbError::invalid("Enter an AWS region such as eu-west-1 (and an optional profile name)."));
            }
        }
        if self.auth.is_cloud() && self.ssl_mode == SslMode::Disable {
            return Err(DbError::invalid(
                "Cloud IAM authentication requires TLS. Choose an SSL mode other than disable.",
            ));
        }
        self.options.validate()
    }

    /// True when TLS is off or the certificate is not verified on a host that is not this machine.
    pub fn insecure_reason(&self) -> Option<&'static str> {
        let local = matches!(self.host.as_str(), "localhost" | "127.0.0.1" | "::1") && self.ssh.is_none();
        if local {
            return None;
        }
        match self.ssl_mode {
            SslMode::Disable if self.ssh.is_some() => None,
            SslMode::Disable => Some("TLS is off: passwords and data cross the network unencrypted."),
            SslMode::Prefer => Some("TLS is optional: the connection may fall back to plain text."),
            SslMode::Require => {
                Some("The server certificate is not verified, so the server's identity is not checked.")
            }
            SslMode::VerifyCa | SslMode::VerifyFull => None,
        }
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

/// Parses `postgres://`, `postgresql://`, `cockroachdb://`, `mysql://`,
/// `mariadb://`, `sqlserver://` (or `mssql://`) and `oracle://` URLs. For
/// Oracle the path is the service name.
pub fn parse_connection_url(input: &str) -> DbResult<ParsedUrl> {
    let url = Url::parse(input.trim()).map_err(|_| DbError::invalid("This is not a valid connection URL."))?;
    let engine = match url.scheme() {
        "postgres" | "postgresql" => Engine::Postgres,
        "cockroachdb" | "cockroach" => Engine::Cockroachdb,
        "mysql" => Engine::Mysql,
        "mariadb" => Engine::Mariadb,
        "sqlserver" | "mssql" => Engine::Sqlserver,
        "oracle" => Engine::Oracle,
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
    let mut tls_files = TlsFiles::default();
    // SQL Server spells TLS as `encrypt` and `trustServerCertificate`.
    let mut encrypt: Option<bool> = None;
    let mut trust_server_certificate = false;
    for (key, value) in url.query_pairs() {
        let truthy = matches!(value.to_ascii_lowercase().as_str(), "true" | "yes" | "1" | "strict" | "mandatory");
        match key.as_ref() {
            "encrypt" => encrypt = Some(truthy),
            "trustServerCertificate" | "trustservercertificate" => trust_server_certificate = truthy,
            "sslmode" | "ssl-mode" | "ssl_mode" => {
                ssl_mode =
                    SslMode::parse(&value).ok_or_else(|| DbError::invalid(format!("Unknown sslmode `{value}`.")))?;
            }
            "sslrootcert" | "ssl-ca" => tls_files.ca_file = Some(value.into_owned()),
            "sslcert" | "ssl-cert" => tls_files.client_cert_file = Some(value.into_owned()),
            "sslkey" | "ssl-key" => tls_files.client_key_file = Some(value.into_owned()),
            "connect_timeout" => {
                connect_timeout_secs =
                    value.parse().map_err(|_| DbError::invalid("connect_timeout must be a number."))?;
            }
            _ => {}
        }
    }
    match encrypt {
        Some(false) => ssl_mode = SslMode::Disable,
        _ if trust_server_certificate => ssl_mode = SslMode::Require,
        _ => {}
    }
    let config = ConnConfig {
        engine,
        port: url.port().or_else(|| engine.default_port()).unwrap_or(0),
        host: host.clone(),
        database: database.clone(),
        username: decode(url.username()),
        ssl_mode,
        connect_timeout_secs,
        tls_files,
        ..ConnConfig::default()
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
    fn parses_sqlserver_oracle_and_cockroach_urls() {
        let p = parse_connection_url("sqlserver://sa:secret@sql.local/sales?trustServerCertificate=true").unwrap();
        assert_eq!((p.config.engine, p.config.port, p.config.ssl_mode), (Engine::Sqlserver, 1433, SslMode::Require));
        let p = parse_connection_url("mssql://sa@sql.local:14330/sales?encrypt=false").unwrap();
        assert_eq!((p.config.port, p.config.ssl_mode), (14330, SslMode::Disable));
        let p = parse_connection_url("oracle://app:pw@ora.local/ORCLPDB1").unwrap();
        assert_eq!((p.config.engine, p.config.port, p.config.database.as_str()), (Engine::Oracle, 1521, "ORCLPDB1"));
        let p = parse_connection_url("cockroachdb://root@crdb.local/defaultdb?sslmode=disable").unwrap();
        assert_eq!((p.config.engine, p.config.port), (Engine::Cockroachdb, 26257));
    }

    #[test]
    fn rejects_unknown_scheme() {
        assert!(parse_connection_url("redis://localhost").is_err());
        assert!(parse_connection_url("not a url").is_err());
    }

    #[test]
    fn reads_tls_file_parameters() {
        let p = parse_connection_url("postgres://u@h/db?sslmode=verify-ca&sslrootcert=/etc/ca.pem").unwrap();
        assert_eq!(p.config.tls_files.ca_file.as_deref(), Some("/etc/ca.pem"));
        assert!(parse_connection_url("postgres://u@h/db?sslrootcert=relative.pem").is_err());
    }

    #[test]
    fn validates_tunnels_options_and_iam() {
        let mut c = parse_connection_url("postgres://u@db.internal/db").unwrap().config;
        c.ssh = Some(SshTunnel {
            host: SshHop { host: "bastion".into(), port: 22, username: "ops".into(), auth: SshAuth::Agent },
            jumps: vec![],
        });
        assert!(c.validate().is_ok());
        assert_eq!(c.ssh.as_ref().unwrap().hops().count(), 1);
        c.options.pool_size = 0;
        assert!(c.validate().is_err());
        c.options = ConnOptions::default();
        c.auth = AuthMethod::AwsIam { region: "eu-west-1".into(), profile: None };
        assert!(c.validate().is_ok());
        c.ssl_mode = SslMode::Disable;
        assert!(c.validate().is_err());
    }

    #[test]
    fn insecure_connections_are_flagged() {
        let mut c = parse_connection_url("postgres://u@db.example.com/db?sslmode=require").unwrap().config;
        assert!(c.insecure_reason().is_some());
        c.ssl_mode = SslMode::VerifyFull;
        assert!(c.insecure_reason().is_none());
        c.host = "localhost".into();
        c.ssl_mode = SslMode::Disable;
        assert!(c.insecure_reason().is_none());
    }

    #[test]
    fn older_saved_configs_still_load() {
        let json = r#"{"engine":"mysql","host":"h","port":3306,"database":"d","username":"u","ssl_mode":"require","connect_timeout_secs":5}"#;
        let c: ConnConfig = serde_json::from_str(json).unwrap();
        assert_eq!(c.options, ConnOptions::default());
        assert!(!c.read_only && c.ssh.is_none() && c.auth == AuthMethod::Password);
    }

    #[test]
    fn validates_fields() {
        let mut c = parse_connection_url("postgres://u@h/db").unwrap().config;
        c.host = "bad host".into();
        assert!(c.validate().is_err());
    }
}
