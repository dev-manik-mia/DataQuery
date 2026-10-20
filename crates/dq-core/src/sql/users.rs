//! SQL for user and role changes ([`crate::users`]). Names are always quoted
//! by the dialect, privileges must be ones the engine's [`UserAdminFeatures`]
//! list, and the password is the only literal: previews show it masked, and
//! the executed statement carries what the adapter prepared for it.

use super::dialect::Dialect;
use crate::error::{DbError, DbResult};
use crate::redact::MASK;
use crate::users::{AccountAttributes, AccountModel, AccountRef, GrantTarget, Password, UserAdminFeatures, UserChange};

/// Turns a typed password into the text the statement carries.
pub type SecretFn<'a> = &'a dyn Fn(&str) -> String;

/// The statements for `change` as shown for review: passwords masked.
pub fn preview(d: &dyn Dialect, features: &UserAdminFeatures, change: &UserChange) -> DbResult<Vec<String>> {
    statements(d, features, change, &|_| MASK.to_owned())
}

/// The statements for `change`. `secret` turns a password into the text the
/// statement carries; use [`preview`] for anything shown or stored.
pub fn statements(
    d: &dyn Dialect,
    features: &UserAdminFeatures,
    change: &UserChange,
    secret: SecretFn<'_>,
) -> DbResult<Vec<String>> {
    let b = Builder { d, f: features, secret };
    let roles = features.model == AccountModel::Role;
    Ok(match change {
        UserChange::Create { account, role, password, attributes, member_of } => {
            b.create(account, *role, password.as_ref(), attributes, member_of)?
        }
        UserChange::Alter { account, attributes } => {
            let clauses = b.attributes(attributes)?;
            if clauses.is_empty() {
                return Err(DbError::invalid("Nothing to change."));
            }
            let head = if roles { "ALTER ROLE" } else { "ALTER USER" };
            vec![format!("{head} {}{}", b.account(account)?, joined(roles, &clauses))]
        }
        UserChange::SetPassword { account, password } => {
            let name = b.account(account)?;
            match (roles, password) {
                (true, Some(p)) => vec![format!("ALTER ROLE {name} PASSWORD {}", b.password(p))],
                (true, None) => vec![format!("ALTER ROLE {name} PASSWORD NULL")],
                (false, Some(p)) => vec![format!("ALTER USER {name} IDENTIFIED BY {}", b.password(p))],
                (false, None) => return Err(DbError::invalid("Enter the new password.")),
            }
        }
        UserChange::Rename { account, new_name } => {
            if !features.rename {
                return Err(DbError::unsupported("This server cannot rename accounts."));
            }
            let renamed = AccountRef { name: new_name.clone(), host: account.host.clone() };
            let (from, to) = (b.account(account)?, b.account(&renamed)?);
            vec![if roles {
                format!("ALTER ROLE {from} RENAME TO {to}")
            } else {
                format!("RENAME USER {from} TO {to}")
            }]
        }
        UserChange::Drop { account, role, reassign_to, drop_owned, .. } => {
            let name = b.account(account)?;
            let mut out = Vec::new();
            if reassign_to.is_some() || *drop_owned {
                if !features.owned_objects {
                    return Err(DbError::unsupported("This server has no REASSIGN OWNED or DROP OWNED."));
                }
                if let Some(to) = reassign_to {
                    let to = b.account(&AccountRef { name: to.clone(), host: None })?;
                    out.push(format!("REASSIGN OWNED BY {name} TO {to}"));
                }
                if *drop_owned {
                    out.push(format!("DROP OWNED BY {name}"));
                }
            }
            out.push(format!("DROP {} {name}", if roles || *role { "ROLE" } else { "USER" }));
            out
        }
        UserChange::Grant { account, target, privileges, grant_option } => {
            let privileges = b.privileges(target, privileges)?;
            let on = b.target(target)?;
            let option = if *grant_option { " WITH GRANT OPTION" } else { "" };
            vec![format!("GRANT {privileges} ON {on} TO {}{option}", b.account(account)?)]
        }
        UserChange::Revoke { account, target, privileges, grant_option_only } => {
            let on = b.target(target)?;
            let name = b.account(account)?;
            // MySQL keeps GRANT OPTION per object, not per privilege.
            if *grant_option_only && !roles {
                b.privileges(target, privileges)?;
                vec![format!("REVOKE GRANT OPTION ON {on} FROM {name}")]
            } else {
                let option = if *grant_option_only { "GRANT OPTION FOR " } else { "" };
                vec![format!("REVOKE {option}{} ON {on} FROM {name}", b.privileges(target, privileges)?)]
            }
        }
        UserChange::GrantRole { account, role, admin_option } => {
            b.roles()?;
            let option = if *admin_option { " WITH ADMIN OPTION" } else { "" };
            vec![format!("GRANT {} TO {}{option}", b.account(role)?, b.account(account)?)]
        }
        UserChange::RevokeRole { account, role } => {
            b.roles()?;
            vec![format!("REVOKE {} FROM {}", b.account(role)?, b.account(account)?)]
        }
    })
}

/// ` WITH a b` for roles (PostgreSQL's optional `WITH`), ` a b` for MySQL clauses.
fn joined(with: bool, clauses: &[String]) -> String {
    match (clauses.is_empty(), with) {
        (true, _) => String::new(),
        (false, true) => format!(" WITH {}", clauses.join(" ")),
        (false, false) => format!(" {}", clauses.join(" ")),
    }
}

fn check_name(value: &str, what: &str, max: usize) -> DbResult<()> {
    if value.trim().is_empty() || value.chars().count() > max || value.chars().any(char::is_control) {
        return Err(DbError::invalid(format!("{what} must be 1–{max} characters, without control characters.")));
    }
    Ok(())
}

/// A timestamp such as `2030-01-01 00:00:00+00`, or `infinity`.
fn check_timestamp(value: &str) -> DbResult<()> {
    let ok = value.eq_ignore_ascii_case("infinity")
        || (!value.is_empty()
            && value.len() <= 40
            && value.starts_with(|c: char| c.is_ascii_digit())
            && value.chars().all(|c| c.is_ascii_digit() || " -:+.T".contains(c)));
    if ok { Ok(()) } else { Err(DbError::invalid("Enter the expiry as YYYY-MM-DD [HH:MM:SS], or infinity.")) }
}

struct Builder<'a> {
    d: &'a dyn Dialect,
    f: &'a UserAdminFeatures,
    secret: SecretFn<'a>,
}

impl Builder<'_> {
    fn account(&self, a: &AccountRef) -> DbResult<String> {
        check_name(&a.name, "The account name", 128)?;
        let name = self.d.quote_ident(&a.name);
        match (&a.host, self.f.model) {
            (Some(host), AccountModel::UserHost) => {
                // An empty host is MariaDB's spelling of a role.
                if host.is_empty() {
                    return Ok(name);
                }
                check_name(host, "The host", 255)?;
                Ok(format!("{name}@{}", self.d.quote_ident(host)))
            }
            _ => Ok(name),
        }
    }

    /// The only literal in executed SQL. PostgreSQL adapters send a SCRAM
    /// verifier (no quotes or backslashes); MySQL adapters run these statements
    /// with backslash escapes on, which this quoting assumes.
    fn password(&self, password: &Password) -> String {
        self.d.quote_literal(&(self.secret)(password.expose()))
    }

    fn roles(&self) -> DbResult<()> {
        if self.f.roles { Ok(()) } else { Err(DbError::unsupported("This server version has no roles.")) }
    }

    fn create(
        &self,
        account: &AccountRef,
        role: bool,
        password: Option<&Password>,
        attributes: &AccountAttributes,
        member_of: &[AccountRef],
    ) -> DbResult<Vec<String>> {
        let name = self.account(account)?;
        let mut clauses = self.attributes(attributes)?;
        let member_of = member_of.iter().map(|r| self.account(r)).collect::<DbResult<Vec<_>>>()?;
        if !member_of.is_empty() {
            self.roles()?;
        }
        // Memberships as separate GRANTs: CockroachDB has no `IN ROLE`.
        let mut out = vec![if self.f.model == AccountModel::Role {
            if let Some(p) = password {
                clauses.push(format!("PASSWORD {}", self.password(p)));
            }
            format!("CREATE ROLE {name}{}", joined(true, &clauses))
        } else if role {
            self.roles()?;
            if password.is_some() || !clauses.is_empty() {
                return Err(DbError::invalid("A role has no password, limits or lock; set them on the users."));
            }
            format!("CREATE ROLE {name}")
        } else {
            let auth = password.map(|p| format!(" IDENTIFIED BY {}", self.password(p))).unwrap_or_default();
            format!("CREATE USER {name}{auth}{}", joined(false, &clauses))
        }];
        out.extend(member_of.iter().map(|r| format!("GRANT {r} TO {name}")));
        Ok(out)
    }

    /// Clauses for the attributes the form sets, in the order the engines accept.
    fn attributes(&self, a: &AccountAttributes) -> DbResult<Vec<String>> {
        let unsupported = |what: &str| Err(DbError::unsupported(format!("This server has no {what}.")));
        let mut out = Vec::new();
        for setting in &a.flags {
            if !self.f.flags.contains(&setting.flag) {
                return unsupported(setting.flag.keyword(true));
            }
            out.push(setting.flag.keyword(setting.on).to_owned());
        }
        if let Some(limit) = a.connection_limit {
            if !self.f.connection_limit {
                return unsupported("connection limit");
            }
            if limit < -1 {
                return Err(DbError::invalid("The connection limit must be -1 (none) or more."));
            }
            out.push(format!("CONNECTION LIMIT {limit}"));
        }
        if let Some(until) = a.valid_until.as_deref().map(str::trim) {
            if !self.f.valid_until {
                return unsupported("password expiry time");
            }
            check_timestamp(until)?;
            out.push(format!("VALID UNTIL {}", self.d.quote_literal(until)));
        }
        if let Some(limits) = a.limits {
            if !self.f.resource_limits {
                return unsupported("resource limits");
            }
            let values = [
                ("MAX_QUERIES_PER_HOUR", limits.max_queries_per_hour),
                ("MAX_UPDATES_PER_HOUR", limits.max_updates_per_hour),
                ("MAX_CONNECTIONS_PER_HOUR", limits.max_connections_per_hour),
                ("MAX_USER_CONNECTIONS", limits.max_user_connections),
            ];
            if values.iter().any(|(_, v)| *v < 0) {
                return Err(DbError::invalid("Limits must be 0 (unlimited) or more."));
            }
            let list = values.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(" ");
            out.push(format!("WITH {list}"));
        }
        if let Some(locked) = a.locked {
            if !self.f.lock {
                return unsupported("account lock");
            }
            out.push(if locked { "ACCOUNT LOCK" } else { "ACCOUNT UNLOCK" }.to_owned());
        }
        if a.expire_password {
            if !self.f.expire_password {
                return unsupported("password expiry");
            }
            out.push("PASSWORD EXPIRE".to_owned());
        }
        Ok(out)
    }

    /// The privilege list, checked against what the level allows, in its canonical spelling.
    fn privileges(&self, target: &GrantTarget, privileges: &[String]) -> DbResult<String> {
        let spec = self
            .f
            .level(target.level())
            .ok_or_else(|| DbError::unsupported("Privileges cannot be granted at this level on this server."))?;
        if privileges.is_empty() {
            return Err(DbError::invalid("Choose at least one privilege."));
        }
        let wanted: Vec<String> = privileges
            .iter()
            .map(|p| p.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_uppercase())
            .collect();
        if wanted.iter().any(|p| p == "ALL" || p == "ALL PRIVILEGES") {
            return Ok("ALL PRIVILEGES".to_owned());
        }
        if let Some(bad) = wanted.iter().find(|p| !spec.privileges.contains(&p.as_str())) {
            return Err(DbError::invalid(format!("{bad} cannot be granted at this level.")));
        }
        Ok(spec.privileges.iter().filter(|p| wanted.iter().any(|w| w == *p)).copied().collect::<Vec<_>>().join(", "))
    }

    fn target(&self, target: &GrantTarget) -> DbResult<String> {
        let q = |s: &str, what: &str| check_name(s, what, 128).map(|()| self.d.quote_ident(s));
        let roles = self.f.model == AccountModel::Role;
        Ok(match target {
            GrantTarget::Server => "*.*".to_owned(),
            GrantTarget::Database { database } if roles => format!("DATABASE {}", q(database, "The database")?),
            GrantTarget::Database { database } => format!("{}.*", q(database, "The database")?),
            GrantTarget::Schema { schema, .. } => format!("SCHEMA {}", q(schema, "The schema")?),
            GrantTarget::SchemaTables { schema, .. } => format!("ALL TABLES IN SCHEMA {}", q(schema, "The schema")?),
            GrantTarget::Table { schema, table, .. } if roles => {
                let schema = schema.as_deref().ok_or_else(|| DbError::invalid("Choose the table's schema."))?;
                format!("TABLE {}.{}", q(schema, "The schema")?, q(table, "The table")?)
            }
            GrantTarget::Table { database, table, .. } => {
                format!("{}.{}", q(database, "The database")?, q(table, "The table")?)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::dialect::{MysqlDialect, PostgresDialect};
    use crate::users::{FlagSetting, ResourceLimits, RoleFlag};

    fn role(name: &str) -> AccountRef {
        AccountRef { name: name.into(), host: None }
    }

    fn user(name: &str, host: &str) -> AccountRef {
        AccountRef { name: name.into(), host: Some(host.into()) }
    }

    /// The executed form, with the password passed through unchanged.
    fn pg(change: &UserChange) -> DbResult<Vec<String>> {
        statements(&PostgresDialect, &UserAdminFeatures::postgres(), change, &|p| p.to_owned())
    }

    fn my(change: &UserChange) -> DbResult<Vec<String>> {
        statements(&MysqlDialect, &UserAdminFeatures::mysql(true), change, &|p| p.to_owned())
    }

    fn flags(list: &[(RoleFlag, bool)]) -> AccountAttributes {
        AccountAttributes {
            flags: list.iter().map(|&(flag, on)| FlagSetting { flag, on }).collect(),
            ..AccountAttributes::default()
        }
    }

    fn create(account: AccountRef, password: Option<&str>, attributes: AccountAttributes) -> UserChange {
        UserChange::Create {
            account,
            role: false,
            password: password.map(Password::new),
            attributes,
            member_of: Vec::new(),
        }
    }

    #[test]
    fn create_role_on_postgres() {
        let change = UserChange::Create {
            account: role("app\"user"),
            role: false,
            password: Some(Password::new("it's\\secret")),
            attributes: AccountAttributes {
                connection_limit: Some(5),
                valid_until: Some("2030-01-01 00:00:00+00".into()),
                ..flags(&[(RoleFlag::Login, true), (RoleFlag::Createdb, true), (RoleFlag::Superuser, false)])
            },
            member_of: vec![role("readers"), role("Writers")],
        };
        assert_eq!(
            pg(&change).unwrap(),
            [
                r#"CREATE ROLE "app""user" WITH LOGIN CREATEDB NOSUPERUSER CONNECTION LIMIT 5 VALID UNTIL '2030-01-01 00:00:00+00' PASSWORD 'it''s\secret'"#,
                r#"GRANT "readers" TO "app""user""#,
                r#"GRANT "Writers" TO "app""user""#,
            ]
        );
        let shown = preview(&PostgresDialect, &UserAdminFeatures::postgres(), &change).unwrap();
        assert!(shown[0].contains("PASSWORD '***'") && !shown[0].contains("secret"));
        assert_eq!(pg(&create(role("r"), None, AccountAttributes::default())).unwrap(), [r#"CREATE ROLE "r""#]);
    }

    #[test]
    fn create_user_on_mysql() {
        let change = UserChange::Create {
            account: user("o'brien", "10.0.%"),
            role: false,
            password: Some(Password::new("a\\b'c")),
            attributes: AccountAttributes {
                limits: Some(ResourceLimits { max_user_connections: 3, ..ResourceLimits::default() }),
                locked: Some(true),
                expire_password: true,
                ..AccountAttributes::default()
            },
            member_of: vec![user("app`role", "%"), role("maria_role")],
        };
        assert_eq!(
            my(&change).unwrap(),
            [
                r"CREATE USER `o'brien`@`10.0.%` IDENTIFIED BY 'a\\b''c' WITH MAX_QUERIES_PER_HOUR 0 MAX_UPDATES_PER_HOUR 0 MAX_CONNECTIONS_PER_HOUR 0 MAX_USER_CONNECTIONS 3 ACCOUNT LOCK PASSWORD EXPIRE",
                "GRANT `app``role`@`%` TO `o'brien`@`10.0.%`",
                "GRANT `maria_role` TO `o'brien`@`10.0.%`",
            ]
        );
        let shown = preview(&MysqlDialect, &UserAdminFeatures::mysql(true), &change).unwrap();
        assert!(shown[0].contains("IDENTIFIED BY '***'") && !shown[0].contains("a\\\\b"));
        let role_change = UserChange::Create {
            account: user("r", "%"),
            role: true,
            password: None,
            attributes: AccountAttributes::default(),
            member_of: Vec::new(),
        };
        assert_eq!(my(&role_change).unwrap(), ["CREATE ROLE `r`@`%`"]);
        // MariaDB roles have an empty host.
        assert_eq!(
            my(&UserChange::Drop {
                account: user("r", ""),
                role: true,
                reassign_to: None,
                drop_owned: false,
                database: None
            })
            .unwrap(),
            ["DROP ROLE `r`"]
        );
    }

    #[test]
    fn unsupported_attributes_are_refused() {
        let pg_lock = AccountAttributes { locked: Some(true), ..AccountAttributes::default() };
        assert!(pg(&create(role("r"), None, pg_lock)).is_err());
        assert!(my(&create(user("u", "%"), None, flags(&[(RoleFlag::Superuser, true)]))).is_err());
        let cockroach = UserAdminFeatures::cockroach();
        let super_user = create(role("r"), None, flags(&[(RoleFlag::Superuser, true)]));
        assert!(statements(&PostgresDialect, &cockroach, &super_user, &|p| p.to_owned()).is_err());
        let rename = UserChange::Rename { account: role("a"), new_name: "b".into() };
        assert!(statements(&PostgresDialect, &cockroach, &rename, &|p| p.to_owned()).is_err());
        let no_roles = UserChange::GrantRole { account: user("u", "%"), role: user("r", "%"), admin_option: false };
        assert!(statements(&MysqlDialect, &UserAdminFeatures::mysql(false), &no_roles, &|p| p.to_owned()).is_err());
        let bad_until =
            AccountAttributes { valid_until: Some("2030'; DROP ROLE x; --".into()), ..AccountAttributes::default() };
        assert!(pg(&create(role("r"), None, bad_until)).is_err());
        assert!(pg(&create(role(""), None, AccountAttributes::default())).is_err());
        assert!(pg(&create(role("a\nb"), None, AccountAttributes::default())).is_err());
        let no_limit = AccountAttributes { connection_limit: Some(-2), ..AccountAttributes::default() };
        assert!(pg(&create(role("r"), None, no_limit)).is_err());
        let role_with_password = UserChange::Create {
            account: user("r", "%"),
            role: true,
            password: Some(Password::new("x")),
            attributes: AccountAttributes::default(),
            member_of: Vec::new(),
        };
        assert!(my(&role_with_password).is_err());
    }

    #[test]
    fn alter_attributes() {
        let attrs = AccountAttributes {
            connection_limit: Some(-1),
            valid_until: Some("infinity".into()),
            ..flags(&[(RoleFlag::Login, false), (RoleFlag::Bypassrls, true)])
        };
        assert_eq!(
            pg(&UserChange::Alter { account: role("app"), attributes: attrs }).unwrap(),
            [r#"ALTER ROLE "app" WITH NOLOGIN BYPASSRLS CONNECTION LIMIT -1 VALID UNTIL 'infinity'"#]
        );
        let unlock = AccountAttributes { locked: Some(false), ..AccountAttributes::default() };
        assert_eq!(
            my(&UserChange::Alter { account: user("u", "localhost"), attributes: unlock }).unwrap(),
            ["ALTER USER `u`@`localhost` ACCOUNT UNLOCK"]
        );
        assert!(pg(&UserChange::Alter { account: role("app"), attributes: AccountAttributes::default() }).is_err());
    }

    #[test]
    fn passwords() {
        let set = |p: Option<&str>| UserChange::SetPassword { account: role("app"), password: p.map(Password::new) };
        assert_eq!(pg(&set(Some("p'w"))).unwrap(), [r#"ALTER ROLE "app" PASSWORD 'p''w'"#]);
        assert_eq!(pg(&set(None)).unwrap(), [r#"ALTER ROLE "app" PASSWORD NULL"#]);
        let secret = statements(&PostgresDialect, &UserAdminFeatures::postgres(), &set(Some("x")), &|_| {
            "SCRAM-SHA-256$v".into()
        });
        assert_eq!(secret.unwrap(), [r#"ALTER ROLE "app" PASSWORD 'SCRAM-SHA-256$v'"#]);
        let my_set = UserChange::SetPassword { account: user("u", "%"), password: Some(Password::new(r"\'")) };
        assert_eq!(my(&my_set).unwrap(), [r"ALTER USER `u`@`%` IDENTIFIED BY '\\'''"]);
        assert_eq!(
            preview(&MysqlDialect, &UserAdminFeatures::mysql(true), &my_set).unwrap(),
            ["ALTER USER `u`@`%` IDENTIFIED BY '***'"]
        );
        assert!(my(&UserChange::SetPassword { account: user("u", "%"), password: None }).is_err());
        assert_eq!(format!("{:?}", Password::new("hunter2")), "***");
    }

    #[test]
    fn rename_and_drop() {
        assert_eq!(
            pg(&UserChange::Rename { account: role("old"), new_name: "New".into() }).unwrap(),
            [r#"ALTER ROLE "old" RENAME TO "New""#]
        );
        assert_eq!(
            my(&UserChange::Rename { account: user("old", "%"), new_name: "new".into() }).unwrap(),
            ["RENAME USER `old`@`%` TO `new`@`%`"]
        );
        let drop = UserChange::Drop {
            account: role("app"),
            role: false,
            reassign_to: Some("postgres".into()),
            drop_owned: true,
            database: Some("shop".into()),
        };
        assert_eq!(
            pg(&drop).unwrap(),
            [r#"REASSIGN OWNED BY "app" TO "postgres""#, r#"DROP OWNED BY "app""#, r#"DROP ROLE "app""#]
        );
        assert_eq!(drop.database(), "shop");
        assert_eq!(drop.risks().len(), 2);
        assert_eq!(
            my(&UserChange::Drop {
                account: user("u", "%"),
                role: false,
                reassign_to: None,
                drop_owned: false,
                database: None
            })
            .unwrap(),
            ["DROP USER `u`@`%`"]
        );
        assert!(
            my(&UserChange::Drop {
                account: user("u", "%"),
                role: false,
                reassign_to: None,
                drop_owned: true,
                database: None
            })
            .is_err()
        );
    }

    #[test]
    fn grants_and_revokes() {
        let table =
            GrantTarget::Table { database: "app".into(), schema: Some("pub lic".into()), table: "Orders".into() };
        let grant = UserChange::Grant {
            account: role("app"),
            target: table.clone(),
            privileges: vec!["insert".into(), "SELECT".into(), "trigger".into()],
            grant_option: true,
        };
        assert_eq!(
            pg(&grant).unwrap(),
            [r#"GRANT SELECT, INSERT, TRIGGER ON TABLE "pub lic"."Orders" TO "app" WITH GRANT OPTION"#]
        );
        assert_eq!(grant.database(), "app");
        let revoke = |target: GrantTarget, privileges: &[&str], only: bool| UserChange::Revoke {
            account: role("app"),
            target,
            privileges: privileges.iter().map(|p| (*p).to_owned()).collect(),
            grant_option_only: only,
        };
        assert_eq!(
            pg(&revoke(table.clone(), &["ALL"], false)).unwrap(),
            [r#"REVOKE ALL PRIVILEGES ON TABLE "pub lic"."Orders" FROM "app""#]
        );
        assert_eq!(
            pg(&revoke(table, &["UPDATE"], true)).unwrap(),
            [r#"REVOKE GRANT OPTION FOR UPDATE ON TABLE "pub lic"."Orders" FROM "app""#]
        );
        let schema = GrantTarget::SchemaTables { database: "app".into(), schema: "public".into() };
        assert_eq!(
            pg(&UserChange::Grant {
                account: role("r"),
                target: schema,
                privileges: vec!["SELECT".into()],
                grant_option: false
            })
            .unwrap(),
            [r#"GRANT SELECT ON ALL TABLES IN SCHEMA "public" TO "r""#]
        );
        let db = GrantTarget::Database { database: "my\"db".into() };
        assert_eq!(
            pg(&UserChange::Grant {
                account: role("r"),
                target: db.clone(),
                privileges: vec!["connect".into(), "temporary".into()],
                grant_option: false
            })
            .unwrap(),
            [r#"GRANT CONNECT, TEMPORARY ON DATABASE "my""db" TO "r""#]
        );
        // Table privileges are not database privileges, and nothing unknown gets through.
        assert!(
            pg(&UserChange::Grant {
                account: role("r"),
                target: db,
                privileges: vec!["SELECT".into()],
                grant_option: false
            })
            .is_err()
        );
        let injected = GrantTarget::Schema { database: "app".into(), schema: "public".into() };
        assert!(
            pg(&UserChange::Grant {
                account: role("r"),
                target: injected,
                privileges: vec!["USAGE; DROP TABLE x".into()],
                grant_option: false
            })
            .is_err()
        );
        assert!(
            pg(&UserChange::Grant {
                account: role("r"),
                target: GrantTarget::Server,
                privileges: vec!["SELECT".into()],
                grant_option: false
            })
            .is_err()
        );

        let my_table = GrantTarget::Table { database: "shop".into(), schema: None, table: "or`ders".into() };
        let my_grant = |target: GrantTarget, privileges: &[&str]| UserChange::Grant {
            account: user("u", "%"),
            target,
            privileges: privileges.iter().map(|p| (*p).to_owned()).collect(),
            grant_option: false,
        };
        assert_eq!(
            my(&my_grant(my_table.clone(), &["select", "show  view"])).unwrap(),
            ["GRANT SELECT, SHOW VIEW ON `shop`.`or``ders` TO `u`@`%`"]
        );
        assert_eq!(
            my(&my_grant(GrantTarget::Database { database: "shop".into() }, &["ALL PRIVILEGES"])).unwrap(),
            ["GRANT ALL PRIVILEGES ON `shop`.* TO `u`@`%`"]
        );
        assert_eq!(my(&my_grant(GrantTarget::Server, &["PROCESS"])).unwrap(), ["GRANT PROCESS ON *.* TO `u`@`%`"]);
        assert!(my(&my_grant(my_table.clone(), &["PROCESS"])).is_err());
        assert!(my(&my_grant(my_table.clone(), &["TRUNCATE"])).is_err());
        let my_revoke = |only: bool| UserChange::Revoke {
            account: user("u", "%"),
            target: my_table.clone(),
            privileges: vec!["SELECT".into()],
            grant_option_only: only,
        };
        assert_eq!(my(&my_revoke(false)).unwrap(), ["REVOKE SELECT ON `shop`.`or``ders` FROM `u`@`%`"]);
        assert_eq!(my(&my_revoke(true)).unwrap(), ["REVOKE GRANT OPTION ON `shop`.`or``ders` FROM `u`@`%`"]);
        assert_eq!(my_revoke(true).risks().len(), 1);
    }

    #[test]
    fn role_membership() {
        assert_eq!(
            pg(&UserChange::GrantRole { account: role("app"), role: role("read\"ers"), admin_option: true }).unwrap(),
            [r#"GRANT "read""ers" TO "app" WITH ADMIN OPTION"#]
        );
        assert_eq!(
            pg(&UserChange::RevokeRole { account: role("app"), role: role("readers") }).unwrap(),
            [r#"REVOKE "readers" FROM "app""#]
        );
        assert_eq!(
            my(&UserChange::GrantRole { account: user("u", "localhost"), role: user("r", "%"), admin_option: false })
                .unwrap(),
            ["GRANT `r`@`%` TO `u`@`localhost`"]
        );
        assert_eq!(
            my(&UserChange::RevokeRole { account: user("u", "%"), role: role("r") }).unwrap(),
            ["REVOKE `r` FROM `u`@`%`"]
        );
    }
}
