//! `DataGrip` and the other `JetBrains` IDEs: `dataSources.xml` (shared) and
//! `dataSources.local.xml` (user name, SSH and TLS settings) in a project's
//! `.idea` folder or the IDE's `options` folder, merged by data source id.
//! SSH tunnels refer to `sshConfigs.xml` entries by id. Passwords are in the
//! IDE's credential store, which is not read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use dq_core::config::{SshHop, SslMode};
use dq_core::engine::Engine;
use dq_core::{DbError, DbResult};

use super::xml::{self, Element};
use super::{Found, Raw, UserDirs, engine_named, file_in, jdbc, locate, read_text, ssh_hop};

pub(super) fn read(path: &Path, dirs: &UserDirs, found: &mut Found) -> DbResult<()> {
    let file = file_in(path, &["dataSources.xml", ".idea/dataSources.xml"]);
    let shared = xml::parse(&read_text(&file)?)?;
    if !shared.descendants("component").iter().any(|c| c.attr("name") == Some("DataSourceManagerImpl")) {
        return Err(DbError::invalid("This is not a JetBrains dataSources.xml file."));
    }
    let local = optional_xml(&file.with_file_name("dataSources.local.xml"));
    let folder = file.parent().unwrap_or(Path::new(""));
    let project = (folder.file_name().is_some_and(|n| n == ".idea")).then(|| folder.parent()).flatten();

    // Project-level SSH configurations first, then each IDE's.
    let mut ssh_files = vec![folder.join("sshConfigs.xml")];
    ssh_files.extend(locate::jetbrains_configs(dirs).into_iter().map(|c| c.join("options/sshConfigs.xml")));
    let ssh = ssh_configs(&ssh_files, dirs, project);

    // One data source may be split between both files; they share its uuid.
    let mut order: Vec<&str> = Vec::new();
    let mut parts: HashMap<&str, Vec<&Element>> = HashMap::new();
    for doc in std::iter::once(&shared).chain(local.as_ref()) {
        for ds in doc.descendants("data-source") {
            let Some(id) = ds.attr("uuid").or_else(|| ds.attr("name")) else { continue };
            if !parts.contains_key(id) {
                order.push(id);
            }
            parts.entry(id).or_default().push(ds);
        }
    }
    for id in order {
        read_source(&parts[id], &ssh, dirs, project, found);
    }
    Ok(())
}

fn optional_xml(path: &Path) -> Option<Element> {
    path.is_file().then(|| read_text(path).ok().and_then(|t| xml::parse(&t).ok())).flatten()
}

/// `sshConfig` entries by id. The first file that defines an id wins.
fn ssh_configs(files: &[PathBuf], dirs: &UserDirs, project: Option<&Path>) -> HashMap<String, SshHop> {
    let mut configs = HashMap::new();
    for doc in files.iter().filter_map(|f| optional_xml(f)) {
        for c in doc.descendants("sshConfig") {
            let Some(id) = c.attr("id") else { continue };
            let auth = c.attr("authType").unwrap_or_default().to_ascii_uppercase();
            let key = c
                .attr("keyPath")
                .filter(|_| auth != "PASSWORD" && auth != "OPEN_SSH")
                .map(|p| dirs.ssh_key(p, project));
            let port = c.attr("port").and_then(|p| p.parse().ok());
            let hop = ssh_hop(
                c.attr("host").unwrap_or_default(),
                port,
                c.attr("username").unwrap_or_default(),
                key,
                auth == "OPEN_SSH",
            );
            if let Some(hop) = hop {
                configs.entry(id.to_owned()).or_insert(hop);
            }
        }
    }
    configs
}

fn read_source(
    parts: &[&Element],
    ssh: &HashMap<String, SshHop>,
    dirs: &UserDirs,
    project: Option<&Path>,
    found: &mut Found,
) {
    let first = |f: &dyn Fn(&Element) -> Option<&str>| parts.iter().find_map(|e| f(e)).unwrap_or_default().to_owned();
    let section =
        |name: &str| parts.iter().find_map(|e| e.child(name)).filter(|s| s.child_text("enabled") == Some("true"));
    let driver = first(&|e| e.child_text("driver-ref"));
    let url = first(&|e| e.child_text("jdbc-url"));
    let target = jdbc::parse(&url);
    let mut raw = Raw {
        name: first(&|e| e.attr("name")),
        driver: if driver.is_empty() {
            target.as_ref().map(|t| t.subprotocol.clone()).unwrap_or_default()
        } else {
            driver.clone()
        },
        engine: engine_named(&driver),
        username: first(&|e| e.child_text("user-name")),
        folder: Some(
            first(&|e| e.attr("group-name"))
                .split('/')
                .filter(|s| !s.trim().is_empty())
                .collect::<Vec<_>>()
                .join(" / "),
        ),
        ssh: section("ssh-properties").and_then(|s| s.child_text("ssh-config-id")).and_then(|id| ssh.get(id).cloned()),
        ..Raw::default()
    };
    if let Some(tls) = section("ssl-config") {
        let path = |name: &str| tls.child_text(name).map(|p| dirs.expand(p, project));
        raw.ssl = Some(tls.child_text("mode").and_then(SslMode::parse).unwrap_or(SslMode::Require));
        raw.tls.ca_file = path("ca-cert");
        raw.tls.client_cert_file = path("client-cert");
        raw.tls.client_key_file = path("client-key");
    }
    if let Some(target) = target {
        target.apply(&mut raw);
    }
    if raw.engine.is_some_and(Engine::is_file_based) {
        raw.database = dirs.expand(&raw.database, project);
    }
    found.push(raw);
}

#[cfg(test)]
mod tests {
    use dq_core::config::SshAuth;
    use dq_core::engine::Engine;

    use super::super::tests::{fixture, fixture_dirs};
    use super::super::{ForeignConnection, ForeignPreview, SourceApp, read as read_app};
    use super::*;

    fn by_name<'a>(p: &'a ForeignPreview, name: &str) -> &'a ForeignConnection {
        p.connections.iter().find(|c| c.connection.name == name).unwrap_or_else(|| panic!("{name} missing"))
    }

    #[test]
    fn merges_shared_and_local_files_with_ssh_configs() {
        let dirs = fixture_dirs();
        let home = dirs.home.to_string_lossy().into_owned();
        // The project folder works as well as the `.idea` folder or the file.
        let preview = read_app(SourceApp::Datagrip, &fixture("home/code/shop"), &dirs).unwrap();
        assert_eq!(preview.connections.len(), 5);
        assert_eq!(preview.skipped, ["reports: bigquery is not supported."]);

        let orders = by_name(&preview, "orders@prod");
        assert_eq!(orders.problem, None);
        let c = &orders.connection.config;
        assert_eq!(
            (c.engine, c.host.as_str(), c.database.as_str(), c.username.as_str()),
            (Engine::Postgres, "orders.prod.internal", "orders", "orders_ro")
        );
        assert_eq!(orders.connection.folder.as_deref(), Some("Production"));
        assert_eq!(
            (c.ssl_mode, c.tls_files.ca_file.clone()),
            (SslMode::VerifyFull, Some(format!("{home}/certs/prod-ca.pem")))
        );
        let hop = &c.ssh.as_ref().unwrap().host;
        assert_eq!((hop.host.as_str(), hop.username.as_str()), ("bastion.prod.example.com", "ec2-user"));
        assert_eq!(hop.auth, SshAuth::PrivateKey { path: format!("{home}/.ssh/prod_ed25519") });

        let crm = &by_name(&preview, "crm").connection.config;
        assert_eq!((crm.engine, crm.database.as_str(), crm.ssl_mode), (Engine::Sqlserver, "crm", SslMode::Disable));
        let hop = &crm.ssh.as_ref().unwrap().host;
        assert_eq!((hop.port, hop.auth.clone()), (2200, SshAuth::Agent));

        let events = by_name(&preview, "events");
        assert_eq!(
            (events.connection.config.engine, events.connection.folder.as_deref()),
            (Engine::Clickhouse, Some("Analytics / ClickHouse"))
        );
        let dev = &by_name(&preview, "dev.db").connection.config;
        assert_eq!(
            (dev.engine, dev.database.clone()),
            (Engine::Sqlite, fixture("home/code/shop/data/dev.db").to_string_lossy().into_owned())
        );
        let sessions = &by_name(&preview, "sessions").connection.config;
        assert_eq!((sessions.engine, sessions.database.as_str(), sessions.port), (Engine::Mongodb, "sessions", 27017));
    }

    #[test]
    fn rejects_other_xml() {
        let err = read_app(
            SourceApp::Datagrip,
            &fixture("home/Library/Application Support/JetBrains/DataGrip2024.3/options/sshConfigs.xml"),
            &fixture_dirs(),
        );
        assert!(err.is_err());
    }
}
