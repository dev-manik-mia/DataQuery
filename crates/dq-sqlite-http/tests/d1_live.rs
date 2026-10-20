//! Read-only checks against a real Cloudflare D1 database. Nothing is written:
//! the connection uses a read-only profile, and the only write attempted must be
//! refused. Only table names and counts are printed, never row values.
//!
//! ```sh
//! DATAQUERY_TEST_D1_ACCOUNT=<account id> DATAQUERY_TEST_D1_DATABASE=<database id or name> \
//! DATAQUERY_TEST_D1_TOKEN=<API token with D1 read> cargo test -p dq-sqlite-http --test d1_live -- --nocapture
//! ```

use std::collections::BTreeMap;

use dq_core::adapter::{ConnectParams, DatabaseAdapter};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::engine::Engine;
use dq_core::node::NodeKind;
use dq_core::result::ExecOptions;
use dq_sqlite_http::D1Adapter;

struct Live {
    account: String,
    database: String,
    token: String,
}

fn live() -> Option<Live> {
    let var = |name| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    Some(Live {
        account: var("DATAQUERY_TEST_D1_ACCOUNT")?,
        database: var("DATAQUERY_TEST_D1_DATABASE")?,
        token: var("DATAQUERY_TEST_D1_TOKEN")?,
    })
}

fn params(live: &Live, database: &str) -> ConnectParams {
    let config = ConnConfig {
        engine: Engine::D1,
        host: "api.cloudflare.com".into(),
        port: 443,
        database: database.into(),
        ssl_mode: SslMode::VerifyFull,
        read_only: true,
        properties: BTreeMap::from([(ConnConfig::D1_ACCOUNT.to_owned(), live.account.clone())]),
        ..ConnConfig::default()
    };
    ConnectParams::new(config, Some(live.token.clone()))
}

const OPTS: ExecOptions = ExecOptions { row_limit: 20, timeout_secs: 30 };

#[tokio::test]
async fn real_d1_read_only() {
    let Some(live) = live() else {
        eprintln!("skipped: set DATAQUERY_TEST_D1_ACCOUNT, DATAQUERY_TEST_D1_DATABASE and DATAQUERY_TEST_D1_TOKEN");
        return;
    };

    let info = D1Adapter.test(&params(&live, &live.database)).await.expect("test connection");
    println!("server: {} ({} ms), read-only: {}", info.version, info.latency_ms, info.read_only);

    let databases = D1Adapter.databases(&params(&live, &live.database)).await.expect("database list");
    println!("account databases: {}", databases.len());

    let session = D1Adapter.connect(&params(&live, &live.database)).await.expect("connect");
    let schemas = session.list_children(None).await.expect("explorer root");
    let main = schemas.first().expect("main").object.clone();
    let mut tables = Vec::new();
    for group in session.list_children(Some(&main)).await.expect("explorer groups") {
        let children = if group.object.kind == NodeKind::Table {
            vec![group]
        } else {
            session.list_children(Some(&group.object)).await.unwrap_or_default()
        };
        tables.extend(children.into_iter().filter(|n| matches!(n.object.kind, NodeKind::Table | NodeKind::View)));
    }
    println!("tables and views: {}", tables.len());
    assert!(tables.iter().all(|t| !t.object.name.starts_with("_cf_")), "internal tables are hidden");

    for table in tables.iter().take(25) {
        let structure = session
            .structure(&table.object)
            .await
            .unwrap_or_else(|e| panic!("structure of {}: {e:?}", table.object.name));
        let ddl = session.ddl(&table.object).await.unwrap_or_else(|e| panic!("ddl of {}: {e:?}", table.object.name));
        let name = session.dialect().quote_ident(&table.object.name);
        let counted = session.execute(&format!("SELECT count(*) FROM {name}"), &[], OPTS).await.expect("count");
        let count = counted.rows.first().and_then(|r| r.first()).map(|c| format!("{c:?}")).unwrap_or_default();
        let page = session.execute(&format!("SELECT * FROM {name}"), &[], OPTS).await.expect("first page");
        let shown = page.rows.len();
        assert!(shown <= OPTS.row_limit as usize, "results stay bounded");
        println!(
            "  {:<32} {:>2} columns, {:>2} keys, ddl {:>5} chars, rows {count}, first page {shown}",
            table.object.name,
            structure.columns.len(),
            structure.constraints.len(),
            ddl.len()
        );
    }

    let model = session.schema_model("main", None).await.expect("schema model");
    println!("schema model tables: {}", model.tables.len());
    let plan = session.explain("SELECT * FROM sqlite_master", &[], false, 30).await;
    println!("explain: {}", if plan.is_ok() { "ok" } else { "not available" });

    let refused = session.execute("CREATE TABLE dataquery_live_check (a)", &[], OPTS).await;
    assert!(refused.is_err(), "the read-only profile must refuse writes");
    println!("write refused: {}", refused.unwrap_err().message);

    session.close().await;
}
