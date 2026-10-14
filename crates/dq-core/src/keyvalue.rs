//! Key-value engines (Redis, Valkey): the key browser of screen 12.
//!
//! Keys, fields and members are binary-safe. They travel as [`RString`]: plain
//! text when the bytes are UTF-8, otherwise an escaped form (`\xNN`, `\\`)
//! that converts back to the exact bytes. Browsing always uses `SCAN`-family
//! cursors; `KEYS` is never sent.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::adapter::BatchFn;
use crate::error::{DbError, DbResult};

/// Largest number of keys one scan step may request (`COUNT`).
pub const MAX_SCAN_COUNT: u32 = 5_000;
/// Most elements read per page of a value (hash fields, list items, …).
pub const MAX_VALUE_PAGE: u32 = 1_000;
/// Largest string value read for the viewer; longer values are cut and read-only.
pub const MAX_STRING_BYTES: usize = 4 * 1024 * 1024;

/// A binary-safe string. `binary` marks an escaped form of non-UTF-8 bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RString {
    pub text: String,
    #[serde(default)]
    pub binary: bool,
}

impl RString {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        match std::str::from_utf8(bytes) {
            Ok(text) => Self { text: text.to_owned(), binary: false },
            Err(_) => Self { text: escape(bytes), binary: true },
        }
    }

    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into(), binary: false }
    }

    /// The exact bytes.
    pub fn to_bytes(&self) -> DbResult<Vec<u8>> {
        if self.binary { unescape(&self.text) } else { Ok(self.text.as_bytes().to_vec()) }
    }
}

/// `\xNN` for bytes outside printable ASCII, `\\` for a backslash.
pub fn escape(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(b as char),
            _ => {
                let _ = write!(out, "\\x{b:02x}");
            }
        }
    }
    out
}

pub fn unescape(text: &str) -> DbResult<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            Some(b'\\') => {
                out.push(b'\\');
                i += 2;
            }
            Some(b'x') => {
                let hex = text.get(i + 2..i + 4).and_then(|h| u8::from_str_radix(h, 16).ok());
                out.push(hex.ok_or_else(|| DbError::invalid("Escaped bytes are written as \\xNN."))?);
                i += 4;
            }
            _ => return Err(DbError::invalid("A backslash starts \\xNN or \\\\ in binary text.")),
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum KeyType {
    String,
    Hash,
    List,
    Set,
    Zset,
    Stream,
    /// A module type (`RedisJSON`, time series, …), shown read-only.
    Other,
}

impl KeyType {
    /// From the `TYPE` reply.
    pub fn from_type_name(name: &str) -> Self {
        match name {
            "string" => Self::String,
            "hash" => Self::Hash,
            "list" => Self::List,
            "set" => Self::Set,
            "zset" => Self::Zset,
            "stream" => Self::Stream,
            _ => Self::Other,
        }
    }

    /// The `SCAN … TYPE` argument.
    pub fn type_name(self) -> Option<&'static str> {
        Some(match self {
            Self::String => "string",
            Self::Hash => "hash",
            Self::List => "list",
            Self::Set => "set",
            Self::Zset => "zset",
            Self::Stream => "stream",
            Self::Other => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct KeyRef {
    pub database: u32,
    pub key: RString,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
pub struct KeyEntry {
    pub key: RString,
    pub key_type: KeyType,
    /// `TYPE` reply, e.g. `ReJSON-RL` for module types.
    pub type_name: String,
    /// Remaining time to live; `None` when the key does not expire.
    #[ts(type = "number | null")]
    pub ttl_ms: Option<i64>,
}

/// One `SCAN` step. `cursor` continues a scan; `None` starts one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ScanRequest {
    pub database: u32,
    /// Glob pattern for `MATCH`; empty means every key.
    #[serde(default)]
    pub pattern: String,
    #[serde(default)]
    pub key_type: Option<KeyType>,
    #[serde(default)]
    pub cursor: Option<String>,
    /// `COUNT` hint, at most [`MAX_SCAN_COUNT`].
    pub count: u32,
}

impl ScanRequest {
    pub fn count(&self) -> u32 {
        self.count.clamp(1, MAX_SCAN_COUNT)
    }

    /// `MATCH` argument.
    pub fn pattern(&self) -> &str {
        if self.pattern.trim().is_empty() { "*" } else { &self.pattern }
    }
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ScanPage {
    pub keys: Vec<KeyEntry>,
    /// Cursor to continue; `None` when the scan is complete.
    pub cursor: Option<String>,
    #[ts(type = "number")]
    pub duration_ms: u64,
}

/// Facts about one key, from `TYPE`, `PTTL`, `MEMORY USAGE` and `OBJECT ENCODING`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
pub struct KeyDetails {
    pub key: RString,
    pub key_type: KeyType,
    pub type_name: String,
    #[ts(type = "number | null")]
    pub ttl_ms: Option<i64>,
    #[ts(type = "number | null")]
    pub memory_bytes: Option<i64>,
    pub encoding: Option<String>,
    /// Fields, items, members or entries; bytes for strings.
    #[ts(type = "number | null")]
    pub length: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
pub struct FieldValue {
    pub field: RString,
    pub value: RString,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
pub struct ListItem {
    #[ts(type = "number")]
    pub index: i64,
    pub value: RString,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
pub struct ScoredMember {
    pub member: RString,
    /// Exact as the server prints it, e.g. `1.5` or `inf`.
    pub score: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
pub struct StreamEntry {
    pub id: String,
    pub fields: Vec<FieldValue>,
}

/// One page of a key's value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum KeyValue {
    /// `truncated` when longer than [`MAX_STRING_BYTES`]; then it cannot be edited here.
    String { value: RString, truncated: bool },
    Hash { entries: Vec<FieldValue> },
    List { items: Vec<ListItem> },
    Set { members: Vec<RString> },
    Zset { members: Vec<ScoredMember> },
    Stream { entries: Vec<StreamEntry> },
    /// Module types: the server's text form when one is available.
    Other { text: Option<String> },
}

/// Which page of a value to read. `cursor` continues from [`ValuePage::next`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ValueRequest {
    pub key: KeyRef,
    #[serde(default)]
    pub cursor: Option<String>,
    pub count: u32,
}

impl ValueRequest {
    pub fn count(&self) -> u32 {
        self.count.clamp(1, MAX_VALUE_PAGE)
    }
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ValuePage {
    pub details: KeyDetails,
    pub value: KeyValue,
    /// Cursor for the next page of elements; `None` when this was the last.
    pub next: Option<String>,
    #[ts(type = "number")]
    pub duration_ms: u64,
}

/// One change to a key from the key browser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "op", rename_all = "snake_case")]
#[ts(export)]
pub enum KeyEdit {
    SetString { value: RString },
    HashSet { field: RString, value: RString },
    HashDelete { field: RString },
    /// `LSET`; fails when the item at `index` is no longer `expected`.
    ListSet {
        #[ts(type = "number")]
        index: i64,
        expected: RString,
        value: RString,
    },
    ListPush { value: RString, head: bool },
    /// Removes the item at `index` if it is still `expected`.
    ListRemove {
        #[ts(type = "number")]
        index: i64,
        expected: RString,
    },
    SetAdd { member: RString },
    SetRemove { member: RString },
    ZsetAdd { member: RString, score: String },
    ZsetRemove { member: RString },
    /// `XADD key * field value …`.
    StreamAdd { fields: Vec<(RString, RString)> },
    StreamDelete { id: String },
    /// `PEXPIRE`, or `PERSIST` when `None`.
    Expire {
        #[ts(type = "number | null")]
        ttl_ms: Option<i64>,
    },
    /// `RENAMENX`: refuses to overwrite an existing key.
    Rename { new_key: RString },
    /// `UNLINK`.
    Delete,
}

impl KeyEdit {
    pub fn is_delete(&self) -> bool {
        matches!(self, Self::Delete)
    }
}

/// A key change. `create` refuses to touch a key that already exists (new keys).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct KeyWrite {
    pub key: KeyRef,
    pub edit: KeyEdit,
    #[serde(default)]
    pub create: bool,
}

/// A console reply printed like `redis-cli` (`(integer) 1`, `1) "a"`, `(nil)`).
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ConsoleReply {
    pub text: String,
    /// The database selected after the command (it changes with `SELECT`).
    pub database: u32,
    pub error: bool,
    #[ts(type = "number")]
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, TS)]
#[ts(export)]
pub struct KeyValueFeatures {
    /// Number of logical databases (`SELECT 0…n-1`); 1 for clusters and servers that hide it.
    pub databases: u32,
    /// `MEMORY USAGE` is available.
    pub memory_usage: bool,
}

/// Key browser operations. Reads use catalog connections; the console uses the
/// session's query connection, so `SELECT` and `MULTI` persist like in `redis-cli`.
#[async_trait]
pub trait KeyValueStore: Send + Sync {
    fn features(&self) -> KeyValueFeatures;

    async fn scan(&self, request: &ScanRequest) -> DbResult<ScanPage>;

    /// `DBSIZE` of a database.
    async fn key_count(&self, database: u32) -> DbResult<u64>;

    async fn value(&self, request: &ValueRequest) -> DbResult<ValuePage>;

    /// Applies one change; fails when the key or element changed in the meantime.
    async fn write(&self, write: &KeyWrite) -> DbResult<()>;

    /// Runs one command line on the query connection and prints the reply.
    async fn console(&self, line: &str, timeout_secs: u32) -> DbResult<ConsoleReply>;

    /// Streams matching keys (key, type, ttl) for exports.
    async fn stream_keys(&self, request: &ScanRequest, on_batch: &mut BatchFn<'_>) -> DbResult<u64>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_strings_round_trip() {
        let bytes = [b'a', 0, 0xff, b'\\', b'z'];
        let r = RString::from_bytes(&bytes);
        assert_eq!(r, RString { text: "a\\x00\\xff\\\\z".into(), binary: true });
        assert_eq!(r.to_bytes().unwrap(), bytes);
        let plain = RString::from_bytes("Zoë \\ ok".as_bytes());
        assert!(!plain.binary);
        assert_eq!(plain.to_bytes().unwrap(), "Zoë \\ ok".as_bytes());
        assert!(unescape("\\q").is_err());
        assert!(unescape("\\x4").is_err());
    }

    #[test]
    fn scan_defaults() {
        let r = ScanRequest { database: 0, pattern: " ".into(), key_type: None, cursor: None, count: 0 };
        assert_eq!((r.pattern(), r.count()), ("*", 1));
        assert_eq!(KeyType::from_type_name("ReJSON-RL"), KeyType::Other);
        assert_eq!(KeyType::Zset.type_name(), Some("zset"));
    }
}
