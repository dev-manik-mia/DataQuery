//! `TablePlus`: `Connections.plist`, an array of connection dictionaries, with
//! `ConnectionGroups.plist` beside it naming the groups. Passwords are in the
//! keychain. Password-protected `.tableplusconnection` exports are encrypted
//! and are refused with an explanation.

use std::collections::HashMap;
use std::path::Path;

use dq_core::config::SslMode;
use dq_core::engine::Engine;
use dq_core::{DbError, DbResult};
use plist::Value;

use super::{Dict, Found, Raw, UserDirs, engine_named, environment_named, file_in, parse_tree, read_file, ssh_hop};

pub(super) fn read(path: &Path, dirs: &UserDirs, found: &mut Found) -> DbResult<()> {
    let file = file_in(path, &["Connections.plist"]);
    let root = parse_tree(&read_file(&file)?).ok_or_else(|| {
        DbError::invalid(
            "This TablePlus file is encrypted or not a connection list. Password-protected exports cannot be read: \
             choose TablePlus's Connections.plist instead.",
        )
    })?;
    let (connections, mut groups) = split(&root);
    if connections.is_empty() && !has_connection_key(&root) {
        return Err(DbError::invalid("This file has no TablePlus connections."));
    }
    if groups.is_empty() {
        let sibling = file.with_file_name("ConnectionGroups.plist");
        if let Some(tree) = sibling.is_file().then(|| read_file(&sibling).ok()).flatten().and_then(|b| parse_tree(&b)) {
            groups = group_names(tree.as_array().map_or(&[], Vec::as_slice));
        }
    }
    for d in connections.iter().filter_map(Dict::of) {
        read_connection(d, &groups, dirs, found);
    }
    Ok(())
}

fn has_connection_key(root: &Value) -> bool {
    root.as_dictionary().is_some_and(|d| d.keys().any(|k| k.eq_ignore_ascii_case("connections")))
}

/// The connection and group arrays: the root array, or the arrays of an export dictionary.
fn split(root: &Value) -> (&[Value], HashMap<String, String>) {
    let is = |items: &[Value], key: &str| items.iter().filter_map(Dict::of).any(|d| d.get(key).is_some());
    match root {
        Value::Array(items) => (items, HashMap::new()),
        Value::Dictionary(d) => {
            let arrays: Vec<&[Value]> = d.values().filter_map(Value::as_array).map(Vec::as_slice).collect();
            let connections = arrays.iter().find(|a| is(a, "ConnectionName")).copied().unwrap_or(&[]);
            let groups = arrays.iter().find(|a| !is(a, "ConnectionName") && is(a, "Name")).copied().unwrap_or(&[]);
            (connections, group_names(groups))
        }
        _ => (&[], HashMap::new()),
    }
}

fn group_names(items: &[Value]) -> HashMap<String, String> {
    items
        .iter()
        .filter_map(Dict::of)
        .filter(|g| !g.str("ID").is_empty() && !g.str("Name").is_empty())
        .map(|g| (g.str("ID").to_owned(), g.str("Name").to_owned()))
        .collect()
}

/// `TablePlus` `tLSMode`: 0 prefer, 1 require, 2 verify-ca, 3 verify-full.
fn tls_mode(mode: Option<i64>) -> Option<SslMode> {
    Some(match mode? {
        0 => SslMode::Prefer,
        1 => SslMode::Require,
        2 => SslMode::VerifyCa,
        3 => SslMode::VerifyFull,
        _ => return None,
    })
}

fn read_connection(d: Dict<'_>, groups: &HashMap<String, String>, dirs: &UserDirs, found: &mut Found) {
    let name = d.str("ConnectionName");
    if d.flag("isUseSocket") {
        return found.skip(name, "connects through a local socket, which DataQuery does not support.");
    }
    let driver = d.str("Driver");
    let engine = engine_named(driver);
    let file_based = engine.is_some_and(Engine::is_file_based);
    let tls_paths: Vec<String> =
        d.array("TlsKeyPaths").iter().map(|v| v.as_string().unwrap_or_default().trim().to_owned()).collect();
    let tls_path = |i: usize| tls_paths.get(i).filter(|p| !p.is_empty()).map(|p| dirs.expand(p, None));
    let mut raw = Raw {
        name: name.to_owned(),
        driver: driver.to_owned(),
        engine,
        host: d.str("DatabaseHost").to_owned(),
        port: d.port("DatabasePort"),
        database: if file_based { dirs.expand(d.str("DatabasePath"), None) } else { d.str("DatabaseName").to_owned() },
        username: d.str("DatabaseUser").to_owned(),
        ssl: tls_mode(d.int("tLSMode")),
        ssh: d
            .flag("isOverSSH")
            .then(|| {
                let key = d.flag("isUsePrivateKey").then(|| dirs.ssh_key(d.str("ServerPrivateKeyName"), None));
                ssh_hop(d.str("ServerAddress"), d.port("ServerPort"), d.str("ServerUser"), key, false)
            })
            .flatten(),
        folder: groups.get(d.str("GroupID")).cloned(),
        // TablePlus spells the key `Enviroment`.
        environment: environment_named(d.any_str(&["Enviroment", "Environment"])).unwrap_or_default(),
        ..Raw::default()
    };
    raw.tls.ca_file = tls_path(0);
    raw.tls.client_cert_file = tls_path(1);
    raw.tls.client_key_file = tls_path(2);
    found.push(raw);
}

#[cfg(test)]
mod tests {
    use dq_core::config::{Environment, SshAuth};
    use dq_core::engine::Engine;

    use super::super::tests::{dirs, fixture};
    use super::super::{SourceApp, read as read_app};
    use super::*;

    #[test]
    fn reads_connections_groups_ssh_and_environments() {
        for file in
            ["home/Library/Application Support/com.tinyapp.TablePlus/Data", "files/tableplus-binary/Connections.plist"]
        {
            let preview = read_app(SourceApp::Tableplus, &fixture(file), &dirs()).unwrap();
            let names: Vec<&str> = preview.connections.iter().map(|c| c.connection.name.as_str()).collect();
            assert_eq!(names, ["Billing prod", "Shop (local)", "Cache", "Analytics file"], "{file}");
            let prod = &preview.connections[0];
            assert_eq!(prod.problem, None);
            assert_eq!(
                (prod.connection.environment, prod.connection.folder.as_deref()),
                (Environment::Production, Some("Billing"))
            );
            let c = &prod.connection.config;
            assert_eq!(
                (c.engine, c.host.as_str(), c.port, c.username.as_str()),
                (Engine::Postgres, "10.0.4.12", 5432, "billing")
            );
            assert_eq!(c.ssl_mode, SslMode::VerifyFull);
            assert_eq!(c.tls_files.ca_file.as_deref(), Some("/Users/ada/certs/billing-ca.pem"));
            let hop = &c.ssh.as_ref().unwrap().host;
            assert_eq!((hop.host.as_str(), hop.port, hop.username.as_str()), ("bastion.example.com", 2222, "deploy"));
            assert_eq!(hop.auth, SshAuth::PrivateKey { path: "/Users/ada/.ssh/id_ed25519".into() });
            let shop = &preview.connections[1].connection;
            assert_eq!(
                (shop.config.engine, shop.config.ssl_mode, shop.environment),
                (Engine::Mariadb, SslMode::Prefer, Environment::Local)
            );
            assert_eq!(preview.connections[2].connection.config.engine, Engine::Redis);
            let file = &preview.connections[3].connection.config;
            assert_eq!(
                (file.engine, file.database.as_str(), file.port),
                (Engine::Sqlite, "/Users/ada/data/analytics.db", 0)
            );
            assert_eq!(
                preview.skipped,
                [
                    "Socket db: connects through a local socket, which DataQuery does not support.",
                    "Warehouse: Redshift is not supported."
                ]
            );
        }
    }

    #[test]
    fn refuses_encrypted_exports_clearly() {
        let err = read_app(SourceApp::Tableplus, &fixture("files/encrypted.tableplusconnection"), &dirs()).unwrap_err();
        assert!(err.message.contains("encrypted"), "{}", err.message);
    }

    #[test]
    fn reads_an_unencrypted_export_dictionary() {
        let preview = read_app(SourceApp::Tableplus, &fixture("files/export.tableplusconnection"), &dirs()).unwrap();
        assert_eq!(preview.connections.len(), 1);
        assert_eq!(preview.connections[0].connection.folder.as_deref(), Some("Team"));
    }
}
