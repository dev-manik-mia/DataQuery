//! Monitoring (screen 07) from `INFO`, `CLIENT LIST` and `CONFIG GET`.

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::monitor::{
    Gauge, Monitor, MonitorSnapshot, ObjectSize, Replication, ServerProcess, ServerSetting, short_query,
};
use dq_core::node::{NodeKind, ObjectRef};
use redis::Value;

use crate::session::{RedisSession, info_fields};

/// Keys sampled with `MEMORY USAGE` for "largest keys".
const SIZE_SAMPLE: usize = 10_000;

/// One `CLIENT LIST` line as `name=value` fields.
fn client_fields(line: &str) -> std::collections::HashMap<&str, &str> {
    line.split(' ').filter_map(|p| p.split_once('=')).collect()
}

pub(crate) fn process(line: &str, long_running_secs: u32) -> Option<(ServerProcess, bool)> {
    let f = client_fields(line);
    let pid = f.get("id")?.parse().ok()?;
    let num = |k: &str| f.get(k).and_then(|v| v.parse::<i64>().ok());
    let flags = f.get("flags").copied().unwrap_or("");
    let cmd = f.get("cmd").copied().filter(|c| *c != "NULL");
    let blocked = flags.contains('b');
    let idle = num("idle").unwrap_or(0);
    let active = blocked || idle == 0;
    let state = if blocked {
        "blocked"
    } else if flags.contains('x') {
        "in MULTI"
    } else if active {
        "active"
    } else {
        "idle"
    };
    let process = ServerProcess {
        pid,
        user: f.get("user").map(|s| (*s).to_owned()),
        database: f.get("db").map(|s| (*s).to_owned()),
        client: f.get("addr").map(|s| (*s).to_owned()),
        state: Some(state.to_owned()),
        duration_ms: Some(idle * 1000),
        query: short_query(cmd.map(str::to_owned)),
        wait: blocked.then(|| "blocking command".to_owned()),
        blocked_by: Vec::new(),
        active,
    };
    let long = blocked && idle >= i64::from(long_running_secs);
    Some((process, long))
}

#[async_trait]
impl Monitor for RedisSession {
    async fn snapshot(&self, long_running_secs: u32) -> DbResult<MonitorSnapshot> {
        let db = self.home_db();
        let info: String = self.query_catalog(db, &redis::cmd("INFO")).await?;
        let info = info_fields(&info);
        let int = |k: &str| info.get(k).and_then(|v| v.parse::<i64>().ok());
        let mut unavailable = Vec::new();
        let max = self
            .query_catalog::<Vec<String>>(db, redis::cmd("CONFIG").arg("GET").arg("maxclients"))
            .await
            .ok()
            .and_then(|r| r.get(1).and_then(|v| v.parse().ok()));
        let connections = int("connected_clients").map(|used| Gauge { used, max: max.unwrap_or(used) });
        let mut processes = Vec::new();
        let mut long_running = 0;
        match self.query_catalog::<String>(db, redis::cmd("CLIENT").arg("LIST")).await {
            Ok(list) => {
                for (p, long) in list.lines().filter_map(|l| process(l, long_running_secs)) {
                    long_running += i64::from(long);
                    processes.push(p);
                }
            }
            Err(e) => unavailable.push(format!("Clients: {}", e.message)),
        }
        let role = info.get("role").map(String::as_str);
        let replication = role.map(|role| {
            let replicas = int("connected_slaves").unwrap_or(0);
            if role == "slave" {
                Replication {
                    role: "replica".into(),
                    lag_secs: int("master_last_io_seconds_ago").map(|s| s as f64),
                    replicas,
                    state: info.get("master_link_status").cloned(),
                }
            } else {
                let lag = (0..replicas)
                    .filter_map(|i| info.get(&format!("slave{i}")))
                    .filter_map(|v| {
                        v.split(',').find_map(|p| p.strip_prefix("lag=")).and_then(|l| l.parse::<f64>().ok())
                    })
                    .reduce(f64::max);
                Replication { role: "primary".into(), lag_secs: lag, replicas, state: None }
            }
        });
        Ok(MonitorSnapshot {
            version: self.info.version.clone(),
            connections,
            active_queries: Some(processes.iter().filter(|p| p.active).count() as i64),
            long_running: Some(long_running),
            blocked: int("blocked_clients"),
            replication,
            processes,
            locks: Vec::new(),
            unavailable,
        })
    }

    async fn server_settings(&self, search: Option<&str>) -> DbResult<Vec<ServerSetting>> {
        let pairs: Vec<String> = self.query_catalog(self.home_db(), redis::cmd("CONFIG").arg("GET").arg("*")).await?;
        let search = search.map(str::to_ascii_lowercase).filter(|s| !s.is_empty());
        let mut out: Vec<ServerSetting> = pairs
            .chunks(2)
            .filter(|p| p.len() == 2)
            .filter(|p| search.as_ref().is_none_or(|s| p[0].to_ascii_lowercase().contains(s)))
            .map(|p| ServerSetting {
                name: p[0].clone(),
                value: p[1].clone(),
                unit: None,
                category: None,
                description: None,
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// The biggest keys among a sample of up to 10,000 (`MEMORY USAGE`).
    async fn largest_tables(&self, database: &str, limit: u32) -> DbResult<Vec<ObjectSize>> {
        let db: u32 = database.parse().unwrap_or_else(|_| self.home_db());
        if !self.memory_usage {
            return Ok(Vec::new());
        }
        let mut cursor = "0".to_owned();
        let mut sizes: Vec<(Vec<u8>, i64)> = Vec::new();
        loop {
            let (next, keys): (String, Vec<Vec<u8>>) =
                self.query_catalog(db, redis::cmd("SCAN").arg(&cursor).arg("COUNT").arg(1000)).await?;
            if !keys.is_empty() {
                let mut pipe = redis::pipe();
                for k in &keys {
                    pipe.cmd("MEMORY").arg("USAGE").arg(k.as_slice());
                }
                let usage: Vec<Option<i64>> = self.pipe_catalog(db, &pipe).await?;
                sizes.extend(keys.into_iter().zip(usage).filter_map(|(k, u)| u.map(|u| (k, u))));
            }
            if next == "0" || sizes.len() >= SIZE_SAMPLE {
                break;
            }
            cursor = next;
        }
        sizes.sort_by_key(|s| std::cmp::Reverse(s.1));
        sizes.truncate(limit as usize);
        Ok(sizes
            .into_iter()
            .map(|(key, bytes)| {
                let key = dq_core::keyvalue::RString::from_bytes(&key);
                let mut object = ObjectRef::new(NodeKind::Key, db.to_string(), None, key.text);
                if key.binary {
                    object = object.with_parent("binary");
                }
                ObjectSize { object, total_bytes: bytes, table_bytes: bytes, index_bytes: 0, rows: None }
            })
            .collect())
    }

    /// `CLIENT KILL ID`: Redis cannot stop only the running command, so both end the connection.
    async fn kill(&self, pid: i64, _terminate: bool) -> DbResult<bool> {
        let reply: DbResult<Value> =
            self.query_catalog(self.home_db(), redis::cmd("CLIENT").arg("KILL").arg("ID").arg(pid)).await;
        Ok(matches!(reply, Ok(Value::Int(n)) if n > 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_list_lines() {
        let line =
            "id=7 addr=127.0.0.1:5000 laddr=x fd=8 name=DataQuery age=12 idle=5 flags=b db=3 cmd=blpop user=default";
        let (p, long) = process(line, 2).unwrap();
        assert_eq!((p.pid, p.state.as_deref(), p.database.as_deref()), (7, Some("blocked"), Some("3")));
        assert!(p.active && long);
        let (idle, _) = process("id=9 flags=N idle=40 cmd=NULL", 60).unwrap();
        assert_eq!((idle.state.as_deref(), idle.query, idle.active), (Some("idle"), None, false));
        assert!(process("garbage", 1).is_none());
    }
}
