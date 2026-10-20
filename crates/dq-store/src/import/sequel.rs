//! Sequel Ace and Sequel Pro: `Favorites.plist`, a tree under `Favorites Root`
//! where groups have `Children`. Both are MySQL clients; passwords are in the
//! keychain.

use std::path::Path;

use dq_core::config::{Environment, SslMode};
use dq_core::engine::Engine;
use dq_core::{DbError, DbResult};
use plist::Value;

use super::{Dict, Found, Raw, UserDirs, file_in, parse_tree, read_file, ssh_hop};

/// Groups nested deeper than this are not read.
const MAX_DEPTH: usize = 32;

pub(super) fn read(path: &Path, dirs: &UserDirs, found: &mut Found) -> DbResult<()> {
    let file = file_in(path, &["Favorites.plist"]);
    let root = parse_tree(&read_file(&file)?).ok_or_else(|| DbError::invalid("This is not a Favorites.plist file."))?;
    let favorites = Dict::of(&root)
        .and_then(|d| d.dict("Favorites Root"))
        .ok_or_else(|| DbError::invalid("This is not a Sequel Ace or Sequel Pro Favorites.plist file."))?;
    read_children(favorites.array("Children"), &mut Vec::new(), dirs, found);
    Ok(())
}

fn read_children(children: &[Value], groups: &mut Vec<String>, dirs: &UserDirs, found: &mut Found) {
    for d in children.iter().filter_map(Dict::of) {
        if d.get("Children").is_some() {
            if groups.len() < MAX_DEPTH {
                groups.push(d.str("Name").to_owned());
                read_children(d.array("Children"), groups, dirs, found);
                groups.pop();
            }
        } else {
            read_favorite(d, groups, dirs, found);
        }
    }
}

/// Sequel Ace's colour labels: red marks production, orange staging and green development.
fn environment(color: Option<i64>) -> Environment {
    match color {
        Some(0) => Environment::Production,
        Some(1) => Environment::Staging,
        Some(3) => Environment::Development,
        _ => Environment::Local,
    }
}

fn read_favorite(d: Dict<'_>, groups: &[String], dirs: &UserDirs, found: &mut Found) {
    let name = d.str("name");
    // `type`: 0 TCP/IP, 1 socket, 2 SSH tunnel.
    let kind = d.int("type").unwrap_or(0);
    if kind == 1 {
        return found.skip(name, "connects through a local socket, which DataQuery does not support.");
    }
    let enabled_path = |key: &str| {
        let on = d.get(&format!("{key}Enabled")).is_none_or(|_| d.flag(&format!("{key}Enabled")));
        Some(d.str(key)).filter(|p| on && !p.is_empty()).map(|p| dirs.expand(p, None))
    };
    let mut raw = Raw {
        name: name.to_owned(),
        driver: "MySQL".into(),
        engine: Some(Engine::Mysql),
        host: d.str("host").to_owned(),
        port: d.port("port"),
        database: d.str("database").to_owned(),
        username: d.str("user").to_owned(),
        ssl: d.flag("useSSL").then_some(SslMode::Require),
        ssh: (kind == 2)
            .then(|| {
                let key = enabled_path("sshKeyLocation").map(|p| dirs.ssh_key(&p, None));
                ssh_hop(d.str("sshHost"), d.port("sshPort"), d.str("sshUser"), key, false)
            })
            .flatten(),
        folder: Some(groups.iter().filter(|g| !g.is_empty()).cloned().collect::<Vec<_>>().join(" / ")),
        environment: environment(d.int("colorIndex")),
        ..Raw::default()
    };
    if raw.ssl.is_some() {
        raw.tls.ca_file = enabled_path("sslCACertFileLocation");
        raw.tls.client_cert_file = enabled_path("sslCertificateFileLocation");
        raw.tls.client_key_file = enabled_path("sslKeyFileLocation");
    }
    found.push(raw);
}

#[cfg(test)]
mod tests {
    use dq_core::config::SshAuth;

    use super::super::tests::{dirs, fixture};
    use super::super::{SourceApp, read as read_app};
    use super::*;

    #[test]
    fn reads_favorites_groups_ssh_and_tls() {
        let path = fixture(
            "home/Library/Containers/com.sequel-ace.sequel-ace/Data/Library/Application Support/Sequel Ace/Data",
        );
        let preview = read_app(SourceApp::SequelAce, &path, &dirs()).unwrap();
        let names: Vec<&str> = preview.connections.iter().map(|c| c.connection.name.as_str()).collect();
        assert_eq!(names, ["Local dev", "Orders primary", "Orders replica"]);
        assert_eq!(
            preview.skipped,
            ["Homebrew socket: connects through a local socket, which DataQuery does not support."]
        );

        let local = &preview.connections[0].connection;
        assert_eq!((local.folder.as_deref(), local.environment), (None, Environment::Development));
        assert_eq!((local.config.port, local.config.ssl_mode), (3306, SslMode::Prefer));

        let primary = &preview.connections[1].connection;
        assert_eq!((primary.folder.as_deref(), primary.environment), (Some("Clients / Acme"), Environment::Production));
        assert_eq!((primary.config.host.as_str(), primary.config.port), ("orders-db.acme.internal", 3307));
        assert_eq!(primary.config.ssl_mode, SslMode::Require);
        assert_eq!(primary.config.tls_files.ca_file.as_deref(), Some("/Users/ada/certs/acme-ca.pem"));
        assert_eq!(primary.config.tls_files.client_cert_file, None);
        let hop = &primary.config.ssh.as_ref().unwrap().host;
        assert_eq!((hop.host.as_str(), hop.port, hop.username.as_str()), ("jump.acme.com", 22, "ops"));
        assert_eq!(hop.auth, SshAuth::PrivateKey { path: "/Users/ada/.ssh/acme_rsa".into() });

        let replica = &preview.connections[2].connection;
        assert_eq!(replica.config.ssh.as_ref().unwrap().host.auth, SshAuth::Password);
        assert!(preview.connections.iter().all(|c| c.problem.is_none()));
    }
}
