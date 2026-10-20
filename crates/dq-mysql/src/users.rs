//! Accounts from `mysql.user` (needs `SELECT` on the `mysql` database), role
//! edges from `mysql.role_edges` (MySQL 8) or `mysql.roles_mapping`
//! (MariaDB), and privileges from `SHOW GRANTS` and the
//! `information_schema` privilege tables.
//!
//! MySQL marks a role as a locked account without a password; MariaDB has an
//! `is_role` column and gives roles an empty host.

use async_trait::async_trait;
use dq_core::adapter::Session;
use dq_core::redact::redact;
use dq_core::users::{
    Account, AccountList, AccountPrivileges, AccountRef, GrantLevel, PrivilegeRow, ResourceLimits, UserAdmin,
    UserAdminFeatures, group_grants,
};
use dq_core::{DbError, DbResult};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Row};

use crate::connect::Flavor;
use crate::error::map_error;
use crate::session::MySession;
use crate::values::text;

fn my(e: mysql_async::Error) -> DbError {
    map_error(&e, None)
}

fn number(row: &Row, i: usize) -> i64 {
    text(row, i).and_then(|v| v.parse().ok()).unwrap_or(0)
}

fn yes(row: &Row, i: usize) -> Option<bool> {
    text(row, i).map(|v| matches!(v.as_str(), "Y" | "1" | "true"))
}

/// `user@host` from `CURRENT_USER()`; hosts never contain `@`.
fn split_account(value: &str) -> AccountRef {
    match value.rsplit_once('@') {
        Some((name, host)) => AccountRef { name: name.to_owned(), host: Some(host.to_owned()) },
        None => AccountRef { name: value.to_owned(), host: None },
    }
}

/// MariaDB roles have an empty host; they are named without one.
fn account_ref(name: String, host: String) -> AccountRef {
    AccountRef { name, host: Some(host).filter(|h| !h.is_empty()) }
}

/// The `GRANTEE` spelling of `information_schema`: `'name'@'host'`, unescaped.
fn grantee(account: &AccountRef) -> String {
    format!("'{}'@'{}'", account.name, account.host.as_deref().unwrap_or(""))
}

async fn current_account(conn: &mut Conn) -> DbResult<AccountRef> {
    let current: Option<String> = conn.query_first("SELECT CURRENT_USER()").await.map_err(my)?;
    Ok(split_account(&current.unwrap_or_default()))
}

/// (member, role) pairs.
async fn memberships(conn: &mut Conn, flavor: Flavor) -> DbResult<Vec<(AccountRef, AccountRef)>> {
    let sql = match flavor {
        Flavor::Mysql | Flavor::Singlestore => {
            "SELECT TO_USER, TO_HOST, FROM_USER, FROM_HOST FROM mysql.role_edges ORDER BY 3, 4"
        }
        Flavor::Mariadb => "SELECT User, Host, Role, '' FROM mysql.roles_mapping ORDER BY 3",
    };
    let rows: Vec<Row> = conn.query(sql).await.map_err(my)?;
    Ok(rows
        .iter()
        .map(|r| {
            let field = |i| text(r, i).unwrap_or_default();
            (account_ref(field(0), field(1)), account_ref(field(2), field(3)))
        })
        .collect())
}

impl MySession {
    fn has_roles(&self) -> bool {
        match self.flavor() {
            Flavor::Mariadb => true,
            // Never asked: SingleStore sessions offer no `UserAdmin`.
            Flavor::Singlestore => false,
            // `info.version` reads like `MySQL 8.4.3`.
            Flavor::Mysql => {
                let number =
                    self.info().version.split_whitespace().find(|w| w.starts_with(|c: char| c.is_ascii_digit()));
                number.and_then(|v| v.split('.').next()?.parse::<u32>().ok()) >= Some(8)
            }
        }
    }
}

#[async_trait]
impl UserAdmin for MySession {
    fn features(&self) -> UserAdminFeatures {
        UserAdminFeatures::mysql(self.has_roles())
    }

    async fn accounts(&self) -> DbResult<AccountList> {
        let flavor = self.flavor();
        let mut conn = self.catalog_conn().await?;
        let current = current_account(&mut conn).await?;
        let sql = match flavor {
            Flavor::Mysql | Flavor::Singlestore => {
                "SELECT User, Host, account_locked, password_expired, plugin, max_questions, max_updates, \
                        max_connections, max_user_connections, \
                        account_locked = 'Y' AND password_expired = 'Y' AND authentication_string = '' \
                 FROM mysql.user ORDER BY User LIKE 'mysql.%', User, Host"
            }
            Flavor::Mariadb => {
                "SELECT u.User, u.Host, JSON_VALUE(g.Priv, '$.account_locked'), u.password_expired, u.plugin, \
                        u.max_questions, u.max_updates, u.max_connections, u.max_user_connections, u.is_role \
                 FROM mysql.user u LEFT JOIN mysql.global_priv g ON g.User = u.User AND g.Host = u.Host \
                 ORDER BY u.User LIKE 'mariadb.%', u.User, u.Host"
            }
        };
        let mut unavailable = Vec::new();
        let rows: Vec<Row> = match conn.query(sql).await {
            Ok(rows) => rows,
            Err(e) => {
                // Without access to `mysql.user`, show the connected account alone.
                unavailable.push(format!("Other accounts need SELECT on the mysql system database: {}", my(e).message));
                return Ok(AccountList {
                    accounts: vec![Account { current: true, ..bare_account(current) }],
                    unavailable,
                });
            }
        };
        let edges = if self.has_roles() {
            memberships(&mut conn, flavor).await.unwrap_or_else(|e| {
                unavailable.push(format!("Role memberships: {}", e.message));
                Vec::new()
            })
        } else {
            Vec::new()
        };
        let accounts = rows
            .iter()
            .map(|r| {
                let reference = account_ref(text(r, 0).unwrap_or_default(), text(r, 1).unwrap_or_default());
                let is_role = yes(r, 9).unwrap_or(false) || edges.iter().any(|(_, role)| *role == reference);
                Account {
                    role: is_role,
                    flags: Vec::new(),
                    connection_limit: None,
                    valid_until: None,
                    member_of: edges.iter().filter(|(m, _)| *m == reference).map(|(_, role)| role.clone()).collect(),
                    locked: Some(yes(r, 2).unwrap_or(false)),
                    password_expired: yes(r, 3),
                    auth_plugin: text(r, 4).filter(|p| !p.is_empty()),
                    limits: Some(ResourceLimits {
                        max_queries_per_hour: number(r, 5),
                        max_updates_per_hour: number(r, 6),
                        max_connections_per_hour: number(r, 7),
                        max_user_connections: number(r, 8),
                    }),
                    system: reference.name.starts_with("mysql.") || reference.name.starts_with("mariadb."),
                    current: reference == current,
                    name: reference.name,
                    host: reference.host,
                }
            })
            .collect();
        Ok(AccountList { accounts, unavailable })
    }

    /// Privileges are server-wide, so `database` is not needed.
    async fn privileges(&self, account: &AccountRef, _database: &str) -> DbResult<AccountPrivileges> {
        let mut conn = self.catalog_conn().await?;
        let mut unavailable = Vec::new();
        let d = self.dialect();
        let mut who = d.quote_ident(&account.name);
        if let Some(host) = account.host.as_deref() {
            who = format!("{who}@{}", d.quote_ident(host));
        }
        let statements = match conn.query::<String, _>(format!("SHOW GRANTS FOR {who}")).await {
            // MariaDB shows the password hash in `IDENTIFIED BY PASSWORD '…'`.
            Ok(lines) => lines.iter().map(|l| redact(l)).collect(),
            Err(e) => {
                unavailable.push(format!("SHOW GRANTS: {}", my(e).message));
                Vec::new()
            }
        };
        let grantee = grantee(account);
        let rows: Vec<Row> = conn
            .exec(
                "SELECT 0, NULL, NULL, PRIVILEGE_TYPE, IS_GRANTABLE FROM information_schema.USER_PRIVILEGES \
                 WHERE GRANTEE = ? AND PRIVILEGE_TYPE <> 'USAGE' \
                 UNION ALL \
                 SELECT 1, TABLE_SCHEMA, NULL, PRIVILEGE_TYPE, IS_GRANTABLE FROM information_schema.SCHEMA_PRIVILEGES \
                 WHERE GRANTEE = ? \
                 UNION ALL \
                 SELECT 2, TABLE_SCHEMA, TABLE_NAME, PRIVILEGE_TYPE, IS_GRANTABLE FROM information_schema.TABLE_PRIVILEGES \
                 WHERE GRANTEE = ? \
                 ORDER BY 1, 2, 3 LIMIT 5000",
                (&grantee, &grantee, &grantee),
            )
            .await
            .unwrap_or_else(|e| {
                unavailable.push(format!("Privilege tables: {}", my(e).message));
                Vec::new()
            });
        let grants = group_grants(rows.iter().map(|r| {
            let level = match number(r, 0) {
                0 => GrantLevel::Server,
                1 => GrantLevel::Database,
                _ => GrantLevel::Table,
            };
            PrivilegeRow {
                level,
                database: text(r, 1),
                schema: None,
                object: text(r, 2),
                object_kind: (level == GrantLevel::Table).then(|| "table".to_owned()),
                privilege: text(r, 3).unwrap_or_default(),
                grantable: text(r, 4).as_deref() == Some("YES"),
            }
        }));
        Ok(AccountPrivileges { grants, statements, unavailable, ..AccountPrivileges::default() })
    }

    /// Account statements commit on their own in MySQL; they run one by one on
    /// a write connection that is closed afterwards. Backslash escapes are
    /// switched on for it, since the password literal is quoted that way.
    async fn apply(&self, _database: &str, statements: &[String]) -> DbResult<()> {
        let mut conn = self.write_conn().await?;
        let mut result =
            conn.query_drop("SET SESSION sql_mode = REPLACE(@@SESSION.sql_mode, 'NO_BACKSLASH_ESCAPES', '')").await;
        for statement in statements {
            if result.is_err() {
                break;
            }
            result = conn.query_drop(statement).await;
        }
        let _ = conn.disconnect().await;
        result.map_err(|e| self.err(&e))
    }
}

fn bare_account(reference: AccountRef) -> Account {
    Account {
        name: reference.name,
        host: reference.host,
        role: false,
        flags: Vec::new(),
        connection_limit: None,
        valid_until: None,
        member_of: Vec::new(),
        locked: None,
        password_expired: None,
        auth_plugin: None,
        limits: None,
        system: false,
        current: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_names() {
        assert_eq!(split_account("app@10.0.%"), AccountRef { name: "app".into(), host: Some("10.0.%".into()) });
        assert_eq!(split_account("we@ird@localhost").name, "we@ird");
        assert_eq!(account_ref("r".into(), String::new()).host, None);
        assert_eq!(grantee(&AccountRef { name: "o'b".into(), host: Some("%".into()) }), "'o'b'@'%'");
        assert_eq!(grantee(&AccountRef { name: "r".into(), host: None }), "'r'@''");
    }
}
