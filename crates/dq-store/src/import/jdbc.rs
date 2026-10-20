//! JDBC URLs as `DataGrip` and `DBeaver` store them, e.g.
//! `jdbc:postgresql://h:5432/db?sslmode=require`,
//! `jdbc:sqlserver://h:1433;databaseName=db;encrypt=true`,
//! `jdbc:oracle:thin:@//h:1521/service`, `jdbc:sqlite:/path/app.db`,
//! `jdbc:snowflake://acct.snowflakecomputing.com/?db=DB&warehouse=WH` and
//! `mongodb://h:27017/db`. Credentials inside a URL are dropped.

use std::collections::BTreeMap;

use dq_core::config::{SslMode, parse_connection_url};

use super::{Raw, engine_named};

/// What a JDBC URL says about the server.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Target {
    /// `postgresql`, `mysql`, `sqlserver`, … (`ch` reads as `clickhouse`).
    pub subprotocol: String,
    pub host: String,
    pub port: Option<u16>,
    pub database: String,
    pub username: String,
    pub ssl: Option<SslMode>,
    pub properties: BTreeMap<String, String>,
}

impl Target {
    /// Fills what `raw` does not have yet; the engine comes from the subprotocol if unknown.
    pub fn apply(self, raw: &mut Raw) {
        if raw.engine.is_none() {
            raw.engine = engine_named(&self.subprotocol);
        }
        let fill = |field: &mut String, value: String| {
            if field.trim().is_empty() {
                *field = value;
            }
        };
        fill(&mut raw.host, self.host);
        fill(&mut raw.database, self.database);
        fill(&mut raw.username, self.username);
        raw.port = raw.port.or(self.port);
        raw.ssl = raw.ssl.or(self.ssl);
        for (k, v) in self.properties {
            raw.properties.entry(k).or_insert(v);
        }
    }
}

pub(super) fn parse(url: &str) -> Option<Target> {
    let url = url.trim();
    if url.starts_with("mongodb://") || url.starts_with("mongodb+srv://") {
        return mongo(url);
    }
    let rest = url.get(..5).filter(|p| p.eq_ignore_ascii_case("jdbc:")).map_or(url, |_| &url[5..]);
    let (sub, tail) = rest.split_once(':')?;
    let sub = match sub.to_ascii_lowercase().as_str() {
        "ch" => "clickhouse".to_owned(),
        "es" => "elasticsearch".to_owned(),
        other => other.to_owned(),
    };
    let mut target = Target { subprotocol: sub, ..Target::default() };
    match target.subprotocol.as_str() {
        "sqlite" | "duckdb" => {
            let path = tail.strip_prefix("file:").unwrap_or(tail);
            path.split('?').next().unwrap_or_default().clone_into(&mut target.database);
        }
        "oracle" => oracle(tail, &mut target)?,
        "sqlserver" => sqlserver(tail, &mut target)?,
        "jtds" => sqlserver(tail.strip_prefix("sqlserver:").unwrap_or(tail), &mut target)?,
        _ => hierarchical(tail, &mut target)?,
    }
    Some(target)
}

/// `MongoDB` URLs are read by the connection URL parser; the password is dropped.
fn mongo(url: &str) -> Option<Target> {
    let parsed = parse_connection_url(url).ok()?;
    let c = parsed.config;
    Some(Target {
        subprotocol: "mongodb".into(),
        host: c.host,
        port: Some(c.port),
        database: c.database,
        username: c.username,
        ssl: Some(c.ssl_mode),
        properties: c.properties,
    })
}

fn decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("v={}", s.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

/// `host[:port]`, `[v6]:port`; the first of several hosts.
fn host_port(authority: &str, target: &mut Target) {
    let authority = authority.split(',').next().unwrap_or_default();
    // `user[:password]@host`: keep the user, never the password.
    let authority = match authority.rsplit_once('@') {
        Some((user, host)) => {
            target.username = decode(user.split(':').next().unwrap_or_default());
            host
        }
        None => authority,
    };
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => match v6.split_once(']') {
            Some((h, rest)) => (h, rest.strip_prefix(':')),
            None => (v6, None),
        },
        None => match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        },
    };
    target.host = decode(host);
    target.port = port.and_then(|p| p.parse().ok());
}

/// `[proto:]//authority[/database][?k=v&…]`, with `;k=v` parameters too.
fn hierarchical(tail: &str, target: &mut Target) -> Option<()> {
    let (scheme, after) = tail.split_once("//")?;
    let scheme = scheme.trim_end_matches(':').to_ascii_lowercase();
    if scheme == "https" || scheme.ends_with("+s") {
        target.ssl = Some(SslMode::Require);
    }
    let end = after.find(['/', '?', ';']).unwrap_or(after.len());
    host_port(&after[..end], target);
    let rest = &after[end..];
    let (path, query) = rest.split_once(['?', ';']).unwrap_or((rest, ""));
    target.database = decode(path.trim_start_matches('/').split('/').next().unwrap_or_default());
    params(query_pairs(query, &['&', ';']), target);
    Some(())
}

fn query_pairs<'a>(query: &'a str, separators: &[char]) -> impl Iterator<Item = (&'a str, &'a str)> {
    query.split(separators).filter_map(|pair| pair.split_once('='))
}

/// Driver parameters that DataQuery keeps: TLS, database, user and Snowflake settings.
pub(super) fn params<'a>(pairs: impl Iterator<Item = (&'a str, &'a str)>, target: &mut Target) {
    for (key, value) in pairs {
        let value = decode(value.trim());
        let lower = value.to_ascii_lowercase();
        let on = matches!(lower.as_str(), "true" | "yes" | "1" | "strict" | "mandatory" | "require" | "required");
        let off = matches!(lower.as_str(), "false" | "no" | "0" | "off" | "disable" | "disabled");
        match key.trim().to_ascii_lowercase().as_str() {
            "sslmode" | "ssl-mode" | "ssl_mode" => target.ssl = SslMode::parse(&value).or(target.ssl),
            "ssl" | "usessl" | "requiressl" | "tls" | "secure" | "encrypt" if off => {
                target.ssl = Some(SslMode::Disable);
            }
            "ssl" | "usessl" | "requiressl" | "tls" | "secure" | "encrypt" if on && target.ssl.is_none() => {
                target.ssl = Some(SslMode::Require);
            }
            "trustservercertificate" if on && target.ssl != Some(SslMode::Disable) => {
                target.ssl = Some(SslMode::Require);
            }
            "db" | "database" | "databasename" if target.database.is_empty() => target.database = value,
            "user" | "username" if target.username.is_empty() => target.username = value,
            "warehouse" | "role" | "schema" if target.subprotocol == "snowflake" => {
                target.properties.insert(key.trim().to_ascii_lowercase(), value);
            }
            _ => {}
        }
    }
}

/// `//host[\instance][:port];databaseName=db;encrypt=true` (jTDS: `//host:port/db;…`).
fn sqlserver(tail: &str, target: &mut Target) -> Option<()> {
    let after = tail.strip_prefix("//")?;
    let (head, query) = after.split_once(';').unwrap_or((after, ""));
    let (authority, database) = head.split_once('/').unwrap_or((head, ""));
    // A named instance is resolved by SQL Browser, which DataQuery does not use; the port is kept.
    let (host, port) = authority.split_once(':').unwrap_or((authority, ""));
    let host = host.split('\\').next().unwrap_or_default();
    host_port(&if port.is_empty() { host.to_owned() } else { format!("{host}:{port}") }, target);
    target.database = decode(database);
    params(query_pairs(query, &[';']), target);
    Some(())
}

/// `thin:@//host:port/service`, `thin:@host:port:SID`, `thin:@host:port/service`
/// or a `(DESCRIPTION=…)` descriptor. A TNS alias has no host and is left empty.
fn oracle(tail: &str, target: &mut Target) -> Option<()> {
    let (_, address) = tail.split_once('@')?;
    let address = address.trim();
    if address.starts_with('(') {
        let upper = address.to_ascii_uppercase();
        let value = |key: &str| {
            let start = upper.find(&format!("({key}="))? + key.len() + 2;
            let len = address[start..].find(')')?;
            Some(address[start..start + len].trim().to_owned())
        };
        target.host = value("HOST").unwrap_or_default();
        target.port = value("PORT").and_then(|p| p.parse().ok());
        target.database = value("SERVICE_NAME").or_else(|| value("SID")).unwrap_or_default();
        if upper.contains("(PROTOCOL=TCPS)") {
            target.ssl = Some(SslMode::VerifyFull);
        }
        return Some(());
    }
    let address = address.trim_start_matches("//");
    let address = address.split('?').next().unwrap_or_default();
    let (authority, service) = match address.split_once('/') {
        Some((a, s)) => (a, s),
        // host:port:SID
        None => match address.splitn(3, ':').collect::<Vec<_>>().as_slice() {
            [h, p, sid] => {
                host_port(&format!("{h}:{p}"), target);
                (*sid).clone_into(&mut target.database);
                return Some(());
            }
            _ => (address, ""),
        },
    };
    if authority.contains(':') || authority.contains('.') || authority == "localhost" {
        host_port(authority, target);
        service.clone_into(&mut target.database);
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(url: &str) -> Target {
        parse(url).unwrap_or_else(|| panic!("{url} did not parse"))
    }

    #[test]
    fn parses_hierarchical_urls() {
        let pg = t("jdbc:postgresql://db.example.com:6432/app?sslmode=verify-full&user=svc");
        assert_eq!((pg.host.as_str(), pg.port, pg.database.as_str()), ("db.example.com", Some(6432), "app"));
        assert_eq!((pg.ssl, pg.username.as_str()), (Some(SslMode::VerifyFull), "svc"));
        let my = t("jdbc:mysql://user:hunter2@[::1]:3307/shop?useSSL=false");
        assert_eq!((my.host.as_str(), my.port, my.username.as_str()), ("::1", Some(3307), "user"));
        assert_eq!(my.ssl, Some(SslMode::Disable));
        let https = t("jdbc:clickhouse:https://ch.example.com:8443/logs");
        assert_eq!(
            (https.subprotocol.as_str(), https.ssl, https.port),
            ("clickhouse", Some(SslMode::Require), Some(8443))
        );
        let ch = t("jdbc:ch://localhost/default");
        assert_eq!((ch.subprotocol.as_str(), ch.port), ("clickhouse", None));
        let sf = t("jdbc:snowflake://xy123.eu-west-1.snowflakecomputing.com/?db=SALES&warehouse=WH&role=ANALYST");
        assert_eq!((sf.host.as_str(), sf.database.as_str()), ("xy123.eu-west-1.snowflakecomputing.com", "SALES"));
        assert_eq!(sf.properties.get("warehouse").map(String::as_str), Some("WH"));
        let lb = t("jdbc:mariadb:loadbalance://a.local:3306,b.local:3306/app");
        assert_eq!((lb.host.as_str(), lb.database.as_str()), ("a.local", "app"));
    }

    #[test]
    fn parses_sql_server_oracle_and_files() {
        let s =
            t("jdbc:sqlserver://sql.local\\SQLEXPRESS:1434;databaseName=erp;encrypt=true;trustServerCertificate=true");
        assert_eq!((s.host.as_str(), s.port, s.database.as_str()), ("sql.local", Some(1434), "erp"));
        assert_eq!(s.ssl, Some(SslMode::Require));
        let j = t("jdbc:jtds:sqlserver://sql.local:1433/erp;ssl=off");
        assert_eq!((j.host.as_str(), j.database.as_str(), j.ssl), ("sql.local", "erp", Some(SslMode::Disable)));
        let o = t("jdbc:oracle:thin:@//ora.local:1521/ORCLPDB1");
        assert_eq!((o.host.as_str(), o.port, o.database.as_str()), ("ora.local", Some(1521), "ORCLPDB1"));
        let sid = t("jdbc:oracle:thin:scott/tiger@ora.local:1521:XE");
        assert_eq!((sid.host.as_str(), sid.database.as_str(), sid.username.as_str()), ("ora.local", "XE", ""));
        let d = t(
            "jdbc:oracle:thin:@(DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=ora.example.com)(PORT=2484))(CONNECT_DATA=(SERVICE_NAME=sales)))",
        );
        assert_eq!((d.host.as_str(), d.port, d.database.as_str()), ("ora.example.com", Some(2484), "sales"));
        assert_eq!(d.ssl, Some(SslMode::VerifyFull));
        assert_eq!(t("jdbc:oracle:thin:@PRODTNS").host, "");
        assert_eq!(t("jdbc:sqlite:/Users/ada/app.db").database, "/Users/ada/app.db");
        assert_eq!(t("jdbc:duckdb:/data/w.duckdb").database, "/data/w.duckdb");
    }

    #[test]
    fn parses_mongo_without_the_password() {
        let m = t("mongodb://reader:secret@mongo.local:27018/app?authSource=admin");
        assert_eq!((m.host.as_str(), m.port, m.username.as_str()), ("mongo.local", Some(27018), "reader"));
        assert_eq!(m.properties.get("auth_source").map(String::as_str), Some("admin"));
        assert!(parse("not a url").is_none());
    }
}
