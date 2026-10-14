//! Confirmation rules for destructive and production writes. Enforced here,
//! not only in the UI: commands refuse to run until the request carries a
//! matching confirmation.

use dq_core::config::Environment;
use dq_core::sql::StatementInfo;
use dq_core::{DbError, DbResult};
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

/// `Ok(None)` to proceed, `Ok(Some(request))` to ask the user first.
pub fn check(
    connection: &SavedConnection,
    settings: &Settings,
    intent: &Intent,
    confirmation: Option<&Confirmation>,
) -> DbResult<Option<ConfirmationRequest>> {
    let Some(request) = required(connection, settings, intent) else {
        return Ok(None);
    };
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
            },
            has_password: false,
            created_at: 0,
            last_used_at: None,
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
    fn production_destructive_cannot_be_disabled() {
        let off = Settings { confirm_destructive: false, ..Settings::default() };
        assert!(required(&conn(Environment::Production), &off, &intent("drop table t")).is_some());
        assert!(required(&conn(Environment::Local), &off, &intent("drop table t")).is_none());
    }
}
