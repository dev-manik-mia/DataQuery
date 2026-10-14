//! Opening a driver session and turning driver errors into user-facing ones.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dq_core::adapter::ConnectParams;
use dq_core::result::TextRow;
use dq_core::{DbError, DbResult, ErrorKind};
use scylla::client::session::Session as Driver;
use scylla::client::session_builder::SessionBuilder;
use scylla::errors::{
    DbError as ServerError, ExecutionError, NewSessionError, PrepareError, RequestAttemptError, TranslationError,
};
use scylla::policies::address_translator::{AddressTranslator, UntranslatedPeer};
use scylla::response::query_result::QueryResult;
use scylla::serialize::row::SerializeRow;
use scylla::value::Row;

use crate::tls;
use crate::values::text_of;

/// Routes every node through the SSH tunnel: the driver still learns the
/// topology, but all requests reach the tunnelled node, which coordinates them.
#[derive(Debug)]
struct Tunnel(SocketAddr);

#[async_trait]
impl AddressTranslator for Tunnel {
    async fn translate_address(&self, _peer: &UntranslatedPeer) -> Result<SocketAddr, TranslationError> {
        Ok(self.0)
    }
}

/// `host` may list several contact points (`a,b:9043`); each takes `port` unless it names one.
pub fn contact_points(host: &str, port: u16) -> Vec<String> {
    host.split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(|h| {
            let bare_ipv6 = h.matches(':').count() > 1 && !h.starts_with('[');
            let has_port = h.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) && !bare_ipv6;
            if has_port {
                h.to_owned()
            } else if bare_ipv6 {
                format!("[{h}]:{port}")
            } else {
                format!("{h}:{port}")
            }
        })
        .collect()
}

/// A single contact point on this machine (local Docker, port forwarding): the
/// nodes advertise addresses that are not reachable from here, so every node
/// is reached through that contact point, as with an SSH tunnel.
async fn loopback(points: &[String]) -> Option<SocketAddr> {
    let [point] = points else { return None };
    let host = point.rsplit_once(':').map_or(point.as_str(), |(h, _)| h).trim_matches(['[', ']']);
    if host != "localhost" && !host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback()) {
        return None;
    }
    tokio::net::lookup_host(point).await.ok()?.next()
}

pub async fn connect(params: &ConnectParams) -> DbResult<Driver> {
    dq_core::tls::install_crypto_provider();
    let config = &params.config;
    let mut builder = SessionBuilder::new()
        .connection_timeout(Duration::from_secs(u64::from(config.connect_timeout_secs.max(1))))
        .fetch_schema_metadata(false)
        .custom_identity(scylla::client::SelfIdentity::new().with_application_name("DataQuery"));
    let points = contact_points(&config.host, config.port);
    builder = match (params.endpoint, loopback(&points).await) {
        (Some(endpoint), _) | (None, Some(endpoint)) => {
            builder.known_node_addr(endpoint).address_translator(Arc::new(Tunnel(endpoint)))
        }
        (None, None) => builder.known_nodes(points),
    };
    if !config.username.trim().is_empty() {
        builder = builder.user(config.username.clone(), params.password().unwrap_or_default());
    }
    if let Some(dc) = config.property("datacenter") {
        builder = builder.prefer_datacenter(dc.to_owned());
    }
    if let Some(tls) = tls::client_config(config.ssl_mode, &config.tls_files)? {
        builder = builder.tls_context(Some(tls));
    }
    let keyspace = config.database.trim();
    if !keyspace.is_empty() {
        builder = builder.use_keyspace(keyspace, keyspace.chars().any(|c| c.is_ascii_uppercase()));
    }
    builder.build().await.map_err(session_error)
}

fn session_error(e: NewSessionError) -> DbError {
    let text = e.to_string();
    let lower = text.to_ascii_lowercase();
    if lower.contains("authentication") || lower.contains("bad credentials") || lower.contains("username") {
        return DbError::new(ErrorKind::Auth, format!("Authentication failed: {text}"));
    }
    if matches!(e, NewSessionError::UseKeyspaceError(_)) {
        return DbError::not_found(format!("The keyspace does not exist or cannot be used: {text}"));
    }
    if lower.contains("tls") || lower.contains("certificate") {
        return DbError::new(ErrorKind::Tls, text);
    }
    DbError::new(ErrorKind::Network, format!("Could not connect: {text}"))
}

/// The server's error inside a driver error, when there is one.
fn server_error(e: &ExecutionError) -> Option<(&ServerError, &str)> {
    let attempt = match e {
        ExecutionError::LastAttemptError(a) => a,
        ExecutionError::PrepareError(PrepareError::AllAttemptsFailed { first_attempt }) => first_attempt,
        _ => return None,
    };
    match attempt {
        RequestAttemptError::DbError(db, message) => Some((db, message.as_str())),
        _ => None,
    }
}

pub fn exec_error(e: &ExecutionError) -> DbError {
    if let Some((db, message)) = server_error(e) {
        let kind = match db {
            ServerError::SyntaxError => ErrorKind::Syntax,
            ServerError::Invalid | ServerError::AlreadyExists { .. } | ServerError::ConfigError => {
                ErrorKind::InvalidInput
            }
            ServerError::AuthenticationError => ErrorKind::Auth,
            ServerError::Unauthorized => ErrorKind::Permission,
            ServerError::ReadTimeout { .. } | ServerError::WriteTimeout { .. } => ErrorKind::Timeout,
            _ => ErrorKind::Server,
        };
        let error = DbError::new(kind, message);
        return match db {
            ServerError::Unavailable { .. } => error.with_action(Some(
                "Not enough replicas are up for this consistency level. Lower the consistency or check the cluster.",
            )),
            ServerError::ReadTimeout { .. } | ServerError::WriteTimeout { .. } => error.with_action(Some(
                "The replicas did not answer within the server timeout. Narrow the query to fewer partitions.",
            )),
            _ => error,
        };
    }
    match e {
        ExecutionError::RequestTimeout(d) => DbError::new(
            ErrorKind::Timeout,
            format!("The statement did not finish within {} s and was abandoned.", d.as_secs()),
        ),
        ExecutionError::BadQuery(b) => DbError::invalid(b.to_string()),
        ExecutionError::ConnectionPoolError(_) | ExecutionError::EmptyPlan => {
            DbError::new(ErrorKind::Network, format!("No node could run the statement: {e}"))
        }
        other => DbError::new(ErrorKind::Server, other.to_string()),
    }
}

pub fn rows_error(e: impl std::fmt::Display) -> DbError {
    DbError::internal(format!("Could not read the result: {e}"))
}

/// Every row of a result, untyped.
pub fn rows(result: QueryResult) -> DbResult<Vec<Row>> {
    if !result.is_rows() {
        return Ok(Vec::new());
    }
    let rows = result.into_rows_result().map_err(rows_error)?;
    rows.rows::<Row>().map_err(rows_error)?.collect::<Result<Vec<_>, _>>().map_err(rows_error)
}

/// Runs a catalog query and reads every value as text (collections as JSON).
pub async fn text_rows(driver: &Driver, cql: &str, values: impl SerializeRow) -> DbResult<Vec<TextRow>> {
    let result = driver.query_unpaged(cql, values).await.map_err(|e| exec_error(&e))?;
    Ok(rows(result)?
        .into_iter()
        .map(|r| TextRow(r.columns.into_iter().map(|c| c.map(|v| text_of(&v))).collect()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::contact_points;

    #[test]
    fn contact_points_take_the_default_port() {
        assert_eq!(contact_points("a, b:9043,,", 9042), ["a:9042", "b:9043"]);
        assert_eq!(contact_points("::1", 9042), ["[::1]:9042"]);
        assert_eq!(contact_points("[::1]:9999", 9042), ["[::1]:9999"]);
    }
}
