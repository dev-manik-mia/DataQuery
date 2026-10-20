//! Confirmation rules for destructive and production writes. Enforced here,
//! not only in the UI: commands refuse to run until the request carries a
//! matching confirmation.

use dq_core::config::Environment;
use dq_core::sql::StatementInfo;
use dq_core::{DbError, DbResult, ErrorKind};
use dq_store::{SavedConnection, Settings};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Default, Deserialize, TS)]
#[ts(export)]
pub struct Confirmation {
    pub acknowledged: bool,
    /// The connection name, typed by the user, for production.
    pub typed_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ConfirmationRequest {
    pub title: String,
    pub reasons: Vec<String>,
    /// When set, the user must type this connection name.
    pub require_typed_name: Option<String>,
    pub destructive: bool,
    pub connection_name: String,
    pub environment: Environment,
}

/// What is about to happen, summarised for the confirmation rules.
#[derive(Debug, Default)]
pub struct Intent {
    pub writes: bool,
    pub destructive: Vec<String>,
}

impl Intent {
    pub fn from_statements(statements: &[StatementInfo]) -> Self {
        Self {
            writes: statements.iter().any(|s| s.writes),
            destructive: statements.iter().filter_map(|s| s.destructive).map(|d| d.describe().to_owned()).collect(),
        }
    }

    /// Reviewed schema changes always write (`SELECT … INTO` reads like a
    /// query), and removing every row is destructive however the engine spells
    /// it (`ClickHouse` `DELETE … WHERE 1`).
    pub fn schema_changes(statements: &[StatementInfo], deletes_rows: bool) -> Self {
        let mut intent = Self::from_statements(statements);
        intent.writes = true;
        if deletes_rows && intent.destructive.is_empty() {
            intent.destructive.push("Deletes every row of the table.".to_owned());
        }
        intent
    }

    pub fn write() -> Self {
        Self { writes: true, destructive: Vec::new() }
    }
}

/// Returns the confirmation the user must give, or `None` when none is needed.
pub fn required(connection: &SavedConnection, settings: &Settings, intent: &Intent) -> Option<ConfirmationRequest> {
    let production = connection.environment == Environment::Production;
    let destructive = !intent.destructive.is_empty() && (settings.confirm_destructive || production);
    let production_write = production && intent.writes && settings.confirm_production_writes;
    if !destructive && !production_write {
        return None;
    }
    let mut reasons = if destructive { intent.destructive.clone() } else { Vec::new() };
    reasons.dedup();
    if production_write {
        reasons.push(format!("This changes data on the production connection {}.", connection.name));
    }
    Some(ConfirmationRequest {
        title: if destructive { "Run a destructive statement?".into() } else { "Write to production?".into() },
        reasons,
        require_typed_name: (production && destructive).then(|| connection.name.clone()),
        destructive,
        connection_name: connection.name.clone(),
        environment: connection.environment,
    })
}

/// The confirmation for cancelling a statement or terminating a session from Monitoring.
pub fn kill_request(
    connection: &SavedConnection,
    pid: i64,
    user: Option<&str>,
    terminate: bool,
) -> ConfirmationRequest {
    let production = connection.environment == Environment::Production;
    let what = if terminate { "Terminate" } else { "Cancel" };
    let mut reasons = vec![format!("pid {pid} · user {} · connection {}", user.unwrap_or("unknown"), connection.name)];
    reasons.push(if terminate {
        "Terminating ends the session; its open transaction is rolled back.".to_owned()
    } else {
        "Cancelling stops the running statement; its transaction may roll back.".to_owned()
    });
    ConfirmationRequest {
        title: format!("{what} process {pid}?"),
        reasons,
        require_typed_name: (production && terminate).then(|| connection.name.clone()),
        destructive: terminate,
        connection_name: connection.name.clone(),
        environment: connection.environment,
    }
}

/// Checks a confirmation against a request that must always be confirmed.
pub fn confirm(
    request: ConfirmationRequest,
    confirmation: Option<&Confirmation>,
) -> DbResult<Option<ConfirmationRequest>> {
    let Some(confirmation) = confirmation.filter(|c| c.acknowledged) else {
        return Ok(Some(request));
    };
    if let Some(expected) = &request.require_typed_name
        && confirmation.typed_name.as_deref().map(str::trim) != Some(expected.as_str())
    {
        return Err(DbError::invalid(format!("Type {expected} exactly to confirm.")));
    }
    Ok(None)
}

/// `Ok(None)` to proceed, `Ok(Some(request))` to ask the user first.
pub fn check(
    connection: &SavedConnection,
    settings: &Settings,
    intent: &Intent,
    confirmation: Option<&Confirmation>,
) -> DbResult<Option<ConfirmationRequest>> {
    if connection.config.read_only && intent.writes {
        return Err(DbError::new(ErrorKind::Permission, format!("{} is a read-only connection.", connection.name))
            .with_action(Some("Edit the connection and turn off Read-only to make changes.")));
    }
    match required(connection, settings, intent) {
        Some(request) => confirm(request, confirmation),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dq_core::config::{ConnConfig, SslMode};
    use dq_core::engine::Engine;
    use dq_core::sql::{SqlFlavor, split_statements};

    fn conn(env: Environment) -> SavedConnection {
        SavedConnection {
            id: "c".into(),
            name: "kestrel-prod".into(),
            folder_id: None,
            environment: env,
            favorite: false,
            config: ConnConfig {
                engine: Engine::Postgres,
                host: "h".into(),
                port: 5432,
                database: "d".into(),
                username: "u".into(),
                ssl_mode: SslMode::VerifyFull,
                connect_timeout_secs: 10,
                ..ConnConfig::default()
            },
            has_password: false,
            created_at: 0,
            last_used_at: None,
            ssh_secrets: Vec::new(),
        }
    }

    fn intent(sql: &str) -> Intent {
        Intent::from_statements(&split_statements(sql, SqlFlavor::Postgres))
    }

    #[test]
    fn reads_never_need_confirmation() {
        let s = Settings::default();
        assert!(required(&conn(Environment::Production), &s, &intent("select 1")).is_none());
    }

    #[test]
    fn destructive_needs_confirmation_everywhere() {
        let s = Settings::default();
        let dev = required(&conn(Environment::Development), &s, &intent("delete from t")).unwrap();
        assert!(dev.destructive && dev.require_typed_name.is_none());
        let prod = required(&conn(Environment::Production), &s, &intent("drop table t")).unwrap();
        assert_eq!(prod.require_typed_name.as_deref(), Some("kestrel-prod"));
    }

    #[test]
    fn production_writes_need_confirmation() {
        let s = Settings::default();
        assert!(required(&conn(Environment::Development), &s, &intent("update t set a=1 where id=1")).is_none());
        let prod = required(&conn(Environment::Production), &s, &intent("update t set a=1 where id=1")).unwrap();
        assert!(!prod.destructive && prod.require_typed_name.is_none());
        let off = Settings { confirm_production_writes: false, ..Settings::default() };
        assert!(required(&conn(Environment::Production), &off, &intent("insert into t values (1)")).is_none());
    }

    #[test]
    fn typed_name_is_verified() {
        let s = Settings::default();
        let c = conn(Environment::Production);
        let i = intent("truncate t");
        assert!(check(&c, &s, &i, None).unwrap().is_some());
        let wrong = Confirmation { acknowledged: true, typed_name: Some("kestrel".into()) };
        assert!(check(&c, &s, &i, Some(&wrong)).is_err());
        let right = Confirmation { acknowledged: true, typed_name: Some("kestrel-prod".into()) };
        assert!(check(&c, &s, &i, Some(&right)).unwrap().is_none());
    }

    #[test]
    fn read_only_profiles_refuse_writes() {
        let mut c = conn(Environment::Development);
        c.config.read_only = true;
        let s = Settings::default();
        assert!(check(&c, &s, &intent("select 1"), None).unwrap().is_none());
        let err = check(&c, &s, &intent("update t set a = 1 where id = 1"), None).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Permission);
    }

    #[test]
    fn schema_changes_always_write_and_row_removal_is_destructive() {
        let mut ro = conn(Environment::Development);
        ro.config.read_only = true;
        let s = Settings::default();
        let select_into = split_statements("SELECT * INTO copy FROM t WHERE 1 = 0", SqlFlavor::Postgres);
        let err = check(&ro, &s, &Intent::schema_changes(&select_into, false), None).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Permission);
        let where_one = split_statements("DELETE FROM t WHERE 1", SqlFlavor::Postgres);
        assert_eq!(Intent::from_statements(&where_one).destructive, Vec::<String>::new());
        let empty = Intent::schema_changes(&where_one, true);
        assert!(required(&conn(Environment::Development), &s, &empty).unwrap().destructive);
        assert_eq!(
            Intent::schema_changes(&split_statements("truncate t", SqlFlavor::Postgres), true).destructive.len(),
            1
        );
    }

    #[test]
    fn kills_always_ask_and_production_terminate_needs_the_name() {
        let dev = kill_request(&conn(Environment::Development), 42, Some("app"), false);
        assert!(dev.reasons[0].contains("pid 42 · user app"));
        assert!(confirm(dev.clone(), None).unwrap().is_some());
        let yes = Confirmation { acknowledged: true, typed_name: None };
        assert!(confirm(dev, Some(&yes)).unwrap().is_none());
        let prod = kill_request(&conn(Environment::Production), 42, None, true);
        assert_eq!(prod.require_typed_name.as_deref(), Some("kestrel-prod"));
        assert!(confirm(prod, Some(&yes)).is_err());
    }

    #[test]
    fn production_destructive_cannot_be_disabled() {
        let off = Settings { confirm_destructive: false, ..Settings::default() };
        assert!(required(&conn(Environment::Production), &off, &intent("drop table t")).is_some());
        assert!(required(&conn(Environment::Local), &off, &intent("drop table t")).is_none());
    }
}
