//! User and role management against a real MySQL 8 or MariaDB server, as
//! root: `DATAQUERY_TEST_MYSQL=localhost:53306` (MySQL 8.4) or
//! `localhost:53307` (MariaDB 11.4). Accounts are named `ur_…` with the
//! process id, and the test removes what it created.

use std::sync::Arc;

use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::engine::Engine;
use dq_core::result::ExecOptions;
use dq_core::sql::users;
use dq_core::users::{
    Account, AccountAttributes, AccountRef, GrantLevel, GrantTarget, Password, ResourceLimits, UserChange,
};
use dq_core::{DbResult, ErrorKind};
use dq_mysql::MysqlAdapter;

const DB: &str = "dataquery_test";
const OPTS: ExecOptions = ExecOptions { row_limit: 500, timeout_secs: 30 };

fn config() -> Option<ConnConfig> {
    let target = std::env::var("DATAQUERY_TEST_MYSQL").ok()?;
    let (host, port) = target.split_once(':')?;
    Some(ConnConfig {
        engine: Engine::Mysql,
        host: host.into(),
        port: port.parse().ok()?,
        database: DB.into(),
        username: "root".into(),
        ssl_mode: SslMode::Disable,
        connect_timeout_secs: 5,
        ..ConnConfig::default()
    })
}

async fn connect(cfg: &ConnConfig, password: &str) -> DbResult<Arc<dyn Session>> {
    MysqlAdapter::MYSQL.connect(&ConnectParams::new(cfg.clone(), Some(password.to_owned()))).await
}

/// What the `apply_user_change` command does: build with the adapter's secret, then apply.
async fn run(s: &dyn Session, change: UserChange) -> DbResult<Vec<String>> {
    let admin = s.users().expect("user management");
    let secret = change.password().map(|p| admin.password_secret(p.expose())).unwrap_or_default();
    let statements = users::statements(s.dialect(), &admin.features(), &change, &|_| secret.clone())?;
    admin.apply(change.database(), &statements).await?;
    Ok(statements)
}

async fn account(s: &dyn Session, name: &str) -> Option<Account> {
    s.users().unwrap().accounts().await.unwrap().accounts.into_iter().find(|a| a.name == name)
}

#[tokio::test]
async fn accounts_round_trip() {
    let Some(cfg) = config() else {
        eprintln!("skipped: set DATAQUERY_TEST_MYSQL to run MySQL integration tests");
        return;
    };
    let s = connect(&cfg, "dataquery").await.expect("connect as root");
    let features = s.users().unwrap().features();
    assert!(features.roles, "MySQL 8 and MariaDB have roles");
    let pid = std::process::id();
    let (group, name, renamed) = (format!("ur_role_{pid}"), format!("ur_o'b`{pid}"), format!("ur_new_{pid}"));
    for (user, host) in [(&name, "%"), (&renamed, "%")] {
        let _ = s.execute(&format!("DROP USER IF EXISTS '{}'@'{host}'", user.replace('\'', "''")), &[], OPTS).await;
    }
    let _ = s.execute(&format!("DROP ROLE IF EXISTS {group}"), &[], OPTS).await;
    let user = AccountRef { name: name.clone(), host: Some("%".into()) };
    let password = r#"p'a\ss"w0rd"#;

    // Roles are created without a host: MySQL adds `%`, MariaDB has none.
    run(
        &*s,
        UserChange::Create {
            account: AccountRef { name: group.clone(), host: None },
            role: true,
            password: None,
            attributes: AccountAttributes::default(),
            member_of: Vec::new(),
        },
    )
    .await
    .unwrap();
    let group_ref = account(&*s, &group).await.expect("role listed").reference();
    assert!(account(&*s, &group).await.unwrap().role);
    let created = run(
        &*s,
        UserChange::Create {
            account: user.clone(),
            role: false,
            password: Some(Password::new(password)),
            attributes: AccountAttributes {
                limits: Some(ResourceLimits { max_user_connections: 3, ..ResourceLimits::default() }),
                ..AccountAttributes::default()
            },
            member_of: vec![group_ref.clone()],
        },
    )
    .await
    .unwrap();
    assert_eq!(created.len(), 2);
    let a = account(&*s, &name).await.expect("user listed");
    assert!(!a.role && a.locked == Some(false));
    assert_eq!(a.limits.map(|l| l.max_user_connections), Some(3));
    assert_eq!(a.member_of, std::slice::from_ref(&group_ref));
    assert!(a.auth_plugin.is_some());

    // Grants, then log in with the password (quotes and a backslash).
    let grant = |target: GrantTarget, privileges: &[&str], grant_option: bool| UserChange::Grant {
        account: user.clone(),
        target,
        privileges: privileges.iter().map(|p| (*p).to_owned()).collect(),
        grant_option,
    };
    run(&*s, grant(GrantTarget::Database { database: DB.into() }, &["SELECT", "SHOW VIEW"], true)).await.unwrap();
    let orders = GrantTarget::Table { database: DB.into(), schema: None, table: "orders".into() };
    run(&*s, grant(orders.clone(), &["UPDATE"], false)).await.unwrap();
    let login = ConnConfig { username: name.clone(), ..cfg.clone() };
    connect(&login, password).await.expect("log in as the new user").close().await;
    assert_eq!(connect(&login, "nope").await.err().expect("wrong password").kind, ErrorKind::Auth);

    let p = s.users().unwrap().privileges(&user, "").await.unwrap();
    assert!(p.unavailable.is_empty(), "{:?}", p.unavailable);
    let on_db = p.grants.iter().find(|g| g.level == GrantLevel::Database).expect("database grant");
    assert_eq!(on_db.database.as_deref(), Some(DB));
    assert!(on_db.privileges.iter().any(|p| p.privilege == "SELECT" && p.grantable));
    assert!(p.grants.iter().any(|g| g.level == GrantLevel::Table && g.object.as_deref() == Some("orders")));
    assert!(p.statements.iter().any(|l| l.contains("GRANT SELECT, SHOW VIEW ON")), "{:?}", p.statements);
    assert!(p.statements.iter().all(|l| !l.contains("w0rd")));

    run(
        &*s,
        UserChange::Revoke {
            account: user.clone(),
            target: GrantTarget::Database { database: DB.into() },
            privileges: vec!["SELECT".into()],
            grant_option_only: true,
        },
    )
    .await
    .unwrap();
    run(
        &*s,
        UserChange::Revoke {
            account: user.clone(),
            target: orders,
            privileges: vec!["UPDATE".into()],
            grant_option_only: false,
        },
    )
    .await
    .unwrap();
    let p = s.users().unwrap().privileges(&user, "").await.unwrap();
    assert!(p.grants.iter().all(|g| g.level != GrantLevel::Table));
    assert!(p.grants.iter().flat_map(|g| &g.privileges).all(|p| !p.grantable));

    // Lock, expire, new password, rename.
    run(
        &*s,
        UserChange::Alter {
            account: user.clone(),
            attributes: AccountAttributes { locked: Some(true), expire_password: true, ..AccountAttributes::default() },
        },
    )
    .await
    .unwrap();
    let a = account(&*s, &name).await.unwrap();
    assert_eq!((a.locked, a.password_expired), (Some(true), Some(true)));
    run(&*s, UserChange::SetPassword { account: user.clone(), password: Some(Password::new("n3w\\pw")) })
        .await
        .unwrap();
    run(
        &*s,
        UserChange::Alter {
            account: user.clone(),
            attributes: AccountAttributes { locked: Some(false), ..AccountAttributes::default() },
        },
    )
    .await
    .unwrap();
    connect(&login, "n3w\\pw").await.expect("new password").close().await;
    run(&*s, UserChange::Rename { account: user.clone(), new_name: renamed.clone() }).await.unwrap();
    let moved = AccountRef { name: renamed.clone(), host: Some("%".into()) };
    assert!(account(&*s, &name).await.is_none());
    run(&*s, UserChange::RevokeRole { account: moved.clone(), role: group_ref.clone() }).await.unwrap();
    assert_eq!(account(&*s, &renamed).await.unwrap().member_of.len(), 0);

    // Errors never carry the password.
    let duplicate = UserChange::Create {
        account: moved.clone(),
        role: false,
        password: Some(Password::new("Secret#123")),
        attributes: AccountAttributes::default(),
        member_of: Vec::new(),
    };
    let err = run(&*s, duplicate).await.unwrap_err();
    assert!(!format!("{err:?}").contains("Secret#123"));

    run(&*s, UserChange::Drop { account: moved, role: false, reassign_to: None, drop_owned: false, database: None })
        .await
        .unwrap();
    run(&*s, UserChange::Drop { account: group_ref, role: true, reassign_to: None, drop_owned: false, database: None })
        .await
        .unwrap();
    assert!(account(&*s, &renamed).await.is_none() && account(&*s, &group).await.is_none());
    s.close().await;
}
