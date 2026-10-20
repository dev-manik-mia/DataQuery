//! User and role management against real servers: PostgreSQL
//! (`DATAQUERY_TEST_PG=localhost:55432`) and `CockroachDB`
//! (`DATAQUERY_TEST_COCKROACH=localhost:56257`). Accounts are named `ur_…`
//! with the process id, and every test removes what it created.

use std::sync::Arc;

use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::engine::Engine;
use dq_core::result::ExecOptions;
use dq_core::sql::users;
use dq_core::users::{
    Account, AccountAttributes, AccountRef, FlagSetting, GrantLevel, GrantTarget, Password, RoleFlag, UserChange,
};
use dq_core::{DbResult, ErrorKind};
use dq_postgres::PostgresAdapter;

const DB: &str = "dataquery_test";
const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };

fn config(var: &str, engine: Engine) -> Option<ConnConfig> {
    let target = std::env::var(var).ok()?;
    let (host, port) = target.split_once(':')?;
    Some(ConnConfig {
        engine,
        host: host.into(),
        port: port.parse().ok()?,
        database: DB.into(),
        username: "dataquery".into(),
        ssl_mode: SslMode::Disable,
        connect_timeout_secs: 5,
        ..ConnConfig::default()
    })
}

async fn connect(adapter: PostgresAdapter, cfg: &ConnConfig, password: &str) -> DbResult<Arc<dyn Session>> {
    adapter.connect(&ConnectParams::new(cfg.clone(), Some(password.to_owned()))).await
}

/// What the `apply_user_change` command does: build with the adapter's secret, then apply.
async fn run(s: &dyn Session, change: UserChange) -> DbResult<Vec<String>> {
    let admin = s.users().expect("user management");
    let secret = change.password().map(|p| admin.password_secret(p.expose())).unwrap_or_default();
    let statements = users::statements(s.dialect(), &admin.features(), &change, &|_| secret.clone())?;
    admin.apply(change.database(), &statements).await?;
    Ok(statements)
}

fn role(name: &str) -> AccountRef {
    AccountRef { name: name.into(), host: None }
}

fn flag(flag: RoleFlag, on: bool) -> FlagSetting {
    FlagSetting { flag, on }
}

async fn account(s: &dyn Session, name: &str) -> Option<Account> {
    s.users().unwrap().accounts().await.unwrap().accounts.into_iter().find(|a| a.name == name)
}

fn table_target(table: &str) -> GrantTarget {
    GrantTarget::Table { database: DB.into(), schema: Some("public".into()), table: table.into() }
}

/// Removes leftovers of an earlier, interrupted run.
async fn clean(s: &dyn Session, table: &str, roles: &[&str]) {
    let _ = s.execute(&format!("DROP TABLE IF EXISTS public.{table}"), &[], OPTS).await;
    for r in roles {
        let q = format!("\"{}\"", r.replace('"', "\"\""));
        let _ = s.execute(&format!("DROP OWNED BY {q}"), &[], OPTS).await;
        let _ = s.execute(&format!("DROP ROLE IF EXISTS {q}"), &[], OPTS).await;
    }
}

#[tokio::test]
async fn postgres_roles_round_trip() {
    let Some(cfg) = config("DATAQUERY_TEST_PG", Engine::Postgres) else {
        eprintln!("skipped: set DATAQUERY_TEST_PG to run PostgreSQL integration tests");
        return;
    };
    let s = connect(PostgresAdapter::POSTGRES, &cfg, "dataquery").await.expect("connect");
    let pid = std::process::id();
    let (group, user, renamed, table) =
        (format!("ur_Read\"ers_{pid}"), format!("ur_app_{pid}"), format!("ur_App 2_{pid}"), format!("ur_t_{pid}"));
    clean(&*s, &table, &[&user, &renamed, &group]).await;
    s.execute(&format!("CREATE TABLE public.{table} (id int)"), &[], OPTS).await.unwrap();
    let password = r#"it's "q\uoted" 🔑"#;

    run(
        &*s,
        UserChange::Create {
            account: role(&group),
            role: true,
            password: None,
            attributes: AccountAttributes::default(),
            member_of: Vec::new(),
        },
    )
    .await
    .unwrap();
    let created = run(
        &*s,
        UserChange::Create {
            account: role(&user),
            role: false,
            password: Some(Password::new(password)),
            attributes: AccountAttributes {
                flags: vec![flag(RoleFlag::Login, true), flag(RoleFlag::Createdb, false)],
                connection_limit: Some(3),
                valid_until: Some("2099-01-01".into()),
                ..AccountAttributes::default()
            },
            member_of: vec![role(&group)],
        },
    )
    .await
    .unwrap();
    assert!(created[0].contains("PASSWORD 'SCRAM-SHA-256$"), "a verifier, never the password");
    assert!(!created[0].contains("q\\uoted"));

    let a = account(&*s, &user).await.expect("listed");
    assert!(!a.role && a.flags.contains(&RoleFlag::Login) && !a.flags.contains(&RoleFlag::Createdb));
    assert_eq!(a.connection_limit, Some(3));
    assert!(a.valid_until.as_deref().unwrap_or_default().starts_with("2099-01-01"));
    assert_eq!(a.member_of, [role(&group)]);
    assert!(account(&*s, &group).await.unwrap().role);
    assert!(account(&*s, "dataquery").await.unwrap().current);

    // The password works, and only that one.
    let login = ConnConfig { username: user.clone(), ..cfg.clone() };
    connect(PostgresAdapter::POSTGRES, &login, password).await.expect("log in as the new user").close().await;
    let wrong = connect(PostgresAdapter::POSTGRES, &login, "nope").await.err().expect("wrong password");
    assert_eq!(wrong.kind, ErrorKind::Auth);

    // Grants on the database, the schema and a table.
    let grant = |target: GrantTarget, privileges: &[&str], grant_option: bool| UserChange::Grant {
        account: role(&user),
        target,
        privileges: privileges.iter().map(|p| (*p).to_owned()).collect(),
        grant_option,
    };
    run(&*s, grant(GrantTarget::Database { database: DB.into() }, &["CONNECT", "TEMPORARY"], false)).await.unwrap();
    run(&*s, grant(GrantTarget::Schema { database: DB.into(), schema: "public".into() }, &["USAGE"], false))
        .await
        .unwrap();
    run(&*s, grant(table_target(&table), &["SELECT", "UPDATE"], true)).await.unwrap();
    run(
        &*s,
        UserChange::Revoke {
            account: role(&user),
            target: table_target(&table),
            privileges: vec!["UPDATE".into()],
            grant_option_only: true,
        },
    )
    .await
    .unwrap();
    let p = s.users().unwrap().privileges(&role(&user), DB).await.unwrap();
    assert!(p.unavailable.is_empty(), "{:?}", p.unavailable);
    let on_table = p.grants.iter().find(|g| g.object.as_deref() == Some(table.as_str())).expect("table grant");
    assert_eq!(on_table.level, GrantLevel::Table);
    let held: Vec<_> = on_table.privileges.iter().map(|p| (p.privilege.as_str(), p.grantable)).collect();
    assert_eq!(held, [("SELECT", true), ("UPDATE", false)]);
    assert!(p.grants.iter().any(|g| g.level == GrantLevel::Database
        && g.database.as_deref() == Some(DB)
        && g.privileges.iter().any(|p| p.privilege == "CONNECT")));
    assert!(p.grants.iter().any(|g| g.level == GrantLevel::Schema && g.schema.as_deref() == Some("public")));

    // Attributes, password, rename.
    run(
        &*s,
        UserChange::Alter {
            account: role(&user),
            attributes: AccountAttributes {
                flags: vec![flag(RoleFlag::Createdb, true)],
                connection_limit: Some(-1),
                ..AccountAttributes::default()
            },
        },
    )
    .await
    .unwrap();
    let a = account(&*s, &user).await.unwrap();
    assert!(a.flags.contains(&RoleFlag::Createdb));
    assert_eq!(a.connection_limit, None);
    run(&*s, UserChange::SetPassword { account: role(&user), password: Some(Password::new("n3w'pw")) }).await.unwrap();
    connect(PostgresAdapter::POSTGRES, &login, "n3w'pw").await.expect("new password").close().await;
    run(&*s, UserChange::Rename { account: role(&user), new_name: renamed.clone() }).await.unwrap();
    assert!(account(&*s, &user).await.is_none());
    run(&*s, UserChange::RevokeRole { account: role(&renamed), role: role(&group) }).await.unwrap();
    assert_eq!(account(&*s, &renamed).await.unwrap().member_of.len(), 0);

    // Owned objects block DROP ROLE until they are reassigned.
    s.execute(&format!("ALTER TABLE public.{table} OWNER TO \"{renamed}\""), &[], OPTS).await.unwrap();
    let p = s.users().unwrap().privileges(&role(&renamed), DB).await.unwrap();
    assert!(p.owned.iter().any(|o| o.database.as_deref() == Some(DB) && o.owned >= 1), "{:?}", p.owned);
    let plain =
        UserChange::Drop { account: role(&renamed), role: false, reassign_to: None, drop_owned: false, database: None };
    assert!(run(&*s, plain).await.is_err(), "owned objects block the drop");
    run(
        &*s,
        UserChange::Drop {
            account: role(&renamed),
            role: false,
            reassign_to: Some("dataquery".into()),
            drop_owned: true,
            database: Some(DB.into()),
        },
    )
    .await
    .unwrap();
    assert!(account(&*s, &renamed).await.is_none());
    let owner =
        s.execute(&format!("SELECT tableowner FROM pg_tables WHERE tablename = '{table}'"), &[], OPTS).await.unwrap();
    assert_eq!(owner.rows[0][0].to_text().as_deref(), Some("dataquery"), "reassigned, not dropped");
    run(
        &*s,
        UserChange::Drop { account: role(&group), role: true, reassign_to: None, drop_owned: false, database: None },
    )
    .await
    .unwrap();
    clean(&*s, &table, &[]).await;
    s.close().await;
}

#[tokio::test]
async fn cockroach_roles_round_trip() {
    let Some(cfg) = config("DATAQUERY_TEST_COCKROACH", Engine::Cockroachdb) else {
        eprintln!("skipped: set DATAQUERY_TEST_COCKROACH to run CockroachDB integration tests");
        return;
    };
    let s = connect(PostgresAdapter::COCKROACH, &cfg, "dataquery").await.expect("connect");
    let features = s.users().unwrap().features();
    assert!(!features.rename && !features.flags.contains(&RoleFlag::Superuser));
    let pid = std::process::id();
    let (group, user, table) = (format!("ur_crdb_group_{pid}"), format!("ur_crdb_user_{pid}"), format!("ur_ct_{pid}"));
    clean(&*s, &table, &[&user, &group]).await;
    s.execute(&format!("CREATE TABLE public.{table} (id int PRIMARY KEY)"), &[], OPTS).await.unwrap();
    let create = |name: &str, login: bool, member_of: Vec<AccountRef>| UserChange::Create {
        account: role(name),
        role: !login,
        // The test node runs `--insecure`, which refuses passwords.
        password: None,
        attributes: AccountAttributes {
            flags: vec![flag(RoleFlag::Login, login), flag(RoleFlag::Createdb, true)],
            ..AccountAttributes::default()
        },
        member_of,
    };
    run(&*s, create(&group, false, Vec::new())).await.unwrap();
    run(&*s, create(&user, true, vec![role(&group)])).await.unwrap();
    let a = account(&*s, &user).await.expect("listed");
    assert!(!a.role && a.flags.contains(&RoleFlag::Createdb));
    assert_eq!(a.member_of, [role(&group)]);
    run(
        &*s,
        UserChange::Grant {
            account: role(&user),
            target: table_target(&table),
            privileges: vec!["SELECT".into(), "INSERT".into()],
            grant_option: false,
        },
    )
    .await
    .unwrap();
    let p = s.users().unwrap().privileges(&role(&user), DB).await.unwrap();
    let on_table = p.grants.iter().find(|g| g.object.as_deref() == Some(table.as_str())).expect("table grant");
    assert_eq!(on_table.privileges.len(), 2, "{:?}", on_table.privileges);
    run(
        &*s,
        UserChange::Drop {
            account: role(&user),
            role: false,
            reassign_to: Some("dataquery".into()),
            drop_owned: true,
            database: Some(DB.into()),
        },
    )
    .await
    .unwrap();
    run(
        &*s,
        UserChange::Drop { account: role(&group), role: true, reassign_to: None, drop_owned: false, database: None },
    )
    .await
    .unwrap();
    assert!(account(&*s, &user).await.is_none());
    clean(&*s, &table, &[]).await;
    s.close().await;
}
