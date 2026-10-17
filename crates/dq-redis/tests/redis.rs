//! Integration tests against Redis 8, Valkey 8 and Dragonfly (`npm run db:up`).
//! Each test runs against every server whose variable is set:
//! `DATAQUERY_TEST_REDIS=localhost:56379`, `DATAQUERY_TEST_VALKEY=localhost:56380`,
//! `DATAQUERY_TEST_DRAGONFLY=localhost:56381`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dq_core::adapter::{ConnectParams, DatabaseAdapter, Session};
use dq_core::config::{ConnConfig, SslMode};
use dq_core::engine::Engine;
use dq_core::keyvalue::{
    KeyEdit, KeyRef, KeyType, KeyValue, KeyWrite, MAX_STRING_BYTES, RString, ScanRequest, ValueRequest,
};
use dq_core::result::ExecOptions;
use dq_core::value::Cell;
use dq_core::{DbResult, ErrorKind};
use dq_redis::RedisAdapter;

/// A database the tests own, away from anything a developer keeps in 0.
const DB: u32 = 9;

fn servers() -> Vec<(RedisAdapter, ConnConfig)> {
    [
        ("DATAQUERY_TEST_REDIS", RedisAdapter::REDIS, Engine::Redis),
        ("DATAQUERY_TEST_VALKEY", RedisAdapter::VALKEY, Engine::Valkey),
        ("DATAQUERY_TEST_DRAGONFLY", RedisAdapter::DRAGONFLY, Engine::Dragonfly),
    ]
    .into_iter()
    .filter_map(|(var, adapter, engine)| {
        let addr = std::env::var(var).ok()?;
        let (host, port) = addr.split_once(':')?;
        Some((
            adapter,
            ConnConfig {
                engine,
                host: host.into(),
                port: port.parse().ok()?,
                database: DB.to_string(),
                username: String::new(),
                ssl_mode: SslMode::Disable,
                ..ConnConfig::default()
            },
        ))
    })
    .collect()
}

async fn open(adapter: RedisAdapter, config: &ConnConfig) -> Arc<dyn Session> {
    adapter.connect(&ConnectParams::new(config.clone(), Some("dataquery".into()))).await.expect("connect")
}

fn prefix() -> String {
    format!("dqtest:{}:", uuid::Uuid::new_v4().simple())
}

fn key(name: &str) -> KeyRef {
    KeyRef { database: DB, key: RString::text(name) }
}

async fn console(s: &Arc<dyn Session>, line: &str) -> String {
    s.keys().unwrap().console(line, 10).await.expect(line).text
}

/// Deletes every key with `prefix` in the test database.
async fn cleanup(s: &Arc<dyn Session>, prefix: &str) {
    let store = s.keys().unwrap();
    let mut req =
        ScanRequest { database: DB, pattern: format!("{prefix}*"), key_type: None, cursor: None, count: 1000 };
    loop {
        let page = store.scan(&req).await.unwrap();
        for k in page.keys {
            let _ = store
                .write(&KeyWrite { key: KeyRef { database: DB, key: k.key }, edit: KeyEdit::Delete, create: false })
                .await;
        }
        match page.cursor {
            Some(c) => req.cursor = Some(c),
            None => break,
        }
    }
}

const OPTS: ExecOptions = ExecOptions { row_limit: 100, timeout_secs: 30 };

#[tokio::test]
async fn connects_and_detects_the_server() -> DbResult<()> {
    for (adapter, config) in servers() {
        let info = adapter.test(&ConnectParams::new(config.clone(), Some("dataquery".into()))).await?;
        let expected = match config.engine {
            Engine::Valkey => "Valkey ",
            Engine::Dragonfly => "Dragonfly ",
            _ => "Redis ",
        };
        assert!(info.version.starts_with(expected), "{}", info.version);
        assert_eq!((info.database.as_str(), info.user.as_str()), ("9", "default"));
        let wrong = adapter.test(&ConnectParams::new(config.clone(), Some("nope".into()))).await.unwrap_err();
        assert_eq!(wrong.kind, ErrorKind::Auth, "{wrong:?}");
    }
    Ok(())
}

#[tokio::test]
async fn lists_databases_with_key_counts() -> DbResult<()> {
    for (adapter, config) in servers() {
        let s = open(adapter, &config).await;
        let p = prefix();
        console(&s, &format!("SET {p}a 1")).await;
        let dbs = s.list_children(None).await?;
        assert_eq!(dbs.len(), 16);
        let nine = &dbs[9];
        assert!(nine.current && nine.row_estimate.unwrap_or(0) >= 1);
        assert_eq!(s.keys().unwrap().features().databases, 16);
        assert!(s.keys().unwrap().key_count(DB).await? >= 1);
        let found = s.search(&p[7..], 10).await?;
        assert_eq!(found.len(), 1);
        cleanup(&s, &p).await;
    }
    Ok(())
}

#[tokio::test]
async fn scans_with_match_type_and_cursor() -> DbResult<()> {
    for (adapter, config) in servers() {
        let s = open(adapter, &config).await;
        let store = s.keys().unwrap();
        let p = prefix();
        for i in 0..30 {
            console(&s, &format!("SET {p}s{i} v")).await;
        }
        console(&s, &format!("HSET {p}h f v")).await;
        console(&s, &format!("PEXPIRE {p}s0 60000")).await;
        // A binary key name round-trips through the escaped form.
        console(&s, &format!("SET \"{p}bin\\xff\\x00\" x")).await;
        let mut req = ScanRequest { database: DB, pattern: format!("{p}*"), key_type: None, cursor: None, count: 5 };
        let mut all = Vec::new();
        loop {
            let page = store.scan(&req).await?;
            all.extend(page.keys);
            match page.cursor {
                Some(c) => req.cursor = Some(c),
                None => break,
            }
        }
        assert_eq!(all.len(), 32);
        let s0 = all.iter().find(|k| k.key.text == format!("{p}s0")).unwrap();
        assert!(s0.ttl_ms.is_some_and(|t| t > 0 && t <= 60_000));
        let bin = all.iter().find(|k| k.key.binary).unwrap();
        assert_eq!(bin.key.text, format!("{p}bin\\xff\\x00"));
        let page = store
            .value(&ValueRequest { key: KeyRef { database: DB, key: bin.key.clone() }, cursor: None, count: 10 })
            .await?;
        assert_eq!(page.value, KeyValue::String { value: RString::text("x"), truncated: false });
        let hashes = ScanRequest { key_type: Some(KeyType::Hash), cursor: None, count: 1000, ..req };
        let mut found = Vec::new();
        let mut hashes = hashes;
        loop {
            let page = store.scan(&hashes).await?;
            found.extend(page.keys);
            match page.cursor {
                Some(c) => hashes.cursor = Some(c),
                None => break,
            }
        }
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key_type, KeyType::Hash);
        cleanup(&s, &p).await;
    }
    Ok(())
}

#[tokio::test]
async fn reads_every_type_in_pages() -> DbResult<()> {
    for (adapter, config) in servers() {
        let s = open(adapter, &config).await;
        let store = s.keys().unwrap();
        let p = prefix();
        for i in 0..25 {
            console(&s, &format!("HSET {p}h f{i} v{i}")).await;
            console(&s, &format!("RPUSH {p}l item{i}")).await;
            console(&s, &format!("SADD {p}set m{i}")).await;
            console(&s, &format!("XADD {p}x * n {i}")).await;
        }
        console(&s, &format!("ZADD {p}z 1.5 a 0.1 b inf c")).await;
        let read_all = |name: String| async move {
            let mut req = ValueRequest { key: key(&name), cursor: None, count: 10 };
            let mut pages = Vec::new();
            loop {
                let page = store.value(&req).await?;
                let next = page.next.clone();
                pages.push(page);
                match next {
                    Some(c) => req.cursor = Some(c),
                    None => return DbResult::Ok(pages),
                }
            }
        };
        let hash = read_all(format!("{p}h")).await?;
        let fields: usize = hash
            .iter()
            .map(|pg| match &pg.value {
                KeyValue::Hash { entries } => entries.len(),
                _ => 0,
            })
            .sum();
        assert_eq!((fields, hash[0].details.length), (25, Some(25)));
        let list = read_all(format!("{p}l")).await?;
        assert_eq!(list.len(), 3);
        let KeyValue::List { items } = &list[2].value else { panic!() };
        assert_eq!((items[0].index, items[0].value.text.as_str()), (20, "item20"));
        let set = read_all(format!("{p}set")).await?;
        let members: usize = set
            .iter()
            .map(|pg| match &pg.value {
                KeyValue::Set { members } => members.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(members, 25);
        let z = read_all(format!("{p}z")).await?;
        let KeyValue::Zset { members } = &z[0].value else { panic!() };
        let scores: Vec<(&str, &str)> = members.iter().map(|m| (m.member.text.as_str(), m.score.as_str())).collect();
        assert_eq!(scores, [("b", "0.1"), ("a", "1.5"), ("c", "inf")]);
        let x = read_all(format!("{p}x")).await?;
        let entries: usize = x
            .iter()
            .map(|pg| match &pg.value {
                KeyValue::Stream { entries } => entries.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(entries, 25);
        let details = &x[0].details;
        assert!(details.memory_bytes.is_some());
        // Dragonfly has no OBJECT command, so it reports no encoding.
        assert_eq!(details.encoding.is_some(), config.engine != Engine::Dragonfly);
        // A long string is cut for the viewer.
        let big = "a".repeat(MAX_STRING_BYTES + 10);
        store
            .write(&KeyWrite {
                key: key(&format!("{p}big")),
                edit: KeyEdit::SetString { value: RString::text(big) },
                create: true,
            })
            .await?;
        let page = store.value(&ValueRequest { key: key(&format!("{p}big")), cursor: None, count: 1 }).await?;
        let KeyValue::String { value, truncated } = page.value else { panic!() };
        assert!(truncated && value.text.len() == MAX_STRING_BYTES);
        let missing =
            store.value(&ValueRequest { key: key(&format!("{p}none")), cursor: None, count: 1 }).await.unwrap_err();
        assert_eq!(missing.kind, ErrorKind::NotFound);
        cleanup(&s, &p).await;
    }
    Ok(())
}

#[tokio::test]
async fn edits_every_type_and_detects_conflicts() -> DbResult<()> {
    for (adapter, config) in servers() {
        let s = open(adapter, &config).await;
        let store = s.keys().unwrap();
        let p = prefix();
        let w = |name: &str, edit: KeyEdit, create: bool| KeyWrite { key: key(&format!("{p}{name}")), edit, create };
        let r = RString::text;
        store.write(&w("s", KeyEdit::SetString { value: r("one") }, true)).await?;
        let dup = store.write(&w("s", KeyEdit::SetString { value: r("two") }, true)).await.unwrap_err();
        assert_eq!(dup.kind, ErrorKind::Constraint);
        store.write(&w("s", KeyEdit::Expire { ttl_ms: Some(50_000) }, false)).await?;
        store.write(&w("s", KeyEdit::SetString { value: r("two") }, false)).await?;
        let pttl = console(&s, &format!("PTTL {p}s")).await;
        let left: i64 = pttl.trim_start_matches("(integer) ").parse().unwrap();
        assert!((40_000..=50_000).contains(&left), "SET keeps the TTL: {pttl}");
        store.write(&w("s", KeyEdit::Expire { ttl_ms: None }, false)).await?;
        assert_eq!(console(&s, &format!("TTL {p}s")).await, "(integer) -1");
        store.write(&w("h", KeyEdit::HashSet { field: r("f"), value: r("v") }, true)).await?;
        store.write(&w("h", KeyEdit::HashDelete { field: r("f") }, false)).await?;
        let gone = store.write(&w("h", KeyEdit::HashDelete { field: r("f") }, false)).await.unwrap_err();
        assert_eq!(gone.kind, ErrorKind::NotFound);
        for v in ["a", "b", "c"] {
            store.write(&w("l", KeyEdit::ListPush { value: r(v), head: false }, false)).await?;
        }
        store.write(&w("l", KeyEdit::ListSet { index: 1, expected: r("b"), value: r("B") }, false)).await?;
        let stale = store
            .write(&w("l", KeyEdit::ListSet { index: 1, expected: r("b"), value: r("x") }, false))
            .await
            .unwrap_err();
        assert_eq!(stale.kind, ErrorKind::Constraint);
        store.write(&w("l", KeyEdit::ListRemove { index: 0, expected: r("a") }, false)).await?;
        assert_eq!(console(&s, &format!("LRANGE {p}l 0 -1")).await, "1) \"B\"\n2) \"c\"");
        store.write(&w("set", KeyEdit::SetAdd { member: r("m") }, false)).await?;
        store.write(&w("set", KeyEdit::SetRemove { member: r("m") }, false)).await?;
        store.write(&w("z", KeyEdit::ZsetAdd { member: r("m"), score: "2.25".into() }, false)).await?;
        let bad =
            store.write(&w("z", KeyEdit::ZsetAdd { member: r("m"), score: "abc".into() }, false)).await.unwrap_err();
        assert_eq!(bad.kind, ErrorKind::InvalidInput);
        assert_eq!(console(&s, &format!("ZSCORE {p}z m")).await, "\"2.25\"");
        store.write(&w("z", KeyEdit::ZsetRemove { member: r("m") }, false)).await?;
        store.write(&w("x", KeyEdit::StreamAdd { fields: vec![(r("k"), r("v"))] }, false)).await?;
        let page = store.value(&ValueRequest { key: key(&format!("{p}x")), cursor: None, count: 10 }).await?;
        let KeyValue::Stream { entries } = page.value else { panic!() };
        store.write(&w("x", KeyEdit::StreamDelete { id: entries[0].id.clone() }, false)).await?;
        store.write(&w("other", KeyEdit::SetString { value: r("o") }, false)).await?;
        let taken = store
            .write(&w("s", KeyEdit::Rename { new_key: RString::text(format!("{p}other")) }, false))
            .await
            .unwrap_err();
        assert_eq!(taken.kind, ErrorKind::Constraint);
        store.write(&w("s", KeyEdit::Rename { new_key: RString::text(format!("{p}renamed")) }, false)).await?;
        let missing = store
            .write(&w("s", KeyEdit::Rename { new_key: RString::text(format!("{p}x2")) }, false))
            .await
            .unwrap_err();
        assert_eq!(missing.kind, ErrorKind::NotFound);
        store.write(&w("renamed", KeyEdit::Delete, false)).await?;
        assert_eq!(store.write(&w("renamed", KeyEdit::Delete, false)).await.unwrap_err().kind, ErrorKind::NotFound);
        cleanup(&s, &p).await;
    }
    Ok(())
}

#[tokio::test]
async fn console_prints_like_redis_cli() -> DbResult<()> {
    for (adapter, config) in servers() {
        let s = open(adapter, &config).await;
        let store = s.keys().unwrap();
        let p = prefix();
        assert_eq!(console(&s, &format!("INCR {p}n")).await, "(integer) 1");
        assert_eq!(console(&s, &format!("GET {p}missing")).await, "(nil)");
        assert_eq!(console(&s, &format!("SET {p}q \"a\\\"b\"")).await, "OK");
        assert_eq!(console(&s, &format!("GET {p}q")).await, "\"a\\\"b\"");
        console(&s, &format!("RPUSH {p}l x y")).await;
        assert_eq!(console(&s, &format!("LRANGE {p}l 0 -1")).await, "1) \"x\"\n2) \"y\"");
        let err = store.console(&format!("LPUSH {p}n x"), 10).await?;
        assert!(err.error && err.text.starts_with("(error) WRONGTYPE"), "{}", err.text);
        let selected = store.console("SELECT 3", 10).await?;
        assert_eq!((selected.text.as_str(), selected.database), ("OK", 3));
        let back = store.console(&format!("SELECT {DB}"), 10).await?;
        assert_eq!(back.database, DB);
        assert!(store.console("GET \"open", 10).await.is_err());
        cleanup(&s, &p).await;
    }
    Ok(())
}

#[tokio::test]
async fn editor_grids_transactions_and_read_only() -> DbResult<()> {
    for (adapter, config) in servers() {
        let s = open(adapter, &config).await;
        let p = prefix();
        let r = s.execute(&format!("HSET {p}h a 1 b 2"), &[], OPTS).await?;
        assert_eq!((r.affected, r.command.as_str()), (Some(2), "HSET"));
        let r = s.execute(&format!("HGETALL {p}h"), &[], OPTS).await?;
        assert_eq!(r.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["field", "value"]);
        assert_eq!(r.rows.len(), 2);
        for i in 0..250 {
            console(&s, &format!("RPUSH {p}l {i}")).await;
        }
        let r = s.execute(&format!("LRANGE {p}l 0 -1"), &[], OPTS).await?;
        assert!(r.has_more && r.rows.len() == 100);
        let more = s.fetch_more(500).await?;
        assert_eq!((more.rows.len(), more.has_more), (150, false));
        assert_eq!(more.rows[0][1], Cell::text("100", dq_core::value::CellLimits::GRID));
        // MULTI/EXEC on the editor's connection.
        assert_eq!(s.execute("MULTI", &[], OPTS).await?.rows[0][0], Cell::text("OK", dq_core::value::CellLimits::GRID));
        s.execute(&format!("INCR {p}c"), &[], OPTS).await?;
        s.execute(&format!("INCR {p}c"), &[], OPTS).await?;
        let exec = s.execute("EXEC", &[], OPTS).await?;
        assert_eq!(exec.rows.len(), 2);
        assert_eq!(console(&s, &format!("GET {p}c")).await, "\"2\"");
        assert!(s.execute("GET a", &[Some("x".into())], OPTS).await.is_err());
        let ro = open(adapter, &ConnConfig { read_only: true, ..config.clone() }).await;
        assert_eq!(ro.execute(&format!("SET {p}x 1"), &[], OPTS).await.unwrap_err().kind, ErrorKind::Permission);
        assert!(ro.execute(&format!("GET {p}c"), &[], OPTS).await.is_ok());
        let edit = KeyWrite { key: key(&format!("{p}c")), edit: KeyEdit::Delete, create: false };
        assert_eq!(ro.keys().unwrap().write(&edit).await.unwrap_err().kind, ErrorKind::Permission);
        cleanup(&s, &p).await;
    }
    Ok(())
}

#[tokio::test]
async fn cancel_and_timeout_stop_blocking_commands() -> DbResult<()> {
    for (adapter, config) in servers() {
        let s = open(adapter, &config).await;
        let p = prefix();
        let runner = s.clone();
        let blpop = format!("BLPOP {p}never 10");
        let task = tokio::spawn(async move { runner.execute(&blpop, &[], OPTS).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let started = Instant::now();
        assert!(s.cancel().await?);
        let err = task.await.unwrap().unwrap_err();
        assert_eq!(err.kind, ErrorKind::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(3));
        // The session works afterwards, on the same database.
        assert_eq!(console(&s, "PING").await, "PONG");
        assert!(!s.cancel().await?);
        let started = Instant::now();
        let err = s.keys().unwrap().console(&format!("BLPOP {p}never 10"), 1).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Timeout);
        assert!(started.elapsed() < Duration::from_secs(4));
        let info = s.keys().unwrap().console(&format!("SET {p}after 1"), 5).await?;
        assert_eq!(info.database, DB);
        cleanup(&s, &p).await;
    }
    Ok(())
}

#[tokio::test]
async fn monitor_sees_clients() -> DbResult<()> {
    for (adapter, config) in servers() {
        let s = open(adapter, &config).await;
        let monitor = s.monitor().unwrap();
        let snap = monitor.snapshot(5).await?;
        assert!(snap.connections.is_some_and(|c| c.used >= 1 && c.max >= c.used));
        assert!(snap.processes.iter().any(|p| p.database.as_deref() == Some("9")));
        assert_eq!(snap.replication.as_ref().map(|r| r.role.as_str()), Some("primary"));
        assert!(!monitor.server_settings(Some("maxmemory")).await?.is_empty());
        assert!(!monitor.kill(987_654_321, false).await?);
        let p = prefix();
        console(&s, &format!("SET {p}big {}", "x".repeat(5000))).await;
        let sizes = monitor.largest_tables("9", 5).await?;
        assert!(!sizes.is_empty());
        cleanup(&s, &p).await;
    }
    Ok(())
}

#[tokio::test]
async fn exports_keys_and_command_rows() -> DbResult<()> {
    for (adapter, config) in servers() {
        let s = open(adapter, &config).await;
        let p = prefix();
        for i in 0..1500 {
            console(&s, &format!("SET {p}k{i} {i}")).await;
        }
        let req = ScanRequest { database: DB, pattern: format!("{p}*"), key_type: None, cursor: None, count: 500 };
        let mut rows = 0;
        let total = s
            .keys()
            .unwrap()
            .stream_keys(&req, &mut |cols, batch| {
                assert_eq!(cols.len(), 3);
                rows += batch.len();
                Ok(())
            })
            .await?;
        assert_eq!((total, rows), (1500, 1500));
        let (cols, data) = s.read_query("9", &format!("GET {p}k7"), &[], dq_core::value::CellLimits::FULL).await?;
        assert_eq!(
            (cols[0].name.as_str(), data[0][0].clone()),
            ("reply", Cell::text("7", dq_core::value::CellLimits::FULL))
        );
        cleanup(&s, &p).await;
    }
    Ok(())
}

#[tokio::test]
async fn atomic_writes_reuse_their_connection_until_an_error() -> DbResult<()> {
    for (adapter, config) in servers() {
        let s = open(adapter, &config).await;
        let p = prefix();
        // `CLIENT ID` replies with a number, which comes back as the "count".
        let run = |commands: Vec<String>| {
            let s = s.clone();
            async move {
                let mut bound = commands
                    .into_iter()
                    .map(|sql| Ok(dq_core::edit::BoundStatement { sql, params: Vec::new(), expect_rows: None }));
                s.run_in_transaction(&DB.to_string(), &mut bound).await
            }
        };
        let first = run(vec![format!("SET {p}a 1"), "CLIENT ID".into()]).await?;
        let second = run(vec![format!("SET {p}a 2"), "CLIENT ID".into()]).await?;
        assert_eq!(first[1], second[1], "the second transaction reused the connection");
        // A command the server rejects aborts the whole transaction.
        assert!(run(vec![format!("SET {p}a 3"), "NOT_A_COMMAND".into()]).await.is_err());
        let third = run(vec!["CLIENT ID".into()]).await?;
        assert_ne!(second[1], third[0], "the failed transaction's connection was dropped");
        assert_eq!(console(&s, &format!("GET {p}a")).await, "\"2\"");
        cleanup(&s, &p).await;
    }
    Ok(())
}
