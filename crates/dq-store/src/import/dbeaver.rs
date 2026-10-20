//! `DBeaver`: `data-sources.json` in a project's `.dbeaver` folder. Connections
//! are keyed by id, folders by path (`Prod/EU`), and network handlers carry the
//! SSH tunnel and TLS settings. `credentials-config.json`, which holds the
//! encrypted users and passwords, is never opened.

use std::path::Path;

use dq_core::config::SslMode;
use dq_core::engine::Engine;
use dq_core::{DbError, DbResult};

use super::{
    Dict, Found, Raw, UserDirs, engine_named, environment_named, file_in, jdbc, parse_tree, read_file, ssh_hop,
};

pub(super) fn read(path: &Path, dirs: &UserDirs, found: &mut Found) -> DbResult<()> {
    let file = file_in(path, &["data-sources.json", ".dbeaver/data-sources.json"]);
    let root = parse_tree(&read_file(&file)?)
        .ok_or_else(|| DbError::invalid("This is not a DBeaver data-sources.json file."))?;
    let connections = Dict::of(&root)
        .and_then(|d| d.dict("connections"))
        .ok_or_else(|| DbError::invalid("This is not a DBeaver data-sources.json file."))?;
    for (id, value) in connections.entries() {
        if let Some(d) = Dict::of(value) {
            read_connection(id, d, dirs, found);
        }
    }
    Ok(())
}

fn read_connection(id: &str, d: Dict<'_>, dirs: &UserDirs, found: &mut Found) {
    let name = Some(d.str("name")).filter(|n| !n.is_empty()).unwrap_or(id);
    let (provider, driver) = (d.str("provider"), d.str("driver"));
    let cfg = d.dict("configuration");
    let get = |key: &str| cfg.map_or("", |c| c.str(key));
    let engine = engine_named(driver).or_else(|| engine_named(provider));
    let file_based = engine.is_some_and(Engine::is_file_based);
    let mut raw = Raw {
        name: name.to_owned(),
        driver: if provider.is_empty() { driver.to_owned() } else { provider.to_owned() },
        engine,
        host: get("host").to_owned(),
        port: cfg.and_then(|c| c.port("port")),
        database: if file_based { dirs.expand(get("database"), None) } else { get("database").to_owned() },
        // Newer DBeaver keeps the user with the password in the encrypted credentials file.
        username: get("user").to_owned(),
        folder: Some(d.str("folder").split('/').filter(|s| !s.trim().is_empty()).collect::<Vec<_>>().join(" / ")),
        environment: environment_named(get("type")).unwrap_or_default(),
        read_only: d.flag("read-only") || cfg.is_some_and(|c| c.flag("read-only")),
        ..Raw::default()
    };
    if let Some(c) = cfg {
        handlers(c, dirs, &mut raw);
        // Driver properties such as `sslmode` or `useSSL`.
        if let Some(props) = c.dict("properties") {
            let mut target = jdbc::Target::default();
            jdbc::params(props.entries().map(|(k, _)| (k, props.str(k))), &mut target);
            raw.ssl = raw.ssl.or(target.ssl);
        }
        // URL-configured connections (and missing fields) come from the JDBC URL.
        if let Some(target) = Some(get("url")).filter(|u| !u.is_empty()).and_then(jdbc::parse) {
            target.apply(&mut raw);
        }
    }
    found.push(raw);
}

/// The SSH tunnel and TLS handlers (`ssh_tunnel`, `postgre_ssl`, `mysql_ssl`, `ssl`…).
fn handlers(cfg: Dict<'_>, dirs: &UserDirs, raw: &mut Raw) {
    let Some(handlers) = cfg.dict("handlers") else { return };
    for (id, value) in handlers.entries() {
        let Some(h) = Dict::of(value).filter(|h| h.flag("enabled")) else { continue };
        let props = h.dict("properties");
        let prop = |keys: &[&str]| props.map_or("", |p| p.any_str(keys));
        let flag = |key: &str| props.is_some_and(|p| p.flag(key));
        if id == "ssh_tunnel" {
            let auth = prop(&["authType"]).to_ascii_uppercase();
            let key = (auth == "PUBLIC_KEY").then(|| dirs.ssh_key(prop(&["keyPath"]), None));
            let user = Some(h.str("user")).filter(|u| !u.is_empty()).unwrap_or_else(|| prop(&["user", "username"]));
            raw.ssh = ssh_hop(prop(&["host"]), props.and_then(|p| p.port("port")), user, key, auth == "AGENT");
        } else if id.ends_with("ssl") {
            let path = |keys: &[&str]| Some(prop(keys)).filter(|p| !p.is_empty()).map(|p| dirs.expand(p, None));
            raw.ssl = SslMode::parse(prop(&["sslMode", "ssl.mode", "sslmode"]))
                .or(Some(if flag("ssl.verify.server") { SslMode::VerifyFull } else { SslMode::Require }));
            raw.tls.ca_file = path(&["caCertPath", "ssl.ca", "ssl.ca.cert", "sslrootcert"]);
            raw.tls.client_cert_file = path(&["clientCertPath", "ssl.cert", "ssl.client.cert", "sslcert"]);
            raw.tls.client_key_file = path(&["clientKeyPath", "ssl.key", "ssl.client.key", "sslkey"]);
        }
    }
}

#[cfg(test)]
mod tests {
    use dq_core::config::{Environment, SshAuth};
    use dq_core::engine::Engine;

    use super::super::tests::{dirs, fixture};
    use super::super::{ForeignPreview, SourceApp, read as read_app};
    use super::*;

    fn by_name<'a>(p: &'a ForeignPreview, name: &str) -> &'a super::super::ForeignConnection {
        p.connections.iter().find(|c| c.connection.name == name).unwrap_or_else(|| panic!("{name} missing"))
    }

    #[test]
    fn reads_folders_handlers_and_urls() {
        let path = fixture("home/Library/DBeaverData/workspace6/General");
        let preview = read_app(SourceApp::Dbeaver, &path, &dirs()).unwrap();
        assert_eq!(preview.connections.len(), 5);
        assert_eq!(preview.skipped, ["Mainframe: db2 is not supported."]);

        let eu = by_name(&preview, "EU orders");
        assert_eq!(eu.problem, None);
        let c = &eu.connection;
        assert_eq!(
            (c.folder.as_deref(), c.environment, c.config.read_only),
            (Some("Production / EU"), Environment::Production, true)
        );
        assert_eq!(
            (c.config.engine, c.config.host.as_str(), c.config.port),
            (Engine::Postgres, "orders.eu.example.com", 5432)
        );
        assert_eq!(
            (c.config.ssl_mode, c.config.tls_files.ca_file.as_deref()),
            (SslMode::VerifyFull, Some("/Users/ada/certs/eu-ca.pem"))
        );
        let hop = &c.config.ssh.as_ref().unwrap().host;
        assert_eq!((hop.host.as_str(), hop.username.as_str()), ("bastion.eu.example.com", "tunnel"));
        assert_eq!(hop.auth, SshAuth::PrivateKey { path: "/Users/ada/.ssh/eu_bastion".into() });

        // The user is in DBeaver's encrypted credentials file, which is not read.
        let shop = by_name(&preview, "Shop");
        assert_eq!((shop.connection.config.engine, shop.connection.config.ssl_mode), (Engine::Mysql, SslMode::Require));
        assert_eq!(shop.problem.as_deref(), Some("Username is required."));
        assert_eq!(shop.connection.config.ssh.as_ref().unwrap().host.auth, SshAuth::Agent);

        let erp = by_name(&preview, "ERP");
        let c = &erp.connection.config;
        assert_eq!(
            (c.engine, c.host.as_str(), c.port, c.database.as_str()),
            (Engine::Sqlserver, "erp.corp.local", 1433, "erp")
        );
        assert_eq!((c.ssl_mode, erp.connection.environment), (SslMode::Require, Environment::Development));

        let file = by_name(&preview, "Notes").connection.config.clone();
        assert_eq!((file.engine, file.database.as_str()), (Engine::Sqlite, "/Users/ada/notes.sqlite"));
        assert_eq!(by_name(&preview, "Mariadb staging").connection.config.engine, Engine::Mariadb);
    }
}
