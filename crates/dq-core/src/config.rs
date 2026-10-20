//! Connection settings. Secrets are never part of these types.

use std::collections::BTreeMap;

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

    /// Reads libpq, `MySQL` and JDBC spellings (`require`, `REQUIRED`, `verify_identity`, `true`…).
    pub fn parse(s: &str) -> Option<Self> {
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
    /// It may also hold the private key (one combined `.pem`).
    pub client_cert_file: Option<String>,
    /// PEM private key for the client certificate; empty when the certificate file holds it.
    pub client_key_file: Option<String>,
}

impl TlsFiles {
    pub fn is_empty(&self) -> bool {
        self.ca_file.is_none() && self.client_cert_file.is_none() && self.client_key_file.is_none()
    }

    /// The client certificate and key paths. Without a key file the certificate
    /// file is a combined `.pem` that holds the key too.
    pub fn client_pair(&self) -> Option<(&str, &str)> {
        let cert = self.client_cert_file.as_deref()?;
        Some((cert, self.client_key_file.as_deref().unwrap_or(cert)))
    }

    pub(crate) fn validate(&self) -> DbResult<()> {
        for path in [&self.ca_file, &self.client_cert_file, &self.client_key_file].into_iter().flatten() {
            validate_path(path, "Certificate files")?;
        }
        if self.client_key_file.is_some() && self.client_cert_file.is_none() {
            return Err(DbError::invalid("Choose the client certificate for this key."));
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
    /// Engine-specific settings, e.g. Snowflake's `warehouse` and `role`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[ts(type = "Record<string, string>")]
    pub properties: BTreeMap<String, String>,
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
            properties: BTreeMap::new(),
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

    /// Redis database index: empty means 0.
    pub fn database_index(&self) -> Option<u32> {
        let db = self.database.trim();
        if db.is_empty() { Some(0) } else { db.parse().ok() }
    }

    /// A property, trimmed; `None` when unset or blank.
    pub fn property(&self, key: &str) -> Option<&str> {
        self.properties.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
    }

    /// Validates a file-based connection: an absolute database path, or `:memory:`.
    fn validate_file(&self) -> DbResult<()> {
        let path = self.database.trim();
        if path.is_empty() {
            return Err(DbError::invalid("Choose a database file, or enter :memory: for a temporary database."));
        }
        if path != ":memory:" {
            validate_path(path, "The database file")?;
        }
        if self.ssh.is_some() {
            return Err(DbError::invalid("A local database file cannot use an SSH tunnel."));
        }
        Ok(())
    }

    /// `DynamoDB`: a region, an optional endpoint (e.g. `DynamoDB` Local), and an
    /// access key ID unless credentials come from an AWS CLI profile.
    fn validate_dynamodb(&self) -> DbResult<()> {
        let region = self.property("region").or(match &self.auth {
            AuthMethod::AwsIam { region, .. } => Some(region.as_str()),
            _ => None,
        });
        let region_ok = region.is_some_and(|r| {
            r.len() <= 32 && r.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        });
        if !region_ok {
            return Err(DbError::invalid("Enter an AWS region such as eu-west-1."));
        }
        if !self.host.trim().is_empty() {
            validate_host(&self.host, "Endpoint")?;
        }
        if matches!(self.auth, AuthMethod::Password) && self.username.trim().is_empty() {
            return Err(DbError::invalid("Enter an access key ID, or use an AWS CLI profile."));
        }
        Ok(())
    }

    fn validate_properties(&self) -> DbResult<()> {
        for (key, value) in &self.properties {
            let key_ok =
                !key.is_empty() && key.len() <= 64 && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !key_ok || value.len() > 512 || value.chars().any(char::is_control) {
                return Err(DbError::invalid(format!("The setting `{key}` is not valid.")));
            }
        }
        Ok(())
    }

    /// Validates every field before an adapter sees it.
    pub fn validate(&self) -> DbResult<()> {
        self.validate_properties()?;
        if self.engine.is_file_based() {
            if self.connect_timeout_secs == 0 || self.connect_timeout_secs > Self::MAX_TIMEOUT_SECS {
                return Err(DbError::invalid("Connect timeout must be between 1 and 300 seconds."));
            }
            return self.validate_file();
        }
        if self.engine == Engine::Dynamodb {
            self.validate_dynamodb()?;
        } else {
            validate_host(&self.host, "Host")?;
        }
        if self.port == 0 {
            return Err(DbError::invalid("Port must be between 1 and 65535."));
        }
        // MongoDB and Redis may run without authentication (Redis uses the `default` user);
        // DynamoDB checks its access key above.
        // Cassandra and ScyllaDB accept anonymous logins unless an authenticator is set.
        let optional_user = matches!(
            self.engine,
            Engine::Mongodb
                | Engine::Dynamodb
                | Engine::Redis
                | Engine::Valkey
                | Engine::Dragonfly
                | Engine::Cassandra
                | Engine::Scylladb
                | Engine::Elasticsearch
        );
        if self.username.trim().is_empty() && !optional_user {
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
        if self.engine.is_redis_like() && self.database_index().is_none() {
            return Err(DbError::invalid("The database is a number such as 0."));
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
        // DynamoDB signs requests (SigV4) instead of sending a token, so DynamoDB Local over HTTP is fine.
        if self.auth.is_cloud() && self.ssl_mode == SslMode::Disable && self.engine != Engine::Dynamodb {
            return Err(DbError::invalid(
                "Cloud IAM authentication requires TLS. Choose an SSL mode other than disable.",
            ));
        }
        self.options.validate()
    }

    /// True when TLS is off or the certificate is not verified on a host that is not this machine.
    pub fn insecure_reason(&self) -> Option<&'static str> {
        if self.engine.is_file_based() {
            return None;
        }
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

/// `duckdb:///absolute/path.duckdb`, `sqlite:///absolute/path.db` (or `sqlite3://`)
/// and `…://:memory:`. `None` for other schemes. Read by hand: `:memory:` is no URL authority.
fn parse_file_url(input: &str) -> DbResult<Option<ParsedUrl>> {
    let Some((scheme, rest)) = input.split_once("://") else { return Ok(None) };
    let engine = match scheme.to_ascii_lowercase().as_str() {
        "duckdb" => Engine::Duckdb,
        "sqlite" | "sqlite3" => Engine::Sqlite,
        _ => return Ok(None),
    };
    let path = url::form_urlencoded::parse(format!("v={}", rest.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default();
    let config = ConnConfig {
        engine,
        host: String::new(),
        port: 0,
        database: path.clone(),
        username: String::new(),
        ..ConnConfig::default()
    };
    config.validate()?;
    let name_suggestion =
        std::path::Path::new(&path).file_stem().map_or_else(|| path.clone(), |s| s.to_string_lossy().into_owned());
    Ok(Some(ParsedUrl { config, password: None, name_suggestion }))
}

/// Parses `postgres://`, `postgresql://`, `cockroachdb://`, `mysql://`,
/// `mariadb://`, `sqlserver://` (or `mssql://`), `oracle://`, `clickhouse://`,
/// `snowflake://`, `vertica://`, `duckdb://`, `sqlite://`, `mongodb://`, `mongodb+srv://`, `redis://`, `rediss://`, `valkey://`,
/// `valkeys://`, `dragonfly://` and `dragonflys://` URLs. For Oracle the path is the service name; for Snowflake it is `database/schema`; for
/// `DuckDB` and `SQLite` it is the file; for Redis it is the database number.
pub fn parse_connection_url(input: &str) -> DbResult<ParsedUrl> {
    // Several hosts are not a valid URL authority, so MongoDB URLs are read by hand.
    if input.trim().starts_with("mongodb://") || input.trim().starts_with("mongodb+srv://") {
        return parse_mongo_url(input.trim());
    }
    if let Some(parsed) = parse_file_url(input.trim())? {
        return Ok(parsed);
    }
    let url = Url::parse(input.trim()).map_err(|_| DbError::invalid("This is not a valid connection URL."))?;
    let engine = match url.scheme() {
        "postgres" | "postgresql" => Engine::Postgres,
        "cockroachdb" | "cockroach" => Engine::Cockroachdb,
        "mysql" => Engine::Mysql,
        "mariadb" => Engine::Mariadb,
        "sqlserver" | "mssql" => Engine::Sqlserver,
        "oracle" => Engine::Oracle,
        "clickhouse" => Engine::Clickhouse,
        "snowflake" => Engine::Snowflake,
        "vertica" => Engine::Vertica,
        "redis" | "rediss" => Engine::Redis,
        "neo4j" | "neo4j+s" | "neo4j+ssc" | "bolt" | "bolt+s" | "bolt+ssc" => Engine::Neo4j,
        "valkey" | "valkeys" => Engine::Valkey,
        "dragonfly" | "dragonflys" => Engine::Dragonfly,
        other => {
            return Err(DbError::unsupported(format!("Unsupported URL scheme `{other}`.")));
        }
    };
    let decode = |s: &str| {
        url::form_urlencoded::parse(format!("v={s}").as_bytes()).next().map(|(_, v)| v.into_owned()).unwrap_or_default()
    };
    let host = url.host_str().ok_or_else(|| DbError::invalid("The URL has no host."))?;
    let host = host.trim_start_matches('[').trim_end_matches(']').to_owned();
    let path = decode(url.path().trim_start_matches('/'));
    // Redis URLs name the database by number (`redis://host/2`).
    let path = if engine.is_redis_like() && path.is_empty() { "0".to_owned() } else { path };
    // Snowflake URLs carry the schema too: `/database/schema`.
    let (database, schema) = match path.split_once('/') {
        Some((db, schema)) if engine == Engine::Snowflake => (db.to_owned(), Some(schema.to_owned())),
        _ => (path, None),
    };
    let mut properties = BTreeMap::new();
    if let Some(schema) = schema.filter(|s| !s.is_empty()) {
        properties.insert("schema".to_owned(), schema);
    }
    let mut ssl_mode = SslMode::default();
    let mut connect_timeout_secs = 10;
    let mut tls_files = TlsFiles::default();
    // SQL Server spells TLS as `encrypt` and `trustServerCertificate`.
    // `rediss://`, `valkeys://`, `dragonflys://` and `neo4j+s://` mean TLS; the plain schemes do not.
    let mut encrypt: Option<bool> = match url.scheme() {
        "redis" | "valkey" | "dragonfly" | "neo4j" | "bolt" => Some(false),
        _ => None,
    };
    // `+ssc`: TLS with a self-signed certificate (not verified).
    let mut trust_server_certificate = url.scheme().ends_with("+ssc");
    for (key, value) in url.query_pairs() {
        let truthy = matches!(value.to_ascii_lowercase().as_str(), "true" | "yes" | "1" | "strict" | "mandatory");
        match key.as_ref() {
            // SQL Server `encrypt`, ClickHouse `secure`.
            "encrypt" | "secure" => encrypt = Some(truthy),
            "warehouse" | "role" | "account" => {
                properties.insert(key.into_owned(), value.into_owned());
            }
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
    // DataQuery talks to Neo4j over its HTTP Query API: Bolt URLs map to the HTTP
    // port (7474, or 7473 with TLS); an explicit non-Bolt port is kept.
    let port = match (engine, url.port()) {
        (Engine::Neo4j, None | Some(7687)) if ssl_mode == SslMode::Disable => Some(7474),
        (Engine::Neo4j, None | Some(7687)) => Some(7473),
        (_, port) => port,
    };
    let config = ConnConfig {
        engine,
        port: port.or_else(|| engine.default_port()).unwrap_or(0),
        host: host.clone(),
        database: database.clone(),
        username: decode(url.username()),
        ssl_mode,
        connect_timeout_secs,
        tls_files,
        properties,
        ..ConnConfig::default()
    };
    config.validate()?;
    let name_suggestion = if database.is_empty() { host } else { format!("{database}@{host}") };
    Ok(ParsedUrl { password: url.password().map(decode).filter(|p| !p.is_empty()), config, name_suggestion })
}

/// `mongodb://user:pw@h1:27017,h2:27018/db?replicaSet=rs0&authSource=admin&tls=true`
/// and `mongodb+srv://user:pw@cluster0.example.net/db`. Several hosts stay
/// comma-separated in `host`; driver options become properties.
fn parse_mongo_url(input: &str) -> DbResult<ParsedUrl> {
    let decode = |s: &str| {
        url::form_urlencoded::parse(format!("v={}", s.replace('+', "%2B")).as_bytes())
            .next()
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default()
    };
    let (srv, rest) = match input.strip_prefix("mongodb+srv://") {
        Some(rest) => (true, rest),
        None => (false, input.strip_prefix("mongodb://").unwrap_or(input)),
    };
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, database) = rest.split_once('/').unwrap_or((rest, ""));
    let (userinfo, hosts) = authority.rsplit_once('@').map_or(("", authority), |(u, h)| (u, h));
    let (user, password) = userinfo.split_once(':').map_or((userinfo, None), |(u, p)| (u, Some(p)));
    if hosts.is_empty() {
        return Err(DbError::invalid("The URL has no host."));
    }
    // One host keeps its port in `port`; several stay as `h1:p1,h2:p2`.
    let (host, port) = match hosts.split_once(',') {
        None => match hosts.rsplit_once(':').filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit())) {
            Some((h, p)) => (h.to_owned(), p.parse().map_err(|_| DbError::invalid("The port is not valid."))?),
            None => (hosts.to_owned(), 27017),
        },
        Some(_) => (hosts.to_owned(), 27017),
    };
    let mut properties = BTreeMap::new();
    if srv {
        properties.insert("srv".to_owned(), "true".to_owned());
    }
    let mut ssl_mode = if srv { SslMode::VerifyFull } else { SslMode::Disable };
    let mut connect_timeout_secs = 10;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let truthy = matches!(value.to_ascii_lowercase().as_str(), "true" | "1" | "yes");
        match key.to_ascii_lowercase().as_str() {
            "tls" | "ssl" => {
                ssl_mode = match (truthy, ssl_mode) {
                    (false, _) => SslMode::Disable,
                    (true, SslMode::Require) => SslMode::Require,
                    (true, _) => SslMode::VerifyFull,
                };
            }
            "tlsinsecure" | "tlsallowinvalidcertificates" if truthy => ssl_mode = SslMode::Require,
            "authsource" => _ = properties.insert("auth_source".to_owned(), value.into_owned()),
            "replicaset" => _ = properties.insert("replica_set".to_owned(), value.into_owned()),
            "readpreference" => _ = properties.insert("read_preference".to_owned(), value.into_owned()),
            "directconnection" => _ = properties.insert("direct_connection".to_owned(), value.into_owned()),
            "authmechanism" => _ = properties.insert("auth_mechanism".to_owned(), value.into_owned()),
            "connecttimeoutms" => {
                let ms: u32 = value.parse().map_err(|_| DbError::invalid("connectTimeoutMS must be a number."))?;
                connect_timeout_secs = ms.div_ceil(1000).max(1);
            }
            _ => {}
        }
    }
    let database = decode(database);
    let config = ConnConfig {
        engine: Engine::Mongodb,
        host: host.clone(),
        port,
        database: database.clone(),
        username: decode(user),
        ssl_mode,
        connect_timeout_secs,
        properties,
        ..ConnConfig::default()
    };
    config.validate()?;
    let first_host = host.split(',').next().unwrap_or_default().to_owned();
    let name_suggestion = if database.is_empty() { first_host } else { format!("{database}@{first_host}") };
    Ok(ParsedUrl { config, password: password.map(decode).filter(|p| !p.is_empty()), name_suggestion })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_neo4j_urls() {
        let p = parse_connection_url("neo4j+s://neo4j:pw@graph.example.com/movies").unwrap();
        assert_eq!((p.config.engine, p.config.port, p.config.ssl_mode), (Engine::Neo4j, 7473, SslMode::VerifyFull));
        assert_eq!((p.config.database.as_str(), p.password.as_deref()), ("movies", Some("pw")));
        let p = parse_connection_url("bolt://neo4j@localhost:7687").unwrap();
        assert_eq!((p.config.port, p.config.ssl_mode), (7474, SslMode::Disable));
        assert_eq!(parse_connection_url("neo4j://neo4j@localhost:57474").unwrap().config.port, 57474);
        assert_eq!(parse_connection_url("neo4j+ssc://a@h").unwrap().config.ssl_mode, SslMode::Require);
    }

    #[test]
    fn parses_redis_urls() {
        let p = parse_connection_url("rediss://default:s%40cret@cache.local:6380/2").unwrap();
        assert_eq!((p.config.engine, p.config.port, p.config.database.as_str()), (Engine::Redis, 6380, "2"));
        assert_eq!((p.config.ssl_mode, p.password.as_deref()), (SslMode::VerifyFull, Some("s@cret")));
        let p = parse_connection_url("valkey://localhost").unwrap();
        assert_eq!((p.config.engine, p.config.port, p.config.database.as_str()), (Engine::Valkey, 6379, "0"));
        assert_eq!((p.config.ssl_mode, p.config.username.as_str()), (SslMode::Disable, ""));
        assert!(parse_connection_url("redis://localhost/x").is_err());
        let p = parse_connection_url("dragonfly://:pw@cache.local:6380/3").unwrap();
        assert_eq!((p.config.engine, p.config.port, p.config.database.as_str()), (Engine::Dragonfly, 6380, "3"));
        assert_eq!((p.config.ssl_mode, p.password.as_deref()), (SslMode::Disable, Some("pw")));
    }

    #[test]
    fn parses_mongodb_urls() {
        let p = parse_connection_url(
            "mongodb://ana:p%40ss@h1:27018,h2:27019/shop?replicaSet=rs0&authSource=admin&tls=true",
        )
        .unwrap();
        assert_eq!((p.config.engine, p.config.host.as_str()), (Engine::Mongodb, "h1:27018,h2:27019"));
        assert_eq!((p.config.database.as_str(), p.config.ssl_mode), ("shop", SslMode::VerifyFull));
        assert_eq!((p.config.property("replica_set"), p.config.property("auth_source")), (Some("rs0"), Some("admin")));
        assert_eq!(p.password.as_deref(), Some("p@ss"));
        let p = parse_connection_url("mongodb+srv://ana@cluster0.example.net/app").unwrap();
        assert_eq!((p.config.property("srv"), p.config.ssl_mode), (Some("true"), SslMode::VerifyFull));
        let p = parse_connection_url("mongodb://localhost:57017").unwrap();
        assert_eq!((p.config.host.as_str(), p.config.port, p.config.ssl_mode), ("localhost", 57017, SslMode::Disable));
        assert_eq!(p.password, None);
    }

    #[test]
    fn document_engine_rules() {
        let dynamo = ConnConfig {
            engine: Engine::Dynamodb,
            host: String::new(),
            port: 443,
            username: "AKIAEXAMPLE".into(),
            properties: [("region".to_owned(), "eu-west-1".to_owned())].into(),
            ..ConnConfig::default()
        };
        dynamo.validate().unwrap();
        assert!(ConnConfig { properties: BTreeMap::new(), ..dynamo.clone() }.validate().is_err());
        assert!(ConnConfig { username: String::new(), ..dynamo.clone() }.validate().is_err());
        let profile = AuthMethod::AwsIam { region: "eu-west-1".into(), profile: Some("dev".into()) };
        ConnConfig { username: String::new(), auth: profile, ..dynamo }.validate().unwrap();
        let mongo = ConnConfig { engine: Engine::Mongodb, host: "a:1,b:2".into(), ..ConnConfig::default() };
        mongo.validate().unwrap();
    }

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
        let p = parse_connection_url("vertica://dbadmin@vt.local/VMart?sslmode=require").unwrap();
        assert_eq!((p.config.engine, p.config.port, p.config.database.as_str()), (Engine::Vertica, 5433, "VMart"));
        let p = parse_connection_url("clickhouse://default@ch.local/logs?secure=false").unwrap();
        assert_eq!((p.config.engine, p.config.port, p.config.ssl_mode), (Engine::Clickhouse, 8123, SslMode::Disable));
        let p = parse_connection_url("snowflake://ana:pw@myorg-acct/SALES/PUBLIC?warehouse=WH&role=ANALYST").unwrap();
        assert_eq!(
            (p.config.engine, p.config.host.as_str(), p.config.database.as_str()),
            (Engine::Snowflake, "myorg-acct", "SALES")
        );
        assert_eq!(
            (p.config.property("schema"), p.config.property("warehouse"), p.config.property("role")),
            (Some("PUBLIC"), Some("WH"), Some("ANALYST"))
        );
        let p = parse_connection_url("duckdb:///tmp/sales.duckdb").unwrap();
        assert_eq!(
            (p.config.engine, p.config.database.as_str(), p.name_suggestion.as_str()),
            (Engine::Duckdb, "/tmp/sales.duckdb", "sales")
        );
        let p = parse_connection_url("sqlite:///Users/ana/My%20Data/app.db").unwrap();
        assert_eq!(
            (p.config.engine, p.config.database.as_str(), p.name_suggestion.as_str()),
            (Engine::Sqlite, "/Users/ana/My Data/app.db", "app")
        );
        assert_eq!(parse_connection_url("sqlite3://:memory:").unwrap().config.database, ":memory:");
    }

    #[test]
    fn file_databases_need_a_path_not_a_host() {
        let mut c = ConnConfig {
            engine: Engine::Duckdb,
            host: String::new(),
            username: String::new(),
            ..ConnConfig::default()
        };
        assert!(c.validate().is_err());
        c.database = ":memory:".into();
        assert!(c.validate().is_ok());
        c.database = "relative.duckdb".into();
        assert!(c.validate().is_err());
        c.database = "/tmp/sales.duckdb".into();
        assert!(c.validate().is_ok() && c.insecure_reason().is_none());
        c.properties.insert("bad key".into(), "x".into());
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_unknown_scheme() {
        assert!(parse_connection_url("cassandra://localhost").is_err());
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
