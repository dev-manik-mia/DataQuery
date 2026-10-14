use oracle_rs::{Config, Connection};

async fn conn(cache: usize) -> Connection {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Connection::connect_with_config(Config::new("localhost", 51521, "FREEPDB1", "dataquery", "dataquery").with_statement_cache_size(cache)).await.unwrap()
}

#[tokio::test]
async fn probe() {
    for cache in [0usize, 20] {
        let c = conn(cache).await;
        let e = c.execute("SELECT nope FROM dual", &[]).await;
        println!("cache {cache}: execute err {:?}", e.err());
        println!("  alive? {:?}", c.query("SELECT 1 FROM dual", &[]).await.map(|r| r.rows.len()));
        let c = conn(cache).await;
        let r = c.query("SELECT LEVEL FROM dual CONNECT BY LEVEL <= 250", &[]).await.unwrap();
        println!("  rows {} more {}", r.rows.len(), r.has_more_rows);
        let r = c.query("SELECT LEVEL FROM dual CONNECT BY LEVEL <= 250 ORDER BY 1", &[]).await.unwrap();
        println!("  rows2 {} more {}", r.rows.len(), r.has_more_rows);
    }
}
