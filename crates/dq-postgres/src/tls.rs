//! TLS for PostgreSQL connections, with the shared rustls settings
//! ([`dq_core::tls::client_config`]).

use dq_core::DbResult;
use dq_core::config::{SslMode, TlsFiles};
use tokio_postgres_rustls::MakeRustlsConnect;

pub fn connector(mode: SslMode, files: &TlsFiles) -> DbResult<MakeRustlsConnect> {
    Ok(MakeRustlsConnect::new(dq_core::tls::client_config(mode, files)?))
}
