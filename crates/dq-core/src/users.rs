//! User and role management: accounts, their attributes, privileges and
//! role memberships. Changes are planned as [`UserChange`]s and turned into
//! SQL by [`crate::sql::users`]; what an engine supports is described by its
//! [`UserAdminFeatures`], so forms and builders follow the same data.
//!
//! Passwords travel only inside [`Password`], whose `Debug` output is masked.
//! Previews show them as `'***'`; the executed statement carries the value the
//! adapter prepares ([`UserAdmin::password_secret`]).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::error::DbResult;

/// A password typed into a form. Never logged, stored or serialised back.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Password(String);

impl Password {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(crate::redact::MASK)
    }
}

/// How the engine names accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum AccountModel {
    /// Roles by name; a user is a role that can log in (`PostgreSQL`, `CockroachDB`).
    Role,
    /// Accounts are `'user'@'host'`; roles are accounts too (`MySQL`, `MariaDB`).
    UserHost,
}

/// A role attribute switched on or off (`LOGIN` / `NOLOGIN`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum RoleFlag {
    Login,
    Superuser,
    Createdb,
    Createrole,
    Inherit,
    Replication,
    Bypassrls,
}

impl RoleFlag {
    /// The keyword that sets (`on`) or clears the attribute.
    pub fn keyword(self, on: bool) -> &'static str {
        match (self, on) {
            (Self::Login, true) => "LOGIN",
            (Self::Login, false) => "NOLOGIN",
            (Self::Superuser, true) => "SUPERUSER",
            (Self::Superuser, false) => "NOSUPERUSER",
            (Self::Createdb, true) => "CREATEDB",
            (Self::Createdb, false) => "NOCREATEDB",
            (Self::Createrole, true) => "CREATEROLE",
            (Self::Createrole, false) => "NOCREATEROLE",
            (Self::Inherit, true) => "INHERIT",
            (Self::Inherit, false) => "NOINHERIT",
            (Self::Replication, true) => "REPLICATION",
            (Self::Replication, false) => "NOREPLICATION",
            (Self::Bypassrls, true) => "BYPASSRLS",
            (Self::Bypassrls, false) => "NOBYPASSRLS",
        }
    }
}

/// Where a privilege applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum GrantLevel {
    /// Every database (`ON *.*`).
    Server,
    Database,
    Schema,
    /// Every existing table of a schema (`ON ALL TABLES IN SCHEMA`).
    SchemaTables,
    Table,
}

/// Privileges that can be granted at one level, in display order.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct GrantLevelSpec {
    pub level: GrantLevel,
    pub privileges: Vec<&'static str>,
}

/// What the engine's user management supports. The forms show only these
/// controls, and the SQL builders refuse anything else.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct UserAdminFeatures {
    pub model: AccountModel,
    /// Attributes that can be switched, in form order.
    pub flags: Vec<RoleFlag>,
    /// Role accounts and role membership.
    pub roles: bool,
    /// `CONNECTION LIMIT`.
    pub connection_limit: bool,
    /// `VALID UNTIL` (password expiry time).
    pub valid_until: bool,
    pub rename: bool,
    /// `ACCOUNT LOCK` / `ACCOUNT UNLOCK`.
    pub lock: bool,
    /// `PASSWORD EXPIRE`.
    pub expire_password: bool,
    /// Per-hour query, update and connection limits.
    pub resource_limits: bool,
    /// `REASSIGN OWNED` / `DROP OWNED` before dropping a role. They act on one database.
    pub owned_objects: bool,
    pub levels: Vec<GrantLevelSpec>,
}

impl UserAdminFeatures {
    pub fn level(&self, level: GrantLevel) -> Option<&GrantLevelSpec> {
        self.levels.iter().find(|l| l.level == level)
    }
}

/// An account: a role name, plus the host for `'user'@'host'` engines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AccountRef {
    pub name: String,
    /// `None` for roles by name (and `MariaDB` roles, which have no host).
    pub host: Option<String>,
}

/// `MySQL` / `MariaDB` resource limits; `0` means unlimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ResourceLimits {
    #[ts(type = "number")]
    pub max_queries_per_hour: i64,
    #[ts(type = "number")]
    pub max_updates_per_hour: i64,
    #[ts(type = "number")]
    pub max_connections_per_hour: i64,
    #[ts(type = "number")]
    pub max_user_connections: i64,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct Account {
    pub name: String,
    pub host: Option<String>,
    /// A role rather than a login: `NOLOGIN` (PostgreSQL) or a role account (MySQL 8, MariaDB).
    pub role: bool,
    /// Attributes that are on.
    pub flags: Vec<RoleFlag>,
    /// `None` when unlimited.
    #[ts(type = "number | null")]
    pub connection_limit: Option<i64>,
    pub valid_until: Option<String>,
    /// Roles this account is a member of.
    pub member_of: Vec<AccountRef>,
    pub locked: Option<bool>,
    pub password_expired: Option<bool>,
    pub auth_plugin: Option<String>,
    pub limits: Option<ResourceLimits>,
    /// Built-in account of the server (`pg_*` roles, `mysql.sys`, …).
    pub system: bool,
    /// The account this connection uses.
    pub current: bool,
}

impl Account {
    pub fn reference(&self) -> AccountRef {
        AccountRef { name: self.name.clone(), host: self.host.clone() }
    }
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct AccountList {
    pub accounts: Vec<Account>,
    /// Information that could not be read, with the reason (privileges, version).
    pub unavailable: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
pub struct GrantedPrivilege {
    pub privilege: String,
    /// Granted `WITH GRANT OPTION`.
    pub grantable: bool,
}

/// Privileges an account holds on one object.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct Grant {
    pub level: GrantLevel,
    pub database: Option<String>,
    pub schema: Option<String>,
    /// Table, view or sequence name, for object-level grants.
    pub object: Option<String>,
    /// e.g. `table`, `view`, `sequence`.
    pub object_kind: Option<String>,
    pub privileges: Vec<GrantedPrivilege>,
}

/// Privileges granted automatically on objects a role creates later.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct DefaultPrivilege {
    /// The role whose new objects get the privileges.
    pub owner: String,
    pub schema: Option<String>,
    /// `tables`, `sequences`, `functions`, `types` or `schemas`.
    pub object_type: String,
    pub privileges: Vec<GrantedPrivilege>,
}

/// What ties a role to one database and blocks `DROP ROLE` until it is
/// reassigned, dropped or revoked there.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct OwnedObjects {
    /// `None` for objects shared by the whole server (databases, tablespaces).
    pub database: Option<String>,
    /// Objects the role owns.
    #[ts(type = "number")]
    pub owned: i64,
    /// Objects the role holds privileges on.
    #[ts(type = "number")]
    pub privileges: i64,
}

#[derive(Debug, Clone, Default, Serialize, TS)]
#[ts(export)]
pub struct AccountPrivileges {
    pub grants: Vec<Grant>,
    pub default_privileges: Vec<DefaultPrivilege>,
    pub owned: Vec<OwnedObjects>,
    /// The server's own account of the grants (`SHOW GRANTS`), secrets masked.
    pub statements: Vec<String>,
    pub unavailable: Vec<String>,
}

/// One privilege row as catalogs list them, before grouping by object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivilegeRow {
    pub level: GrantLevel,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub object: Option<String>,
    pub object_kind: Option<String>,
    pub privilege: String,
    pub grantable: bool,
}

/// Groups privilege rows into one [`Grant`] per object, keeping the rows' order.
pub fn group_grants(rows: impl IntoIterator<Item = PrivilegeRow>) -> Vec<Grant> {
    let mut grants: Vec<Grant> = Vec::new();
    for row in rows {
        let privilege = GrantedPrivilege { privilege: row.privilege, grantable: row.grantable };
        let same = |g: &&mut Grant| {
            g.level == row.level && g.database == row.database && g.schema == row.schema && g.object == row.object
        };
        match grants.iter_mut().rev().find(same) {
            Some(grant) if !grant.privileges.contains(&privilege) => grant.privileges.push(privilege),
            Some(_) => {}
            None => grants.push(Grant {
                level: row.level,
                database: row.database,
                schema: row.schema,
                object: row.object,
                object_kind: row.object_kind,
                privileges: vec![privilege],
            }),
        }
    }
    grants
}

/// An attribute set or cleared in a create or alter form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, TS)]
#[ts(export)]
pub struct FlagSetting {
    pub flag: RoleFlag,
    pub on: bool,
}

/// Account attributes in a create or alter form. `None` leaves one unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, TS)]
#[ts(export)]
pub struct AccountAttributes {
    pub flags: Vec<FlagSetting>,
    /// `-1` removes the limit.
    #[ts(type = "number | null")]
    pub connection_limit: Option<i64>,
    /// A timestamp, or `infinity` to remove the expiry.
    pub valid_until: Option<String>,
    pub locked: Option<bool>,
    /// Makes the password expire now.
    pub expire_password: bool,
    pub limits: Option<ResourceLimits>,
}

/// The object of a grant or revoke.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, TS)]
#[serde(tag = "level", rename_all = "snake_case")]
#[ts(export)]
pub enum GrantTarget {
    Server,
    Database { database: String },
    Schema { database: String, schema: String },
    SchemaTables { database: String, schema: String },
    Table { database: String, schema: Option<String>, table: String },
}

impl GrantTarget {
    pub fn level(&self) -> GrantLevel {
        match self {
            Self::Server => GrantLevel::Server,
            Self::Database { .. } => GrantLevel::Database,
            Self::Schema { .. } => GrantLevel::Schema,
            Self::SchemaTables { .. } => GrantLevel::SchemaTables,
            Self::Table { .. } => GrantLevel::Table,
        }
    }
}

/// One change from the Users & roles forms.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, TS)]
#[serde(tag = "op", rename_all = "snake_case")]
#[ts(export)]
pub enum UserChange {
    Create {
        account: AccountRef,
        /// A role (`CREATE ROLE`) rather than a user (MySQL/MariaDB). PostgreSQL uses the `login` flag.
        role: bool,
        #[ts(type = "string | null")]
        password: Option<Password>,
        attributes: AccountAttributes,
        member_of: Vec<AccountRef>,
    },
    Alter {
        account: AccountRef,
        attributes: AccountAttributes,
    },
    /// `None` removes the password (PostgreSQL).
    SetPassword {
        account: AccountRef,
        #[ts(type = "string | null")]
        password: Option<Password>,
    },
    Rename {
        account: AccountRef,
        new_name: String,
    },
    Drop {
        account: AccountRef,
        role: bool,
        /// `REASSIGN OWNED BY … TO` this role first.
        reassign_to: Option<String>,
        /// `DROP OWNED BY` first: drops the role's objects and privileges.
        drop_owned: bool,
        /// Database the owned-object statements act on.
        database: Option<String>,
    },
    Grant {
        account: AccountRef,
        target: GrantTarget,
        privileges: Vec<String>,
        grant_option: bool,
    },
    Revoke {
        account: AccountRef,
        target: GrantTarget,
        privileges: Vec<String>,
        /// Revoke only the right to grant them on.
        grant_option_only: bool,
    },
    GrantRole {
        account: AccountRef,
        role: AccountRef,
        admin_option: bool,
    },
    RevokeRole {
        account: AccountRef,
        role: AccountRef,
    },
}

impl UserChange {
    pub fn account(&self) -> &AccountRef {
        match self {
            Self::Create { account, .. }
            | Self::Alter { account, .. }
            | Self::SetPassword { account, .. }
            | Self::Rename { account, .. }
            | Self::Drop { account, .. }
            | Self::Grant { account, .. }
            | Self::Revoke { account, .. }
            | Self::GrantRole { account, .. }
            | Self::RevokeRole { account, .. } => account,
        }
    }

    /// The password the change sets, if any.
    pub fn password(&self) -> Option<&Password> {
        match self {
            Self::Create { password, .. } | Self::SetPassword { password, .. } => password.as_ref(),
            _ => None,
        }
    }

    /// The database the statements must run in (schema and table grants,
    /// owned objects); empty for the connection's database.
    pub fn database(&self) -> &str {
        match self {
            Self::Grant { target, .. } | Self::Revoke { target, .. } => match target {
                GrantTarget::Schema { database, .. }
                | GrantTarget::SchemaTables { database, .. }
                | GrantTarget::Table { database, .. } => database,
                GrantTarget::Server | GrantTarget::Database { .. } => "",
            },
            Self::Drop { database: Some(database), .. } => database,
            _ => "",
        }
    }

    /// Why the change needs a destructive confirmation, if it does.
    pub fn risks(&self) -> Vec<&'static str> {
        match self {
            Self::Drop { drop_owned, .. } => {
                let mut risks = vec!["Dropping the account ends its access to the server."];
                if *drop_owned {
                    risks.push("DROP OWNED drops every object the role owns in this database.");
                }
                risks
            }
            Self::Revoke { .. } | Self::RevokeRole { .. } => {
                vec!["Revoking privileges can break applications that use this account."]
            }
            _ => Vec::new(),
        }
    }
}

/// Implemented by sessions of engines with user management. Reads run on
/// catalog connections; changes on a write connection.
#[async_trait]
pub trait UserAdmin: Send + Sync {
    fn features(&self) -> UserAdminFeatures;

    /// Every account with its attributes and role memberships.
    async fn accounts(&self) -> DbResult<AccountList>;

    /// The account's privileges. Schema- and table-level ones are read from
    /// `database` (empty: the connection's database).
    async fn privileges(&self, account: &AccountRef, database: &str) -> DbResult<AccountPrivileges>;

    /// The text the executed statement carries for `password`. PostgreSQL
    /// sends a SCRAM verifier, so the clear password never reaches the server.
    fn password_secret(&self, password: &str) -> String {
        password.to_owned()
    }

    /// Runs reviewed statements, in one transaction where the engine allows it.
    async fn apply(&self, database: &str, statements: &[String]) -> DbResult<()>;
}

const PG_TABLE: &[&str] = &["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER"];
const COCKROACH_TABLE: &[&str] = &["SELECT", "INSERT", "UPDATE", "DELETE", "DROP"];
const MYSQL_TABLE: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "CREATE",
    "DROP",
    "ALTER",
    "INDEX",
    "REFERENCES",
    "TRIGGER",
    "CREATE VIEW",
    "SHOW VIEW",
];
const MYSQL_DATABASE: &[&str] =
    &["CREATE ROUTINE", "ALTER ROUTINE", "EXECUTE", "EVENT", "CREATE TEMPORARY TABLES", "LOCK TABLES"];
const MYSQL_SERVER: &[&str] =
    &["PROCESS", "RELOAD", "SHOW DATABASES", "CREATE USER", "REPLICATION CLIENT", "REPLICATION SLAVE"];

fn level(level: GrantLevel, groups: &[&[&'static str]]) -> GrantLevelSpec {
    GrantLevelSpec { level, privileges: groups.concat() }
}

impl UserAdminFeatures {
    pub fn postgres() -> Self {
        Self {
            model: AccountModel::Role,
            flags: vec![
                RoleFlag::Login,
                RoleFlag::Superuser,
                RoleFlag::Createdb,
                RoleFlag::Createrole,
                RoleFlag::Inherit,
                RoleFlag::Replication,
                RoleFlag::Bypassrls,
            ],
            roles: true,
            connection_limit: true,
            valid_until: true,
            rename: true,
            lock: false,
            expire_password: false,
            resource_limits: false,
            owned_objects: true,
            levels: vec![
                level(GrantLevel::Database, &[&["CONNECT", "CREATE", "TEMPORARY"]]),
                level(GrantLevel::Schema, &[&["USAGE", "CREATE"]]),
                level(GrantLevel::SchemaTables, &[PG_TABLE]),
                level(GrantLevel::Table, &[PG_TABLE]),
            ],
        }
    }

    /// `CockroachDB`: fewer role options, no renames or connection limits.
    pub fn cockroach() -> Self {
        Self {
            flags: vec![RoleFlag::Login, RoleFlag::Createdb, RoleFlag::Createrole],
            connection_limit: false,
            rename: false,
            levels: vec![
                level(GrantLevel::Database, &[&["CONNECT", "CREATE"]]),
                level(GrantLevel::Schema, &[&["USAGE", "CREATE"]]),
                level(GrantLevel::SchemaTables, &[COCKROACH_TABLE]),
                level(GrantLevel::Table, &[COCKROACH_TABLE]),
            ],
            ..Self::postgres()
        }
    }

    /// `MySQL` and `MariaDB`. `roles`: the server has roles (`MySQL` 8, `MariaDB` 10.0.5+).
    pub fn mysql(roles: bool) -> Self {
        Self {
            model: AccountModel::UserHost,
            flags: Vec::new(),
            roles,
            connection_limit: false,
            valid_until: false,
            rename: true,
            lock: true,
            expire_password: true,
            resource_limits: true,
            owned_objects: false,
            levels: vec![
                level(GrantLevel::Server, &[MYSQL_TABLE, MYSQL_DATABASE, MYSQL_SERVER]),
                level(GrantLevel::Database, &[MYSQL_TABLE, MYSQL_DATABASE]),
                level(GrantLevel::Table, &[MYSQL_TABLE]),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(object: &str, privilege: &str, grantable: bool) -> PrivilegeRow {
        PrivilegeRow {
            level: GrantLevel::Table,
            database: Some("app".into()),
            schema: Some("public".into()),
            object: Some(object.into()),
            object_kind: Some("table".into()),
            privilege: privilege.into(),
            grantable,
        }
    }

    #[test]
    fn grants_are_grouped_per_object() {
        let grants = group_grants([
            row("a", "SELECT", false),
            row("a", "UPDATE", true),
            row("b", "SELECT", false),
            row("a", "SELECT", false),
        ]);
        assert_eq!(grants.len(), 2);
        assert_eq!(grants[0].privileges.len(), 2);
        assert!(grants[0].privileges[1].grantable);
        assert_eq!(grants[1].object.as_deref(), Some("b"));
    }

    #[test]
    fn changes_name_their_database_and_risks() {
        let account = AccountRef { name: "u".into(), host: None };
        let grant = UserChange::Grant {
            account: account.clone(),
            target: GrantTarget::Database { database: "app".into() },
            privileges: vec!["CONNECT".into()],
            grant_option: false,
        };
        assert_eq!(grant.database(), "");
        assert_eq!(grant.risks().len(), 0);
        let password = UserChange::SetPassword { account, password: Some(Password::new("x")) };
        assert_eq!(password.password().map(Password::expose), Some("x"));
        assert!(format!("{password:?}").contains("password: Some(***)"));
    }
}
