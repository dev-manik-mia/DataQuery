//! Redis command lines: `redis-cli` argument splitting and what a command does.
//!
//! One line is one command. Classification uses a table of read-only commands:
//! anything not listed may write. Dangerous server commands (`FLUSHALL`,
//! `DEBUG`, `KEYS`, …) are destructive so they always ask first.

use super::analyze::{DestructiveReason, StatementKind};
use crate::error::{DbError, DbResult};

/// Splits a line like `redis-cli` does: whitespace separates arguments; `"…"`
/// takes `\n \r \t \b \a \\ \" \xHH` escapes; `'…'` takes `\'`. A closing
/// quote must end the argument.
pub fn split_args(line: &str) -> DbResult<Vec<Vec<u8>>> {
    let b = line.as_bytes();
    let mut args = Vec::new();
    let mut i = 0;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            return Ok(args);
        }
        let mut arg = Vec::new();
        let mut quote: Option<u8> = None;
        loop {
            let Some(&c) = b.get(i) else {
                if quote.is_some() {
                    return Err(DbError::invalid("A quoted argument is not closed."));
                }
                break;
            };
            match quote {
                None if c.is_ascii_whitespace() => break,
                None if c == b'"' || c == b'\'' => {
                    quote = Some(c);
                    i += 1;
                }
                None => {
                    arg.push(c);
                    i += 1;
                }
                Some(b'"') if c == b'\\' => {
                    let (byte, used) = match b.get(i + 1) {
                        Some(b'x') => {
                            let hex = line.get(i + 2..i + 4).and_then(|h| u8::from_str_radix(h, 16).ok());
                            (hex.ok_or_else(|| DbError::invalid("Write escaped bytes as \\xHH."))?, 4)
                        }
                        Some(b'n') => (b'\n', 2),
                        Some(b'r') => (b'\r', 2),
                        Some(b't') => (b'\t', 2),
                        Some(b'b') => (8, 2),
                        Some(b'a') => (7, 2),
                        Some(&other) => (other, 2),
                        None => return Err(DbError::invalid("A quoted argument is not closed.")),
                    };
                    arg.push(byte);
                    i += used;
                }
                Some(b'\'') if c == b'\\' && b.get(i + 1) == Some(&b'\'') => {
                    arg.push(b'\'');
                    i += 2;
                }
                Some(q) if c == q => {
                    i += 1;
                    if b.get(i).is_some_and(|n| !n.is_ascii_whitespace()) {
                        return Err(DbError::invalid("A closing quote must be followed by a space."));
                    }
                    break;
                }
                Some(_) => {
                    arg.push(c);
                    i += 1;
                }
            }
        }
        args.push(arg);
    }
}

/// Commands that never change data, keys or server state.
const READ_ONLY: &[&str] = &[
    "GET", "MGET", "GETRANGE", "SUBSTR", "STRLEN", "EXISTS", "TYPE", "TTL", "PTTL", "EXPIRETIME", "PEXPIRETIME",
    "SCAN", "HSCAN", "SSCAN", "ZSCAN", "RANDOMKEY", "DBSIZE", "HGET", "HMGET", "HGETALL", "HKEYS", "HVALS", "HLEN",
    "HEXISTS", "HSTRLEN", "HRANDFIELD", "HTTL", "HPTTL", "LRANGE", "LLEN", "LINDEX", "LPOS", "SMEMBERS", "SISMEMBER",
    "SMISMEMBER", "SCARD", "SRANDMEMBER", "SINTER", "SUNION", "SDIFF", "SINTERCARD", "ZRANGE", "ZRANGEBYSCORE",
    "ZREVRANGEBYSCORE", "ZRANGEBYLEX", "ZREVRANGEBYLEX", "ZREVRANGE", "ZCARD", "ZSCORE", "ZMSCORE", "ZRANK",
    "ZREVRANK", "ZCOUNT", "ZLEXCOUNT", "ZRANDMEMBER", "ZINTER", "ZUNION", "ZDIFF", "ZINTERCARD", "XRANGE",
    "XREVRANGE", "XLEN", "XREAD", "XINFO", "XPENDING", "BITCOUNT", "BITPOS", "GETBIT", "BITFIELD_RO", "PFCOUNT",
    "GEOPOS", "GEODIST", "GEOHASH", "GEOSEARCH", "GEORADIUS_RO", "GEORADIUSBYMEMBER_RO", "LCS", "DUMP", "OBJECT",
    "MEMORY", "INFO", "PING", "ECHO", "TIME", "LASTSAVE", "ROLE", "COMMAND", "LOLWUT", "HELLO", "SELECT", "AUTH",
    "SLOWLOG", "LATENCY", "PUBSUB", "WAIT", "WAITAOF", "QUIT", "RESET", "READONLY", "READWRITE",
    "TOUCH", "SORT_RO", "EVAL_RO", "EVALSHA_RO", "FCALL_RO", "JSON.GET", "JSON.MGET", "JSON.TYPE", "JSON.STRLEN",
    "JSON.ARRLEN", "JSON.OBJKEYS", "JSON.OBJLEN", "TS.GET", "TS.MGET", "TS.RANGE", "TS.REVRANGE", "TS.INFO",
    "FT.SEARCH", "FT.AGGREGATE", "FT.INFO", "FT._LIST",
];

/// Subcommands of these commands that only read.
const READ_ONLY_SUBCOMMANDS: &[(&str, &[&str])] = &[
    ("CONFIG", &["GET"]),
    ("CLIENT", &["LIST", "INFO", "GETNAME", "ID", "TRACKINGINFO", "GETREDIR", "NO-EVICT", "NO-TOUCH", "SETNAME"]),
    ("SCRIPT", &["EXISTS"]),
    ("FUNCTION", &["LIST", "STATS", "DUMP"]),
    ("CLUSTER", &["INFO", "NODES", "SHARDS", "SLOTS", "MYID", "KEYSLOT", "COUNTKEYSINSLOT", "GETKEYSINSLOT"]),
    ("ACL", &["WHOAMI", "LIST", "USERS", "GETUSER", "CAT", "LOG"]),
    ("MODULE", &["LIST"]),
];

/// Commands that delete everything, change or stop the server, or block it.
fn danger(name: &str, sub: &str) -> Option<DestructiveReason> {
    Some(match (name, sub) {
        ("FLUSHALL" | "FLUSHDB", _) => DestructiveReason::FlushData,
        ("KEYS", _) => DestructiveReason::BlockingKeys,
        ("DEBUG" | "SHUTDOWN" | "MONITOR" | "SWAPDB" | "MIGRATE" | "REPLICAOF" | "SLAVEOF" | "FAILOVER" | "SAVE", _)
        | ("CONFIG", "SET" | "RESETSTAT" | "REWRITE")
        | ("SCRIPT" | "FUNCTION", "FLUSH" | "DELETE" | "KILL")
        | ("CLUSTER", "RESET" | "FAILOVER" | "FORGET" | "FLUSHSLOTS" | "DELSLOTS" | "SETSLOT")
        | ("CLIENT", "KILL" | "PAUSE")
        | ("ACL", "DELUSER" | "SETUSER" | "LOAD")
        | ("MODULE", "UNLOAD" | "LOAD") => DestructiveReason::ServerCommand,
        _ => return None,
    })
}

/// What one command line does.
pub(super) fn classify(line: &str) -> (StatementKind, bool, Option<DestructiveReason>) {
    let Ok(args) = split_args(line) else {
        return (StatementKind::Other, true, None);
    };
    let word = |i: usize| args.get(i).map(|a| String::from_utf8_lossy(a).to_ascii_uppercase()).unwrap_or_default();
    let (name, sub) = (word(0), word(1));
    if let Some(reason) = danger(&name, &sub) {
        let kind = if reason == DestructiveReason::BlockingKeys { StatementKind::Select } else { StatementKind::Other };
        return (kind, reason != DestructiveReason::BlockingKeys, Some(reason));
    }
    let kind = match name.as_str() {
        "MULTI" | "EXEC" | "DISCARD" | "WATCH" | "UNWATCH" => return (StatementKind::Transaction, false, None),
        "SELECT" => StatementKind::Use,
        "DEL" | "UNLINK" | "HDEL" | "SREM" | "ZREM" | "LREM" | "XDEL" | "XTRIM" | "GETDEL" | "LPOP" | "RPOP" | "SPOP"
        | "ZPOPMIN" | "ZPOPMAX" => StatementKind::Delete,
        "SET" | "MSET" | "HSET" | "HMSET" | "LPUSH" | "RPUSH" | "SADD" | "ZADD" | "XADD" | "APPEND" | "SETNX"
        | "SETEX" | "PSETEX" | "MSETNX" | "HSETNX" => StatementKind::Insert,
        _ => StatementKind::Other,
    };
    let read = READ_ONLY.contains(&name.as_str())
        || READ_ONLY_SUBCOMMANDS.iter().any(|(c, subs)| *c == name && subs.contains(&sub.as_str()));
    let writes = !read;
    let kind = if kind == StatementKind::Other && !writes { StatementKind::Select } else { kind };
    (kind, writes, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::{SqlFlavor, split_statements};

    #[test]
    fn splits_like_redis_cli() {
        let args = split_args(r#"SET "a b" 'it\'s' "\x00\xff\n" plain"#).unwrap();
        assert_eq!(args, [b"SET".to_vec(), b"a b".to_vec(), b"it's".to_vec(), vec![0, 0xff, b'\n'], b"plain".to_vec()]);
        assert!(split_args(r#"GET "open"#).is_err());
        assert!(split_args(r#"GET "a"b"#).is_err());
        assert!(split_args("   ").unwrap().is_empty());
    }

    #[test]
    fn classifies_commands() {
        let one = |s: &str| split_statements(s, SqlFlavor::Redis).remove(0);
        assert!(!one("hgetall user:1").writes);
        assert!(!one("CONFIG GET maxmemory").writes);
        assert!(one("CONFIG SET maxmemory 1mb").destructive.is_some());
        assert!(one("HSET user:1 plan pro").writes);
        assert_eq!(one("flushdb").destructive, Some(DestructiveReason::FlushData));
        assert_eq!(one("KEYS *").destructive, Some(DestructiveReason::BlockingKeys));
        assert!(!one("KEYS *").writes);
        assert!(one("DEBUG SLEEP 1").destructive.is_some());
        assert!(one("CLIENT KILL ID 4").destructive.is_some());
        assert!(!one("CLIENT LIST").writes);
        assert_eq!(one("MULTI").kind, StatementKind::Transaction);
        assert!(one("EVAL \"return 1\" 0").writes);
    }

    #[test]
    fn one_command_per_line() {
        let s = split_statements("GET a\n\n  SET b \"x;y\"\nINFO server", SqlFlavor::Redis);
        assert_eq!(s.len(), 3);
        assert!(s[1].writes);
    }
}
