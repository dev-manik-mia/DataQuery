//! Roles from `pg_roles` and `pg_auth_members`; privileges from the ACLs of
//! databases, schemas, relations and default privileges (`aclexplode`), so
//! they are visible to any account, not only to the grantor or grantee as in
//! `information_schema`. Ownership comes from `pg_shdepend`, across databases.
//!
//! `CockroachDB` keeps no ACL columns; its grants come from `SHOW GRANTS FOR`.
//! Passwords are sent as SCRAM-SHA-256 verifiers computed here.

use async_trait::async_trait;
use dq_core::adapter::Session;
use dq_core::edit::BoundStatement;
use dq_core::sql::{Dialect, PostgresDialect};
use dq_core::users::{
    Account, AccountList, AccountPrivileges, AccountRef, DefaultPrivilege, GrantLevel, GrantedPrivilege, OwnedObjects,
    PrivilegeRow, RoleFlag, UserAdmin, UserAdminFeatures, group_grants,
};
use dq_core::{DbError, DbResult};
use tokio_postgres::{Client, Row};

use crate::connect::Flavor;
use crate::error::map_error;
use crate::session::PgSession;

fn pg(e: tokio_postgres::Error) -> DbError {
    map_error(&e, None)
}

/// Role attributes in `pg_roles` column order (columns 1–7 of the account query).
const FLAG_COLUMNS: [RoleFlag; 7] = [
    RoleFlag::Login,
    RoleFlag::Superuser,
    RoleFlag::Createdb,
    RoleFlag::Createrole,
    RoleFlag::Inherit,
    RoleFlag::Replication,
    RoleFlag::Bypassrls,
];

fn account(r: &Row, flags: &[RoleFlag], cockroach: bool) -> Account {
    let name: String = r.get(0);
    let limit: i64 = r.get(8);
    Account {
        role: !r.get::<_, bool>(1),
        flags: FLAG_COLUMNS
            .iter()
            .enumerate()
            .filter(|(i, flag)| flags.contains(*flag) && r.get::<_, Option<bool>>(i + 1).unwrap_or(false))
            .map(|(_, flag)| *flag)
            .collect(),
        connection_limit: (limit >= 0).then_some(limit),
        valid_until: r.get(9),
        member_of: r.get::<_, Vec<String>>(10).into_iter().map(|name| AccountRef { name, host: None }).collect(),
        locked: None,
        password_expired: None,
        auth_plugin: None,
        limits: None,
        system: name.starts_with("pg_") || (cockroach && name == "node"),
        current: r.get(11),
        host: None,
        name,
    }
}

const ROLE_OID: &str = "(SELECT oid FROM pg_roles WHERE rolname = $1)";

fn privilege_row(r: &Row, level: GrantLevel) -> PrivilegeRow {
    PrivilegeRow {
        level,
        database: r.get(1),
        schema: r.get(2),
        object: r.get(3),
        object_kind: r.get(4),
        privilege: r.get(5),
        grantable: r.get(6),
    }
}

/// Database, schema and relation grants from their ACLs.
async fn grants(client: &Client, role: &str) -> DbResult<Vec<PrivilegeRow>> {
    let sql = format!(
        "SELECT 0, d.datname::text, NULL::text, NULL::text, NULL::text, a.privilege_type, a.is_grantable \
         FROM pg_database d, aclexplode(d.datacl) a WHERE a.grantee = {ROLE_OID} \
         UNION ALL \
         SELECT 1, current_database()::text, n.nspname::text, NULL, NULL, a.privilege_type, a.is_grantable \
         FROM pg_namespace n, aclexplode(n.nspacl) a WHERE a.grantee = {ROLE_OID} \
         UNION ALL \
         SELECT 2, current_database()::text, n.nspname::text, c.relname::text, \
                CASE c.relkind WHEN 'S' THEN 'sequence' WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' \
                               WHEN 'f' THEN 'foreign table' ELSE 'table' END, \
                a.privilege_type, a.is_grantable \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace, aclexplode(c.relacl) a \
         WHERE a.grantee = {ROLE_OID} AND c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S') \
         ORDER BY 1, 2, 3, 4 LIMIT 5000"
    );
    let rows = client.query(&sql, &[&role]).await.map_err(pg)?;
    Ok(rows
        .iter()
        .map(|r| {
            let level = match r.get::<_, i32>(0) {
                0 => GrantLevel::Database,
                1 => GrantLevel::Schema,
                _ => GrantLevel::Table,
            };
            privilege_row(r, level)
        })
        .collect())
}

async fn default_privileges(client: &Client, role: &str) -> DbResult<Vec<DefaultPrivilege>> {
    let sql = format!(
        "SELECT pg_get_userbyid(d.defaclrole)::text, n.nspname::text, d.defaclobjtype::text, \
                a.privilege_type, a.is_grantable \
         FROM pg_default_acl d LEFT JOIN pg_namespace n ON n.oid = d.defaclnamespace, aclexplode(d.defaclacl) a \
         WHERE a.grantee = {ROLE_OID} ORDER BY 1, 2, 3"
    );
    let rows = client.query(&sql, &[&role]).await.map_err(pg)?;
    let mut out: Vec<DefaultPrivilege> = Vec::new();
    for r in &rows {
        let (owner, schema): (String, Option<String>) = (r.get(0), r.get(1));
        let object_type = match r.get::<_, String>(2).as_str() {
            "r" => "tables",
            "S" => "sequences",
            "f" => "functions",
            "T" => "types",
            "n" => "schemas",
            _ => "objects",
        }
        .to_owned();
        let privilege = GrantedPrivilege { privilege: r.get(3), grantable: r.get(4) };
        match out.iter_mut().find(|d| d.owner == owner && d.schema == schema && d.object_type == object_type) {
            Some(d) => d.privileges.push(privilege),
            None => out.push(DefaultPrivilege { owner, schema, object_type, privileges: vec![privilege] }),
        }
    }
    Ok(out)
}

/// Objects owned and privileges held per database, from `pg_shdepend`.
async fn owned(client: &Client, role: &str) -> DbResult<Vec<OwnedObjects>> {
    let sql = format!(
        "SELECT d.datname::text, count(*) FILTER (WHERE s.deptype = 'o'), count(*) FILTER (WHERE s.deptype = 'a') \
         FROM pg_shdepend s LEFT JOIN pg_database d ON d.oid = s.dbid \
         WHERE s.refclassid = 'pg_authid'::regclass AND s.refobjid = {ROLE_OID} AND s.deptype IN ('o', 'a') \
         GROUP BY 1 ORDER BY 1 NULLS FIRST"
    );
    let rows = client.query(&sql, &[&role]).await.map_err(pg)?;
    Ok(rows.iter().map(|r| OwnedObjects { database: r.get(0), owned: r.get(1), privileges: r.get(2) }).collect())
}

/// `CockroachDB`: grants in the connection's database from `SHOW GRANTS FOR`.
async fn cockroach_grants(client: &Client, role: &str) -> DbResult<Vec<PrivilegeRow>> {
    let sql = format!(
        "SELECT object_type, database_name, schema_name, object_name, object_type, privilege_type, is_grantable \
         FROM [SHOW GRANTS FOR {}] WHERE grantee = $1 ORDER BY 2, 3, 4",
        PostgresDialect.quote_ident(role)
    );
    let rows = client.query(&sql, &[&role]).await.map_err(pg)?;
    Ok(rows
        .iter()
        .map(|r| {
            let level = match r.get::<_, Option<String>>(0).as_deref() {
                Some("database") => GrantLevel::Database,
                Some("schema") => GrantLevel::Schema,
                _ => GrantLevel::Table,
            };
            privilege_row(r, level)
        })
        .collect())
}

/// Records what could not be read instead of failing the whole page.
fn partial<T: Default>(result: DbResult<T>, what: &str, unavailable: &mut Vec<String>) -> T {
    result.unwrap_or_else(|e| {
        unavailable.push(format!("{what}: {}", e.message));
        T::default()
    })
}

#[async_trait]
impl UserAdmin for PgSession {
    fn features(&self) -> UserAdminFeatures {
        match self.server.flavor {
            Flavor::Cockroach => UserAdminFeatures::cockroach(),
            // Redshift has no user admin (`PgSession::users`).
            Flavor::Postgres | Flavor::Greenplum | Flavor::Redshift => {
                let mut features = UserAdminFeatures::postgres();
                // BYPASSRLS came with PostgreSQL 9.5.
                if self.server.legacy {
                    features.flags.retain(|f| *f != RoleFlag::Bypassrls);
                }
                features
            }
        }
    }

    async fn accounts(&self) -> DbResult<AccountList> {
        let client = self.catalog_client("").await?;
        let bypassrls = if self.server.legacy { "false" } else { "r.rolbypassrls" };
        let rows = client
            .query(
                &format!(
                    "SELECT r.rolname::text, r.rolcanlogin, r.rolsuper, r.rolcreatedb, r.rolcreaterole, r.rolinherit, \
                            r.rolreplication, {bypassrls}, r.rolconnlimit::int8, r.rolvaliduntil::text, \
                            ARRAY(SELECT b.rolname::text FROM pg_auth_members m JOIN pg_roles b ON b.oid = m.roleid \
                                  WHERE m.member = r.oid ORDER BY 1), \
                            r.rolname = current_user \
                     FROM pg_roles r ORDER BY r.rolname LIKE 'pg\\_%', r.rolname"
                ),
                &[],
            )
            .await
            .map_err(pg)?;
        let flags = self.features().flags;
        let cockroach = self.server.flavor == Flavor::Cockroach;
        Ok(AccountList {
            accounts: rows.iter().map(|r| account(r, &flags, cockroach)).collect(),
            unavailable: Vec::new(),
        })
    }

    async fn privileges(&self, account: &AccountRef, database: &str) -> DbResult<AccountPrivileges> {
        let client = self.catalog_client(database).await?;
        let role = account.name.as_str();
        let mut unavailable = Vec::new();
        let (rows, default_privileges) = if self.server.flavor == Flavor::Cockroach {
            (partial(cockroach_grants(&client, role).await, "Grants", &mut unavailable), Vec::new())
        } else {
            (
                partial(grants(&client, role).await, "Grants", &mut unavailable),
                partial(default_privileges(&client, role).await, "Default privileges", &mut unavailable),
            )
        };
        let owned = partial(owned(&client, role).await, "Owned objects", &mut unavailable);
        Ok(AccountPrivileges {
            grants: group_grants(rows),
            default_privileges,
            owned,
            statements: Vec::new(),
            unavailable,
        })
    }

    /// Servers built on PostgreSQL 9.x (Greenplum 6) know no SCRAM and hash
    /// the clear password themselves (md5).
    fn password_secret(&self, password: &str) -> String {
        if self.server.legacy {
            return password.to_owned();
        }
        postgres_protocol::password::scram_sha_256(password.as_bytes())
    }

    /// One transaction: role statements are transactional in PostgreSQL.
    async fn apply(&self, database: &str, statements: &[String]) -> DbResult<()> {
        let mut bound =
            statements.iter().map(|s| Ok(BoundStatement { sql: s.clone(), params: Vec::new(), expect_rows: None }));
        self.run_in_transaction(database, &mut bound).await.map(drop)
    }
}
