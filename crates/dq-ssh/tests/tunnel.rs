//! Tunnel tests against the SSH bastion in `docker-compose.yml`.
//!
//! Run with `DATAQUERY_TEST_SSH=localhost:52222 cargo test -p dq-ssh`. The
//! bastion reaches PostgreSQL as `postgres:5432` inside the compose network.

use std::sync::Arc;
use std::time::Duration;

use dq_core::ErrorKind;
use dq_core::adapter::{ConnectParams, DatabaseAdapter};
use dq_core::config::{ConnConfig, SshAuth, SshHop, SshTunnel, SslMode};
use dq_core::result::ExecOptions;
use dq_ssh::{KnownHosts, UNKNOWN_HOST_KEY};

const TIMEOUT: Duration = Duration::from_secs(10);

fn bastion() -> Option<(String, u16)> {
    let target = std::env::var("DATAQUERY_TEST_SSH").ok()?;
    let (host, port) = target.split_once(':').unwrap_or((&target, "22"));
    Some((host.to_owned(), port.parse().unwrap()))
}

macro_rules! require_ssh {
    () => {
        match bastion() {
            Some(b) => b,
            None => {
                eprintln!("skipped: set DATAQUERY_TEST_SSH to run SSH tunnel tests");
                return;
            }
        }
    };
}

fn hop(host: &str, port: u16) -> SshHop {
    SshHop { host: host.into(), port, username: "dataquery".into(), auth: SshAuth::Password }
}

/// Connects once to learn the key, then trusts it by the fingerprint shown.
async fn trust(known: &Arc<KnownHosts>, tunnel: &SshTunnel, secrets: &Vec<Option<String>>) {
    for _ in 0..=tunnel.jumps.len() {
        match dq_ssh::open(tunnel, secrets, ("postgres", 5432), known, TIMEOUT).await {
            Ok(_) => return,
            Err(e) => {
                assert_eq!(e.code.as_deref(), Some(UNKNOWN_HOST_KEY), "{e:?}");
                let detail = e.detail.unwrap();
                let (host_port, rest) = detail.trim_start_matches("Host ").split_once(" · ").unwrap();
                let (host, port) = host_port.rsplit_once(':').unwrap();
                let fingerprint = rest.rsplit(" · ").next().unwrap();
                known.trust(host, port.parse().unwrap(), fingerprint).unwrap();
            }
        }
    }
}

#[tokio::test]
async fn tunnels_to_postgres_after_trusting_the_host_key() {
    let (host, port) = require_ssh!();
    let dir = tempfile::tempdir().unwrap();
    let known = Arc::new(KnownHosts::app_only(dir.path().join("known_hosts")));
    let tunnel = SshTunnel { host: hop(&host, port), jumps: vec![] };
    let secrets = vec![Some("dataquery".to_owned())];

    let err = dq_ssh::open(&tunnel, &secrets, ("postgres", 5432), &known, TIMEOUT).await.unwrap_err();
    assert_eq!(err.code.as_deref(), Some(UNKNOWN_HOST_KEY));
    assert!(err.detail.as_deref().unwrap().contains("SHA256:"));
    trust(&known, &tunnel, &secrets).await;

    let opened = dq_ssh::open(&tunnel, &secrets, ("postgres", 5432), &known, TIMEOUT).await.unwrap();
    let config = ConnConfig {
        host: "postgres".into(),
        port: 5432,
        database: "dataquery_test".into(),
        username: "dataquery".into(),
        ssl_mode: SslMode::Disable,
        ssh: Some(tunnel),
        ..ConnConfig::default()
    };
    let params = ConnectParams { config, password: Some("dataquery".into()), endpoint: Some(opened.local_addr()) };
    let session = dq_postgres::PostgresAdapter::POSTGRES.connect(&params).await.unwrap();
    let options = ExecOptions { row_limit: 10, timeout_secs: 10 };
    let r = session.execute("SELECT current_database()", &[], options).await.unwrap();
    assert_eq!(r.rows[0][0].to_text().as_deref(), Some("dataquery_test"));
    // Catalog connections go through the same tunnel.
    assert!(!session.list_children(None).await.unwrap().is_empty());
    session.close().await;
    assert!(opened.is_open());
}

#[tokio::test]
async fn backup_tools_connect_through_the_tunnel() {
    use dq_core::backup::{self, BackupContent, BackupOptions, Tool, ToolOverrides};
    let (host, port) = require_ssh!();
    if backup::tools::locate(Tool::PgDump, dq_core::engine::Engine::Postgres, None, None).await.found.is_none() {
        eprintln!("skipped: pg_dump is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let known = Arc::new(KnownHosts::app_only(dir.path().join("known_hosts")));
    let tunnel = SshTunnel { host: hop(&host, port), jumps: vec![] };
    let secrets = vec![Some("dataquery".to_owned())];
    trust(&known, &tunnel, &secrets).await;
    let opened = dq_ssh::open(&tunnel, &secrets, ("postgres", 5432), &known, TIMEOUT).await.unwrap();
    // `postgres` only resolves inside the compose network: pg_dump must use the tunnel's address.
    let config = ConnConfig {
        host: "postgres".into(),
        port: 5432,
        database: "dataquery_test".into(),
        username: "dataquery".into(),
        ssl_mode: SslMode::Disable,
        ssh: Some(tunnel),
        ..ConnConfig::default()
    };
    let params = ConnectParams { config, password: Some("dataquery".into()), endpoint: Some(opened.local_addr()) };
    let options = BackupOptions {
        database: "dataquery_test".into(),
        content: BackupContent::SchemaOnly,
        ..BackupOptions::default()
    };
    let path = dir.path().join("schema.dump");
    let report = backup::backup(
        &params,
        "",
        None,
        &options,
        &path,
        &ToolOverrides::new(),
        &dq_core::transfer::TransferControl::none(),
    )
    .await
    .unwrap();
    assert!(report.bytes > 0 && path.is_file());
}

#[tokio::test]
async fn jump_hosts_are_chained() {
    let (host, port) = require_ssh!();
    let dir = tempfile::tempdir().unwrap();
    let known = Arc::new(KnownHosts::app_only(dir.path().join("known_hosts")));
    // Jump to the bastion, then from the bastion to itself by its compose name.
    let tunnel = SshTunnel { host: hop("ssh", 22), jumps: vec![hop(&host, port)] };
    let secrets = vec![Some("dataquery".to_owned()), Some("dataquery".to_owned())];
    trust(&known, &tunnel, &secrets).await;
    let opened = dq_ssh::open(&tunnel, &secrets, ("postgres", 5432), &known, TIMEOUT).await.unwrap();
    let stream = tokio::net::TcpStream::connect(opened.local_addr()).await;
    assert!(stream.is_ok());
}

#[tokio::test]
async fn wrong_password_is_an_auth_error() {
    let (host, port) = require_ssh!();
    let dir = tempfile::tempdir().unwrap();
    let known = Arc::new(KnownHosts::app_only(dir.path().join("known_hosts")));
    let tunnel = SshTunnel { host: hop(&host, port), jumps: vec![] };
    trust(&known, &tunnel, &vec![Some("dataquery".to_owned())]).await;
    let err =
        dq_ssh::open(&tunnel, &vec![Some("nope".to_owned())], ("postgres", 5432), &known, TIMEOUT).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth, "{err:?}");
    assert!(!format!("{err:?}").contains("nope\""));
    let missing = dq_ssh::open(&tunnel, &vec![None], ("postgres", 5432), &known, TIMEOUT).await.unwrap_err();
    assert_eq!(missing.kind, ErrorKind::Auth);
}

/// Cloud `.pem` keys (AWS EC2 RSA, SEC1 EC, an encrypted key) authenticate the tunnel.
#[tokio::test]
async fn pem_private_keys_authenticate() {
    let (host, port) = require_ssh!();
    let dir = tempfile::tempdir().unwrap();
    let known = Arc::new(KnownHosts::app_only(dir.path().join("known_hosts")));
    let key = |name: &str| format!("{}/src/testdata/{name}", env!("CARGO_MANIFEST_DIR"));
    let password = SshTunnel { host: hop(&host, port), jumps: vec![] };
    trust(&known, &password, &vec![Some("dataquery".to_owned())]).await;
    for (file, secret) in [("rsa_pkcs1.pem", None), ("ec_sec1.pem", None), ("rsa_aes128.pem", Some("secret"))] {
        let auth = SshAuth::PrivateKey { path: key(file) };
        let tunnel = SshTunnel { host: SshHop { auth, ..hop(&host, port) }, jumps: vec![] };
        let secrets = vec![secret.map(str::to_owned)];
        let opened = dq_ssh::open(&tunnel, &secrets, ("postgres", 5432), &known, TIMEOUT).await;
        assert!(opened.is_ok(), "{file}: {:?}", opened.err());
    }
    // An encrypted key without its passphrase asks for it.
    let auth = SshAuth::PrivateKey { path: key("rsa_aes128.pem") };
    let tunnel = SshTunnel { host: SshHop { auth, ..hop(&host, port) }, jumps: vec![] };
    let err = dq_ssh::open(&tunnel, &vec![None], ("postgres", 5432), &known, TIMEOUT).await.unwrap_err();
    assert!(err.message.contains("passphrase"), "{err:?}");
}
