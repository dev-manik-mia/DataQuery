//! One client connection: socket, TLS, startup and authentication, and the
//! simple and extended query flows. A result stays open on the socket while
//! its rows are read, so callers page through it with [`Connection::next_row`]
//! and must [`Connection::drain`] (or cancel and drain) before the next
//! statement.

use std::sync::Arc;
use std::time::Duration;

use dq_core::adapter::ConnectParams;
use dq_core::config::SslMode;
use dq_core::result::TextRow;
use dq_core::{DbError, DbResult, ErrorKind};
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream};
use tokio::net::TcpStream;

use crate::error::{closed, io_error, protocol_error, server_error};
use crate::protocol::{self, Backend, Column, Fields, auth};

/// A raw row: text values, `None` for NULL.
pub type RawRow = Vec<Option<Vec<u8>>>;

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// What a connection is used for; read-only roles run read-only transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Query,
    Catalog,
    Export,
    Write,
}

impl Role {
    fn label(self) -> &'static str {
        match self {
            Self::Query => "DataQuery",
            Self::Catalog => "DataQuery (catalog)",
            Self::Export => "DataQuery (export)",
            Self::Write => "DataQuery (edit)",
        }
    }
}

/// Everything needed to open connections and send cancel requests. Holds the
/// password in memory only.
pub struct Target {
    params: ConnectParams,
    tls: Option<Arc<ClientConfig>>,
}

impl Target {
    pub fn new(params: &ConnectParams) -> DbResult<Self> {
        let config = &params.config;
        let tls = match config.ssl_mode {
            SslMode::Disable => None,
            mode => Some(Arc::new(dq_core::tls::client_config(mode, &config.tls_files)?)),
        };
        Ok(Self { params: params.clone(), tls })
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(u64::from(self.params.config.connect_timeout_secs))
    }

    /// A socket to the server (or the SSH tunnel), with TLS when the SSL mode asks for it.
    async fn socket(&self) -> DbResult<(Box<dyn Io>, Option<String>)> {
        let config = &self.params.config;
        let (host, port) = self.params.socket_target();
        let tcp = tokio::time::timeout(self.timeout(), TcpStream::connect((host.as_str(), port)))
            .await
            .map_err(|_| {
                DbError::new(ErrorKind::Timeout, format!("Timed out connecting to {}:{}.", config.host, config.port))
            })?
            .map_err(|e| io_error(&e))?;
        let _ = tcp.set_nodelay(true);
        if config.options.keepalive_secs > 0 {
            let keepalive =
                socket2::TcpKeepalive::new().with_time(Duration::from_secs(u64::from(config.options.keepalive_secs)));
            let _ = socket2::SockRef::from(&tcp).set_tcp_keepalive(&keepalive);
        }
        let Some(tls) = &self.tls else {
            return Ok((Box::new(tcp), None));
        };
        let mut tcp = tcp;
        tcp.write_all(&protocol::ssl_request()).await.map_err(|e| io_error(&e))?;
        let answer = tcp.read_u8().await.map_err(|e| io_error(&e))?;
        if answer != b'S' {
            if config.ssl_mode == SslMode::Prefer {
                return Ok((Box::new(tcp), None));
            }
            return Err(DbError::new(ErrorKind::Tls, "The server does not accept TLS connections.").with_action(Some(
                "Enable TLS on the server, or choose SSL mode `disable` only if this network is trusted.",
            )));
        }
        let name = ServerName::try_from(config.host.clone())
            .map_err(|_| DbError::new(ErrorKind::Tls, format!("`{}` is not a valid TLS server name.", config.host)))?;
        let stream =
            tokio::time::timeout(self.timeout(), tokio_rustls::TlsConnector::from(tls.clone()).connect(name, tcp))
                .await
                .map_err(|_| DbError::new(ErrorKind::Timeout, "Timed out during the TLS handshake."))?
                .map_err(|e| DbError::new(ErrorKind::Tls, format!("TLS handshake failed: {e}")))?;
        let version = stream.get_ref().1.protocol_version().map(|v| format!("{v:?}").replace('_', "."));
        Ok((Box::new(stream), Some(version.unwrap_or_else(|| "TLS".into()))))
    }

    /// Asks the server to stop the statement running on the session with `key`.
    pub async fn cancel(&self, key: (u32, u32)) -> DbResult<()> {
        let (mut socket, _) = self.socket().await?;
        socket.write_all(&protocol::cancel_request(key.0, key.1)).await.map_err(|e| io_error(&e))?;
        let _ = socket.shutdown().await;
        Ok(())
    }

    /// Opens a session. `autocommit` off keeps every statement in a transaction until `COMMIT`.
    pub async fn connect(&self, role: Role, autocommit: bool) -> DbResult<Connection> {
        let deadline = self.timeout() + Duration::from_secs(2);
        let config = &self.params.config;
        let mut conn = tokio::time::timeout(deadline, self.handshake(role, autocommit))
            .await
            .map_err(|_| {
                DbError::new(ErrorKind::Timeout, format!("Timed out connecting to {}:{}.", config.host, config.port))
            })?
            .map_err(|e| match self.params.password() {
                Some(p) => e.without_secret(p),
                None => e,
            })?;
        if config.read_only || matches!(role, Role::Catalog | Role::Export) {
            conn.execute("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY").await?;
        }
        Ok(conn)
    }

    async fn handshake(&self, role: Role, autocommit: bool) -> DbResult<Connection> {
        let config = &self.params.config;
        let (socket, tls_version) = self.socket().await?;
        let mut conn = Connection {
            io: BufStream::with_capacity(64 * 1024, 16 * 1024, socket),
            key: (0, 0),
            protocol_version: 0,
            complex_types_on: false,
            tls_version,
            notices: Vec::new(),
            open: false,
            broken: false,
        };
        let pid = std::process::id().to_string();
        let version = env!("CARGO_PKG_VERSION");
        let startup = protocol::startup(&[
            ("user", &config.username),
            ("database", &config.database),
            ("client_label", role.label()),
            ("client_type", "DataQuery"),
            ("client_version", version),
            ("client_os", std::env::consts::OS),
            ("client_pid", &pid),
            ("autocommit", if autocommit { "on" } else { "off" }),
            ("binary_data_protocol", "0"),
            ("protocol_features", "{\"request_complex_types\":true}"),
            ("protocol_compat", "VER"),
            ("auth_category", if self.params.password().is_some() { "User" } else { "" }),
        ]);
        conn.send(&startup).await?;
        loop {
            match conn.read().await? {
                Backend::Authentication(request) => match request.code {
                    auth::OK | auth::PASSWORD_GRACE => {}
                    auth::CHANGE_PASSWORD => {
                        return Err(DbError::new(ErrorKind::Auth, "The password of this user has expired.")
                            .with_action(Some("Change it with vsql or ask the database administrator.")));
                    }
                    _ => {
                        let password = self.params.password().unwrap_or_default();
                        let answer =
                            protocol::password_response(&request, &config.username, password).ok_or_else(|| {
                                DbError::unsupported(format!(
                                    "The server asks for an authentication method DataQuery does not offer (code {}).",
                                    request.code
                                ))
                                .with_action(Some("Use password authentication (MD5 or SHA-512) for this user."))
                            })?;
                        conn.send(&protocol::password(&answer)).await?;
                    }
                },
                Backend::KeyData { pid, key } => conn.key = (pid, key),
                Backend::ReadyForQuery(_) => break,
                Backend::Error(fields) => return Err(server_error(&fields)),
                Backend::Notice(_) => {}
                other => return Err(protocol_error(format!("{other:?} during startup"))),
            }
        }
        Ok(conn)
    }
}

/// How a statement started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    /// Rows follow; read them with [`Connection::next_row`].
    Rows(Vec<Column>),
    /// The statement finished without rows. (Vertica's command tags are
    /// empty; DML reports its row count as an `OUTPUT` row.)
    Done,
}

pub struct Connection {
    io: BufStream<Box<dyn Io>>,
    key: (u32, u32),
    /// The protocol version in effect, from the server's `ParameterStatus`.
    protocol_version: u32,
    /// The server sends complex types' element fields (`request_complex_types`).
    complex_types_on: bool,
    tls_version: Option<String>,
    notices: Vec<String>,
    /// A result's rows (or the rest of its statement) are still on the socket.
    open: bool,
    /// The socket is unusable (I/O error or an unexpected message).
    broken: bool,
}

impl Connection {
    /// Process id and secret key for cancel requests.
    pub fn key(&self) -> (u32, u32) {
        self.key
    }

    pub fn tls_version(&self) -> Option<&str> {
        self.tls_version.as_deref()
    }

    pub fn is_broken(&self) -> bool {
        self.broken
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notices)
    }

    async fn send(&mut self, bytes: &[u8]) -> DbResult<()> {
        let result = async {
            self.io.write_all(bytes).await?;
            self.io.flush().await
        }
        .await;
        result.map_err(|e| {
            self.broken = true;
            io_error(&e)
        })
    }

    async fn read_frame(&mut self) -> std::io::Result<(u8, Vec<u8>)> {
        let tag = self.io.read_u8().await?;
        let len = self.io.read_u32().await? as usize;
        if !(4..=protocol::MAX_MESSAGE).contains(&len) {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("message of {len} bytes")));
        }
        let mut body = vec![0; len - 4];
        self.io.read_exact(&mut body).await?;
        Ok((tag, body))
    }

    /// The next backend message; notices are kept for the statement's result.
    async fn read(&mut self) -> DbResult<Backend> {
        if self.broken {
            return Err(closed());
        }
        loop {
            let (tag, body) = self.read_frame().await.map_err(|e| {
                self.broken = true;
                if e.kind() == std::io::ErrorKind::UnexpectedEof { closed() } else { io_error(&e) }
            })?;
            let complex_types = self.complex_types_on && self.protocol_version >= protocol::COMPLEX_TYPES_VERSION;
            let message = protocol::decode(tag, &body, complex_types).map_err(|e| {
                self.broken = true;
                protocol_error(e)
            })?;
            match message {
                Backend::Notice(fields) => self.notices.push(notice(&fields)),
                Backend::ParameterStatus(name, value) => match name.as_str() {
                    "protocol_version" => self.protocol_version = value.parse().unwrap_or(0),
                    "request_complex_types" => self.complex_types_on = value == "on",
                    _ => {}
                },
                other => return Ok(other),
            }
        }
    }

    /// Reads up to `ReadyForQuery`, discarding rows; returns the first error.
    pub async fn drain(&mut self) -> DbResult<()> {
        let mut first_error = None;
        loop {
            match self.read().await? {
                Backend::ReadyForQuery(_) => break,
                Backend::Error(fields) => {
                    first_error.get_or_insert_with(|| server_error(&fields));
                }
                Backend::CopyIn => self.send(&protocol::copy_fail("DataQuery does not send COPY data.")).await?,
                Backend::Other(tag) => return Err(self.unsupported_message(tag)),
                _ => {}
            }
        }
        self.open = false;
        first_error.map_or(Ok(()), Err)
    }

    /// `COPY … LOCAL` and load balancing need driver features DataQuery does not have.
    fn unsupported_message(&mut self, tag: u8) -> DbError {
        self.broken = true;
        let what = match tag {
            b'F' | b'H' | b'O' => "COPY … FROM LOCAL is not supported; load files with Import instead.",
            _ => "The server sent a message DataQuery does not understand.",
        };
        DbError::unsupported(what).with_action(Some("Reconnect to run statements again."))
    }

    /// Starts a statement. Without parameters it runs as a simple query (any
    /// statement, `SET`, transactions); with them it is prepared, described
    /// and bound with the types the server inferred.
    pub async fn start(&mut self, sql: &str, params: &[Option<String>]) -> DbResult<Start> {
        if self.open {
            self.drain().await?;
        }
        if params.is_empty() {
            self.send(&protocol::query(sql)).await?;
            return self.first_result(None).await;
        }
        let mut prepare = protocol::parse(sql);
        prepare.extend(protocol::describe());
        prepare.extend(protocol::flush());
        self.send(&prepare).await?;
        let described = self.describe().await;
        let (types, columns) = match described {
            Ok(found) => found,
            Err(e) => {
                self.send(&protocol::sync()).await?;
                let _ = self.drain().await;
                return Err(e);
            }
        };
        if types.len() != params.len() {
            self.send(&protocol::sync()).await?;
            self.drain().await?;
            return Err(DbError::invalid(format!(
                "The statement has {} parameter(s) but {} value(s) were given.",
                types.len(),
                params.len()
            ))
            .with_action(Some("Fill in a value for every ? placeholder.")));
        }
        let oids: Vec<u32> = types.iter().map(|t| t.type_oid).collect();
        let values: Vec<Option<Vec<u8>>> =
            params.iter().zip(&oids).map(|(v, oid)| v.as_deref().map(|v| protocol::bind_value(v, *oid))).collect();
        let mut run = protocol::bind(&values, &oids);
        run.extend(protocol::execute());
        run.extend(protocol::sync());
        self.send(&run).await?;
        self.first_result(columns).await
    }

    /// `ParseComplete`, `ParameterDescription`, `RowDescription` or `NoData`, `CommandDescription`.
    async fn describe(&mut self) -> DbResult<(Vec<protocol::Param>, Option<Vec<Column>>)> {
        let (mut types, mut columns) = (Vec::new(), None);
        loop {
            match self.read().await? {
                Backend::ParseComplete | Backend::NoData => {}
                Backend::ParameterDescription(found) => types = found,
                Backend::RowDescription(found) => columns = Some(found),
                Backend::CommandDescription(_) => return Ok((types, columns)),
                Backend::Error(fields) => return Err(server_error(&fields)),
                other => return Err(protocol_error(format!("{other:?} while preparing"))),
            }
        }
    }

    /// Reads until the first result: rows (left open) or the end of the statement.
    async fn first_result(&mut self, described: Option<Vec<Column>>) -> DbResult<Start> {
        loop {
            match self.read().await? {
                Backend::BindComplete => {
                    if let Some(columns) = described.clone() {
                        self.open = true;
                        return Ok(Start::Rows(columns));
                    }
                }
                Backend::RowDescription(columns) => {
                    self.open = true;
                    return Ok(Start::Rows(columns));
                }
                Backend::CommandComplete(_) | Backend::EmptyQuery => {
                    self.drain().await?;
                    return Ok(Start::Done);
                }
                Backend::ReadyForQuery(_) => return Ok(Start::Done),
                Backend::Error(fields) => {
                    let error = server_error(&fields);
                    let _ = self.drain().await;
                    return Err(error);
                }
                Backend::CopyIn => self.send(&protocol::copy_fail("DataQuery does not send COPY data.")).await?,
                Backend::Other(tag) => return Err(self.unsupported_message(tag)),
                _ => {}
            }
        }
    }

    /// The next row of the open result; `None` once the statement finished
    /// (later results of a multi-statement query are discarded).
    pub async fn next_row(&mut self) -> DbResult<Option<RawRow>> {
        if !self.open {
            return Ok(None);
        }
        loop {
            match self.read().await? {
                Backend::DataRow(row) => return Ok(Some(row)),
                Backend::CommandComplete(_) => {
                    self.drain().await?;
                    return Ok(None);
                }
                Backend::ReadyForQuery(_) => {
                    self.open = false;
                    return Ok(None);
                }
                Backend::Error(fields) => {
                    let error = server_error(&fields);
                    let _ = self.drain().await;
                    return Err(error);
                }
                Backend::Other(tag) => return Err(self.unsupported_message(tag)),
                _ => {}
            }
        }
    }

    /// Runs a statement without rows (or ignoring them).
    pub async fn execute(&mut self, sql: &str) -> DbResult<()> {
        match self.start(sql, &[]).await? {
            Start::Rows(_) => self.drain().await,
            Start::Done => Ok(()),
        }
    }

    /// Runs a statement and returns every row as text (catalog reads, which bound their rows).
    pub async fn text_rows(&mut self, sql: &str, params: &[&str]) -> DbResult<Vec<TextRow>> {
        let params: Vec<Option<String>> = params.iter().map(|p| Some((*p).to_owned())).collect();
        let mut rows = Vec::new();
        if let Start::Rows(_) = self.start(sql, &params).await? {
            while let Some(row) = self.next_row().await? {
                rows.push(TextRow(
                    row.into_iter().map(|v| v.map(|v| String::from_utf8_lossy(&v).into_owned())).collect(),
                ));
            }
        }
        Ok(rows)
    }

    /// Ends the session politely; the connection is unusable afterwards.
    pub async fn terminate(&mut self) {
        if !self.broken {
            let _ = self.send(&protocol::terminate()).await;
            let _ = self.io.shutdown().await;
            self.broken = true;
        }
    }
}

fn notice(fields: &Fields) -> String {
    format!("{}: {}", fields.severity(), fields.message())
}
