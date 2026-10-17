//! The key browser (screen 12): `SCAN`-family paging, typed value pages and
//! guarded edits. `KEYS` is never sent.

use std::time::Instant;

use async_trait::async_trait;
use dq_core::adapter::BatchFn;
use dq_core::keyvalue::{
    ConsoleReply, FieldValue, KeyDetails, KeyEdit, KeyEntry, KeyType, KeyValue, KeyValueFeatures, KeyValueStore,
    KeyWrite, ListItem, MAX_STRING_BYTES, RString, ScanPage, ScanRequest, ScoredMember, StreamEntry, ValuePage,
    ValueRequest,
};
use dq_core::result::ColumnMeta;
use dq_core::value::{Cell, CellLimits};
use dq_core::{DbError, DbResult, ErrorKind};
use redis::Value;

use crate::client::map_error;
use crate::reply::format_cli;
use crate::session::{RedisSession, parse_line, read_only_error};

const EXPORT_BATCH: usize = 1_000;

fn gone() -> DbError {
    DbError::not_found("The key no longer exists. Reload.")
}

fn changed(what: &str) -> DbError {
    DbError::new(ErrorKind::Constraint, format!("The {what} changed. Reload."))
}

/// Bytes of a bulk or simple string reply.
fn bytes(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::BulkString(b) => Some(b.clone()),
        Value::SimpleString(s) => Some(s.as_bytes().to_vec()),
        Value::Int(i) => Some(i.to_string().into_bytes()),
        Value::Okay => Some(b"OK".to_vec()),
        _ => None,
    }
}

fn text(v: &Value) -> String {
    bytes(v).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
}

fn rstring(v: &Value) -> RString {
    RString::from_bytes(&bytes(v).unwrap_or_default())
}

fn items(v: Value) -> Vec<Value> {
    match v {
        Value::Array(items) | Value::Set(items) => items,
        Value::Map(pairs) => pairs.into_iter().flat_map(|(k, v)| [k, v]).collect(),
        _ => Vec::new(),
    }
}

/// `field value field value …` as pairs.
fn field_values(v: Value) -> Vec<FieldValue> {
    items(v)
        .chunks(2)
        .filter(|p| p.len() == 2)
        .map(|p| FieldValue { field: rstring(&p[0]), value: rstring(&p[1]) })
        .collect()
}

/// A score `ZADD` accepts: a number or ±inf.
pub(crate) fn valid_score(score: &str) -> bool {
    let s = score.trim();
    matches!(s.to_ascii_lowercase().as_str(), "inf" | "+inf" | "-inf") || s.parse::<f64>().is_ok_and(f64::is_finite)
}

/// Cursor of a by-index page (lists, sorted sets): the next start index.
pub(crate) fn next_index(start: i64, read: usize, count: u32, length: u64) -> Option<String> {
    let next = start + read as i64;
    (read as u32 >= count && (next as u64) < length).then(|| next.to_string())
}

fn ttl(pttl: i64) -> Option<i64> {
    (pttl >= 0).then_some(pttl)
}

impl RedisSession {
    fn write_guard(&self) -> DbResult<()> {
        if self.read_only { Err(read_only_error()) } else { Ok(()) }
    }

    /// `TYPE` and `PTTL` of keys; keys that vanished meanwhile are left out.
    async fn entries(&self, database: u32, keys: Vec<Vec<u8>>) -> DbResult<Vec<KeyEntry>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut pipe = redis::pipe();
        for k in &keys {
            pipe.cmd("TYPE").arg(k.as_slice()).cmd("PTTL").arg(k.as_slice());
        }
        let replies: Vec<Value> = self.pipe_catalog(database, &pipe).await?;
        Ok(keys
            .iter()
            .zip(replies.chunks(2))
            .filter_map(|(key, r)| {
                let type_name = text(&r[0]);
                let pttl = match r.get(1) {
                    Some(Value::Int(n)) => *n,
                    _ => -1,
                };
                (type_name != "none" && pttl != -2).then(|| KeyEntry {
                    key: RString::from_bytes(key),
                    key_type: KeyType::from_type_name(&type_name),
                    type_name,
                    ttl_ms: ttl(pttl),
                })
            })
            .collect())
    }

    /// One `SCAN` step.
    async fn scan_step(&self, request: &ScanRequest, cursor: &str) -> DbResult<(String, Vec<Vec<u8>>)> {
        let mut cmd = redis::cmd("SCAN");
        cmd.arg(cursor).arg("MATCH").arg(request.pattern()).arg("COUNT").arg(request.count());
        if let Some(t) = request.key_type.and_then(KeyType::type_name) {
            cmd.arg("TYPE").arg(t);
        }
        self.query_catalog(request.database, &cmd).await
    }

    async fn details(&self, database: u32, key: &[u8]) -> DbResult<KeyDetails> {
        let mut pipe = redis::pipe();
        pipe.cmd("TYPE").arg(key).cmd("PTTL").arg(key);
        let r: Vec<Value> = self.pipe_catalog(database, &pipe).await?;
        let type_name = r.first().map(text).unwrap_or_default();
        if type_name == "none" {
            return Err(gone());
        }
        let key_type = KeyType::from_type_name(&type_name);
        let pttl = match r.get(1) {
            Some(Value::Int(n)) => *n,
            _ => -1,
        };
        let memory_bytes = if self.memory_usage {
            self.query_catalog::<Option<i64>>(database, redis::cmd("MEMORY").arg("USAGE").arg(key)).await.ok().flatten()
        } else {
            None
        };
        let encoding = self
            .query_catalog::<Option<String>>(database, redis::cmd("OBJECT").arg("ENCODING").arg(key))
            .await
            .ok()
            .flatten();
        let length_cmd = match key_type {
            KeyType::String => Some("STRLEN"),
            KeyType::Hash => Some("HLEN"),
            KeyType::List => Some("LLEN"),
            KeyType::Set => Some("SCARD"),
            KeyType::Zset => Some("ZCARD"),
            KeyType::Stream => Some("XLEN"),
            KeyType::Other => None,
        };
        let length = match length_cmd {
            Some(c) => self.query_catalog::<Option<u64>>(database, redis::cmd(c).arg(key)).await.ok().flatten(),
            None => None,
        };
        Ok(KeyDetails {
            key: RString::from_bytes(key),
            key_type,
            type_name,
            ttl_ms: ttl(pttl),
            memory_bytes,
            encoding,
            length,
        })
    }

    /// Runs `pipe` after `WATCH`ing `key` and checking its item at `index`.
    async fn watched<T>(
        &self,
        database: u32,
        key: &[u8],
        check: impl FnOnce(Option<Vec<u8>>) -> DbResult<()>,
        index: i64,
        pipe: redis::Pipeline,
        then: impl FnOnce(Value) -> DbResult<T>,
    ) -> DbResult<T> {
        let mut conn = self.write_conn(database).await?;
        redis::cmd("WATCH").arg(key).query_async::<()>(&mut conn).await.map_err(map_error)?;
        let current: Option<Vec<u8>> =
            redis::cmd("LINDEX").arg(key).arg(index).query_async(&mut conn).await.map_err(map_error)?;
        if let Err(e) = check(current) {
            let _ = redis::cmd("UNWATCH").query_async::<()>(&mut conn).await;
            return Err(e);
        }
        let reply: Value = pipe.query_async(&mut conn).await.map_err(map_error)?;
        // `EXEC` ended the `WATCH`.
        self.keep_write_conn(database, conn);
        then(reply)
    }
}

#[async_trait]
impl KeyValueStore for RedisSession {
    fn features(&self) -> KeyValueFeatures {
        KeyValueFeatures { databases: self.databases, memory_usage: self.memory_usage }
    }

    async fn scan(&self, request: &ScanRequest) -> DbResult<ScanPage> {
        let started = Instant::now();
        let cursor = request.cursor.clone().unwrap_or_else(|| "0".into());
        let (next, keys) = self.scan_step(request, &cursor).await?;
        let keys = self.entries(request.database, keys).await?;
        Ok(ScanPage { keys, cursor: (next != "0").then_some(next), duration_ms: started.elapsed().as_millis() as u64 })
    }

    async fn key_count(&self, database: u32) -> DbResult<u64> {
        self.query_catalog(database, &redis::cmd("DBSIZE")).await
    }

    async fn value(&self, request: &ValueRequest) -> DbResult<ValuePage> {
        let started = Instant::now();
        let db = request.key.database;
        let key = request.key.key.to_bytes()?;
        let details = self.details(db, &key).await?;
        let count = request.count();
        let cursor = request.cursor.clone();
        let index = || cursor.as_deref().and_then(|c| c.parse::<i64>().ok()).unwrap_or(0);
        let length = details.length.unwrap_or(0);
        let (value, next) = match details.key_type {
            KeyType::String => {
                let value: Vec<u8> = self
                    .query_catalog(db, redis::cmd("GETRANGE").arg(&key).arg(0).arg(MAX_STRING_BYTES as i64 - 1))
                    .await?;
                (
                    KeyValue::String {
                        value: RString::from_bytes(&value),
                        truncated: length > MAX_STRING_BYTES as u64,
                    },
                    None,
                )
            }
            KeyType::Hash | KeyType::Set => {
                let c = if details.key_type == KeyType::Hash { "HSCAN" } else { "SSCAN" };
                let (next, page): (String, Value) = self
                    .query_catalog(
                        db,
                        redis::cmd(c).arg(&key).arg(cursor.as_deref().unwrap_or("0")).arg("COUNT").arg(count),
                    )
                    .await?;
                let value = if details.key_type == KeyType::Hash {
                    KeyValue::Hash { entries: field_values(page) }
                } else {
                    KeyValue::Set { members: items(page).iter().map(rstring).collect() }
                };
                (value, (next != "0").then_some(next))
            }
            KeyType::List => {
                let start = index();
                let page: Vec<Vec<u8>> = self
                    .query_catalog(db, redis::cmd("LRANGE").arg(&key).arg(start).arg(start + i64::from(count) - 1))
                    .await?;
                let next = next_index(start, page.len(), count, length);
                let items = page
                    .iter()
                    .enumerate()
                    .map(|(i, v)| ListItem { index: start + i as i64, value: RString::from_bytes(v) })
                    .collect();
                (KeyValue::List { items }, next)
            }
            KeyType::Zset => {
                let start = index();
                let page: Value = self
                    .query_catalog(
                        db,
                        redis::cmd("ZRANGE").arg(&key).arg(start).arg(start + i64::from(count) - 1).arg("WITHSCORES"),
                    )
                    .await?;
                let members: Vec<ScoredMember> = items(page)
                    .chunks(2)
                    .filter(|p| p.len() == 2)
                    .map(|p| ScoredMember { member: rstring(&p[0]), score: text(&p[1]) })
                    .collect();
                let next = next_index(start, members.len(), count, length);
                (KeyValue::Zset { members }, next)
            }
            KeyType::Stream => {
                let start = cursor.clone().unwrap_or_else(|| "-".into());
                let page: Value = self
                    .query_catalog(db, redis::cmd("XRANGE").arg(&key).arg(&start).arg("+").arg("COUNT").arg(count))
                    .await?;
                let entries: Vec<StreamEntry> = items(page)
                    .into_iter()
                    .filter_map(|e| {
                        let mut parts = items(e).into_iter();
                        let id = text(&parts.next()?);
                        Some(StreamEntry { id, fields: parts.next().map(field_values).unwrap_or_default() })
                    })
                    .collect();
                let next =
                    (entries.len() as u32 >= count).then(|| entries.last().map(|e| format!("({}", e.id))).flatten();
                (KeyValue::Stream { entries }, next)
            }
            KeyType::Other => {
                let text = if details.type_name == "ReJSON-RL" {
                    self.query_catalog::<Option<String>>(db, redis::cmd("JSON.GET").arg(&key)).await.ok().flatten()
                } else {
                    None
                };
                (KeyValue::Other { text }, None)
            }
        };
        Ok(ValuePage { details, value, next, duration_ms: started.elapsed().as_millis() as u64 })
    }

    async fn write(&self, write: &KeyWrite) -> DbResult<()> {
        self.write_guard()?;
        let db = write.key.database;
        let key = write.key.key.to_bytes()?;
        if write.create && self.query_catalog::<i64>(db, redis::cmd("EXISTS").arg(&key)).await? > 0 {
            return Err(DbError::new(
                ErrorKind::Constraint,
                format!("A key named {} already exists.", write.key.key.text),
            ));
        }
        let int = |db, cmd: redis::Cmd| async move { self.query_catalog::<i64>(db, &cmd).await };
        match &write.edit {
            KeyEdit::SetString { value } => {
                let mut cmd = redis::cmd("SET");
                cmd.arg(&key).arg(value.to_bytes()?);
                if write.create {
                    cmd.arg("NX");
                } else {
                    cmd.arg("KEEPTTL");
                }
                let reply: Value = self.query_catalog(db, &cmd).await?;
                if reply == Value::Nil {
                    return Err(DbError::new(ErrorKind::Constraint, "The key was created meanwhile. Reload."));
                }
            }
            KeyEdit::HashSet { field, value } => {
                let mut cmd = redis::cmd("HSET");
                cmd.arg(&key).arg(field.to_bytes()?).arg(value.to_bytes()?);
                int(db, cmd).await?;
            }
            KeyEdit::HashDelete { field } => {
                let mut cmd = redis::cmd("HDEL");
                cmd.arg(&key).arg(field.to_bytes()?);
                if int(db, cmd).await? == 0 {
                    return Err(DbError::not_found("The field no longer exists. Reload."));
                }
            }
            KeyEdit::ListSet { index, expected, value } => {
                let expected = expected.to_bytes()?;
                let mut pipe = redis::pipe();
                pipe.atomic().cmd("LSET").arg(&key).arg(*index).arg(value.to_bytes()?);
                self.watched(
                    db,
                    &key,
                    |current| {
                        if current.as_deref() == Some(expected.as_slice()) { Ok(()) } else { Err(changed("list")) }
                    },
                    *index,
                    pipe,
                    |reply| if reply == Value::Nil { Err(changed("list")) } else { Ok(()) },
                )
                .await?;
            }
            KeyEdit::ListRemove { index, expected } => {
                let expected = expected.to_bytes()?;
                let nanos =
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
                let marker = format!("__dataquery_removed_{nanos}_{}", std::process::id());
                let mut pipe = redis::pipe();
                pipe.atomic().cmd("LSET").arg(&key).arg(*index).arg(&marker).cmd("LREM").arg(&key).arg(1).arg(&marker);
                self.watched(
                    db,
                    &key,
                    |current| {
                        if current.as_deref() == Some(expected.as_slice()) { Ok(()) } else { Err(changed("list")) }
                    },
                    *index,
                    pipe,
                    |reply| if reply == Value::Nil { Err(changed("list")) } else { Ok(()) },
                )
                .await?;
            }
            KeyEdit::ListPush { value, head } => {
                let mut cmd = redis::cmd(if *head { "LPUSH" } else { "RPUSH" });
                cmd.arg(&key).arg(value.to_bytes()?);
                int(db, cmd).await?;
            }
            KeyEdit::SetAdd { member } => {
                let mut cmd = redis::cmd("SADD");
                cmd.arg(&key).arg(member.to_bytes()?);
                int(db, cmd).await?;
            }
            KeyEdit::SetRemove { member } | KeyEdit::ZsetRemove { member } => {
                let c = if matches!(write.edit, KeyEdit::SetRemove { .. }) { "SREM" } else { "ZREM" };
                let mut cmd = redis::cmd(c);
                cmd.arg(&key).arg(member.to_bytes()?);
                if int(db, cmd).await? == 0 {
                    return Err(DbError::not_found("The member no longer exists. Reload."));
                }
            }
            KeyEdit::ZsetAdd { member, score } => {
                if !valid_score(score) {
                    return Err(DbError::invalid(format!("`{score}` is not a score: use a number, inf or -inf.")));
                }
                let mut cmd = redis::cmd("ZADD");
                cmd.arg(&key).arg(score.trim()).arg(member.to_bytes()?);
                int(db, cmd).await?;
            }
            KeyEdit::StreamAdd { fields } => {
                if fields.is_empty() {
                    return Err(DbError::invalid("A stream entry needs at least one field."));
                }
                let mut cmd = redis::cmd("XADD");
                cmd.arg(&key).arg("*");
                for (f, v) in fields {
                    cmd.arg(f.to_bytes()?).arg(v.to_bytes()?);
                }
                self.query_catalog::<Value>(db, &cmd).await?;
            }
            KeyEdit::StreamDelete { id } => {
                let mut cmd = redis::cmd("XDEL");
                cmd.arg(&key).arg(id);
                if int(db, cmd).await? == 0 {
                    return Err(DbError::not_found("The entry no longer exists. Reload."));
                }
            }
            KeyEdit::Expire { ttl_ms: Some(ms) } => {
                if *ms <= 0 {
                    return Err(DbError::invalid(
                        "The time to live must be positive. Use \"No expiry\" to keep the key.",
                    ));
                }
                let mut cmd = redis::cmd("PEXPIRE");
                cmd.arg(&key).arg(*ms);
                if int(db, cmd).await? == 0 {
                    return Err(gone());
                }
            }
            KeyEdit::Expire { ttl_ms: None } => {
                if int(db, redis::cmd("EXISTS").arg(&key).clone()).await? == 0 {
                    return Err(gone());
                }
                int(db, redis::cmd("PERSIST").arg(&key).clone()).await?;
            }
            KeyEdit::Rename { new_key } => {
                let target = new_key.to_bytes()?;
                match self.query_catalog::<i64>(db, redis::cmd("RENAMENX").arg(&key).arg(&target)).await {
                    Ok(0) => {
                        return Err(DbError::new(
                            ErrorKind::Constraint,
                            format!("A key named {} already exists.", new_key.text),
                        ));
                    }
                    Ok(_) => {}
                    Err(e) if e.message.contains("no such key") => return Err(gone()),
                    Err(e) => return Err(e),
                }
            }
            KeyEdit::Delete => {
                if int(db, redis::cmd("UNLINK").arg(&key).clone()).await? == 0 {
                    return Err(gone());
                }
            }
        }
        Ok(())
    }

    async fn console(&self, line: &str, timeout_secs: u32) -> DbResult<ConsoleReply> {
        let started = Instant::now();
        self.guard(line)?;
        let args = parse_line(line)?;
        let (reply, database) = self.run_query(&args, timeout_secs).await?;
        let (text, error) = match reply {
            Ok(value) => (format_cli(&value), false),
            Err(e) => {
                let detail = e.detail().map(|d| format!(" {d}")).unwrap_or_default();
                (format!("(error) {}{detail}", e.code().unwrap_or("ERR")), true)
            }
        };
        Ok(ConsoleReply { text, database, error, duration_ms: started.elapsed().as_millis() as u64 })
    }

    async fn stream_keys(&self, request: &ScanRequest, on_batch: &mut BatchFn<'_>) -> DbResult<u64> {
        let columns = [("key", false), ("type", false), ("ttl_ms", true)]
            .map(|(name, numeric)| ColumnMeta { name: name.into(), type_name: String::new(), numeric })
            .to_vec();
        let mut cursor = "0".to_owned();
        let mut batch = Vec::new();
        let mut total = 0u64;
        loop {
            let (next, keys) = self.scan_step(request, &cursor).await?;
            for e in self.entries(request.database, keys).await? {
                batch.push(vec![
                    Cell::text(e.key.text, CellLimits::FULL),
                    Cell::text(e.type_name, CellLimits::FULL),
                    e.ttl_ms.map_or(Cell::Null, |t| Cell::number(t.to_string())),
                ]);
                if batch.len() >= EXPORT_BATCH {
                    total += batch.len() as u64;
                    on_batch(&columns, std::mem::take(&mut batch))?;
                }
            }
            if next == "0" {
                break;
            }
            cursor = next;
        }
        total += batch.len() as u64;
        on_batch(&columns, batch)?;
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores() {
        for ok in ["1", "-2.5", "1e3", "inf", "-INF", "+inf"] {
            assert!(valid_score(ok), "{ok}");
        }
        for bad in ["", "abc", "nan", "1,5"] {
            assert!(!valid_score(bad), "{bad}");
        }
    }

    #[test]
    fn index_cursors() {
        assert_eq!(next_index(0, 10, 10, 25), Some("10".into()));
        assert_eq!(next_index(20, 5, 10, 25), None);
        assert_eq!(next_index(10, 10, 10, 20), None);
    }

    #[test]
    fn pairs_from_replies() {
        let v = Value::Array(vec![Value::BulkString(b"f".to_vec()), Value::BulkString(vec![0xff])]);
        let pairs = field_values(v);
        assert_eq!(pairs[0].field, RString::text("f"));
        assert!(pairs[0].value.binary);
    }
}
