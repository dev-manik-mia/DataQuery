//! Data compare: the rows of a source and a target table, possibly on
//! different connections and engines, and the SQL that makes the target match
//! the source.
//!
//! Both sides stream ordered by the key and are merge-joined here, so memory
//! stays bounded whatever the table size: counts are always complete, the
//! differing rows kept for display are capped, and every change goes to a
//! [`ChangeSink`] (a journal file in the app). The sync SQL is rendered from
//! the journal for review, or applied from it with bound parameters.

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use ts_rs::TS;

use crate::adapter::Session;
use crate::edit::{ApplyReport, BoundStatement, ColumnValue, RowChange};
use crate::error::{DbError, DbResult, ErrorKind};
use crate::node::{NodeKind, ObjectRef};
use crate::result::Row;
use crate::schema::{ColumnInfo, TableStructure};
use crate::sql::{Dialect, SqlFlavor, builder};
use crate::table::RowFilter;
use crate::transfer::TransferControl;
use crate::value::{Cell, CellLimits};

/// Most differing rows kept for display.
pub const MAX_EXAMPLES: u32 = 10_000;
/// Rows per multi-row `INSERT` in the sync SQL (lower where the engine needs it).
const INSERT_BATCH_ROWS: usize = 500;
/// Batches buffered per side while the merge catches up.
const CHANNEL_BATCHES: usize = 4;
/// Cell size kept in displayed examples.
const EXAMPLE_LIMITS: CellLimits = CellLimits { max_text_bytes: 1024, binary_head_bytes: 32 };

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(default)]
#[ts(export)]
pub struct DataCompareOptions {
    /// Columns that identify a row on both sides. Empty uses the source's
    /// primary key, else a unique key.
    pub key_columns: Vec<String>,
    /// Columns compared besides the key. Empty compares every column both tables have.
    pub columns: Vec<String>,
    /// Rows compared, on both sides (the visual filter builder).
    pub rows: RowFilter,
    /// Text that differs only in leading, trailing or repeated whitespace is equal.
    pub ignore_whitespace: bool,
    pub ignore_case: bool,
    /// Numbers at most this far apart are equal; 0 compares exactly.
    pub float_tolerance: f64,
    /// Differing rows kept for display. Counts are always complete.
    pub max_examples: u32,
}

impl Default for DataCompareOptions {
    fn default() -> Self {
        Self {
            key_columns: Vec::new(),
            columns: Vec::new(),
            rows: RowFilter::default(),
            ignore_whitespace: false,
            ignore_case: false,
            float_tolerance: 0.0,
            max_examples: 1_000,
        }
    }
}

/// A column both tables have.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct CompareColumn {
    /// Name in the source table.
    pub name: String,
    /// Name in the target table (it may differ in case).
    pub target_name: String,
    pub source_type: String,
    pub target_type: String,
    pub key: bool,
    /// Compared; key columns always are.
    pub compared: bool,
    /// The types belong to different families; values are compared after normalisation.
    pub type_mismatch: bool,
    /// The sync SQL writes this column (it is editable on the target).
    pub synced: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum KeySource {
    Primary,
    Unique,
    Chosen,
}

/// What will be compared: shown before the compare runs, so the user can pick
/// key and compared columns.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ComparePlan {
    /// Every column both tables have: key columns first (in key order), then the others in source order.
    pub columns: Vec<CompareColumn>,
    pub key_source: KeySource,
    pub only_in_source: Vec<String>,
    pub only_in_target: Vec<String>,
}

impl ComparePlan {
    /// Columns read from each side, keys first.
    fn selected(&self) -> impl Iterator<Item = &CompareColumn> {
        self.columns.iter().filter(|c| c.compared)
    }

    fn keys(&self) -> usize {
        self.columns.iter().filter(|c| c.key).count()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum RowDiffKind {
    OnlyInSource,
    OnlyInTarget,
    Different,
}

/// One differing row, for display.
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
#[ts(export)]
pub struct RowDiff {
    pub kind: RowDiffKind,
    /// Values in [`DataDiff::columns`] order; empty when the row is only in the target.
    pub source: Vec<Cell>,
    /// Empty when the row is only in the source.
    pub target: Vec<Cell>,
    /// Indexes of the columns that differ.
    pub changed: Vec<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
pub struct CompareCounts {
    #[ts(type = "number")]
    pub identical: u64,
    #[ts(type = "number")]
    pub only_in_source: u64,
    #[ts(type = "number")]
    pub only_in_target: u64,
    #[ts(type = "number")]
    pub different: u64,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct DataDiff {
    /// Compared columns (source names), keys first.
    pub columns: Vec<String>,
    pub keys: u32,
    pub counts: CompareCounts,
    /// The first differing rows, at most `max_examples`.
    pub examples: Vec<RowDiff>,
    /// More rows differ than `examples` holds.
    pub examples_truncated: bool,
    #[ts(type = "number")]
    pub duration_ms: u64,
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

/// Refuses engines whose tables are not SQL tables with an orderable key.
pub fn check_engine(session: &dyn Session) -> DbResult<()> {
    let flavor = session.dialect().flavor();
    if session.documents().is_some() || session.keys().is_some() || flavor.is_document() || flavor == SqlFlavor::Cql {
        return Err(DbError::unsupported("Data compare works on tables of SQL databases."));
    }
    Ok(())
}

/// Broad type family, so `int4` and `int` match but `text` and `int` do not.
fn type_family(data_type: &str) -> String {
    let lower = data_type.trim().to_ascii_lowercase();
    // MySQL's BOOLEAN.
    if lower.starts_with("tinyint(1)") {
        return "bool".to_owned();
    }
    let base = lower.split(['(', '[', ' ']).next().unwrap_or_default();
    let sized_int = |b: &str| {
        b.trim_start_matches('u')
            .strip_prefix("int")
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|c| c.is_ascii_digit()))
    };
    let family = match base {
        "bool" | "boolean" | "bit" => "bool",
        b if (b.ends_with("int") && b != "point") || b.ends_with("serial") || b == "integer" || sized_int(b) => {
            "integer"
        }
        "numeric" | "decimal" | "dec" | "number" | "money" | "smallmoney" | "real" | "double" => "number",
        b if b.starts_with("float") => "number",
        b if b.starts_with("timestamp") || b.starts_with("datetime") => "timestamp",
        "date" => "date",
        b if b.starts_with("time") => "time",
        "json" | "jsonb" => "json",
        "bytea" | "blob" | "tinyblob" | "mediumblob" | "longblob" | "binary" | "varbinary" | "raw" | "image" => {
            "binary"
        }
        b if b.contains("char") || b.contains("text") || b.contains("clob") => "text",
        "string" | "enum" | "uuid" | "name" => "text",
        other => other,
    };
    family.to_owned()
}

fn binary_type(data_type: &str) -> bool {
    type_family(data_type) == "binary"
}

/// Pairs the columns of both tables, resolves the key and the compared columns.
pub fn plan(
    source: &TableStructure,
    source_dialect: &dyn Dialect,
    target: &TableStructure,
    target_dialect: &dyn Dialect,
    options: &DataCompareOptions,
) -> DbResult<ComparePlan> {
    if target.object.kind != NodeKind::Table {
        return Err(DbError::invalid("The target must be a table."));
    }
    if !source.object.kind.is_relation() {
        return Err(DbError::invalid("The source must be a table or view."));
    }
    let pair = |s: &ColumnInfo| {
        target
            .columns
            .iter()
            .find(|t| t.name == s.name)
            .or_else(|| target.columns.iter().find(|t| t.name.eq_ignore_ascii_case(&s.name)))
    };
    let pairs: Vec<(&ColumnInfo, &ColumnInfo)> =
        source.columns.iter().filter_map(|s| pair(s).map(|t| (s, t))).collect();
    let paired = |name: &str| pairs.iter().any(|(s, _)| s.name == name);
    let missing = |names: &[String], what: &str| -> DbResult<()> {
        match names.iter().find(|n| !paired(n)) {
            Some(n) => Err(DbError::invalid(format!("{what} `{n}` is not in both tables."))),
            None => Ok(()),
        }
    };

    let primary = source.indexes.iter().find(|i| i.primary).map(|i| i.columns.clone()).unwrap_or_default();
    let (keys, key_source) = if !options.key_columns.is_empty() {
        missing(&options.key_columns, "Key column")?;
        (options.key_columns.clone(), KeySource::Chosen)
    } else if !source.row_identity.is_empty() && source.row_identity.iter().all(|k| paired(k)) {
        let kind = if source.row_identity == primary { KeySource::Primary } else { KeySource::Unique };
        (source.row_identity.clone(), kind)
    } else if let Some(unique) =
        source.indexes.iter().find(|i| i.unique && !i.columns.is_empty() && i.columns.iter().all(|c| paired(c)))
    {
        (unique.columns.clone(), if unique.primary { KeySource::Primary } else { KeySource::Unique })
    } else {
        return Err(DbError::invalid(
            "The source has no primary or unique key the target also has. Choose key columns that identify a row.",
        ));
    };
    for (i, k) in keys.iter().enumerate() {
        if keys[..i].contains(k) {
            return Err(DbError::invalid(format!("Key column `{k}` is chosen twice.")));
        }
        let (s, t) = pairs.iter().find(|(s, _)| &s.name == k).ok_or_else(|| DbError::internal("key vanished"))?;
        if !source_dialect.comparable(&s.data_type) || !target_dialect.comparable(&t.data_type) {
            return Err(DbError::invalid(format!("`{k}` ({}) cannot be part of a key.", s.data_type)));
        }
    }
    missing(&options.columns, "Column")?;

    let column = |(s, t): (&ColumnInfo, &ColumnInfo), key: bool| CompareColumn {
        name: s.name.clone(),
        target_name: t.name.clone(),
        source_type: s.data_type.clone(),
        target_type: t.data_type.clone(),
        key,
        compared: key || options.columns.is_empty() || options.columns.contains(&s.name),
        type_mismatch: type_family(&s.data_type) != type_family(&t.data_type),
        synced: t.editable && !binary_type(&t.data_type),
    };
    let mut columns: Vec<CompareColumn> =
        keys.iter().filter_map(|k| pairs.iter().find(|(s, _)| &s.name == k)).map(|&p| column(p, true)).collect();
    columns.extend(pairs.iter().filter(|(s, _)| !keys.contains(&s.name)).map(|&p| column(p, false)));
    let names = |list: &[ColumnInfo], other: &dyn Fn(&ColumnInfo) -> bool| {
        list.iter().filter(|c| !other(c)).map(|c| c.name.clone()).collect::<Vec<_>>()
    };
    Ok(ComparePlan {
        only_in_source: names(&source.columns, &|c| pair(c).is_some()),
        only_in_target: names(&target.columns, &|t| pairs.iter().any(|(_, p)| p.name == t.name)),
        columns,
        key_source,
    })
}

// ---------------------------------------------------------------------------
// Value normalisation
// ---------------------------------------------------------------------------

/// Canonical decimal text: no sign on zero, no leading or trailing zeros, no
/// exponent. `None` when `text` is not a finite number.
fn canonical_number(text: &str) -> Option<String> {
    let t = text.trim();
    let (negative, digits) = match t.as_bytes().first()? {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.contains(['e', 'E']) {
        let f: f64 = t.parse().ok().filter(|f: &f64| f.is_finite())?;
        // `Display` never uses an exponent.
        return canonical_number(&f.to_string());
    }
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    let digit = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if (int.is_empty() && frac.is_empty()) || !digit(int) || !digit(frac) {
        return None;
    }
    let int = match int.trim_start_matches('0') {
        "" => "0",
        rest => rest,
    };
    let frac = frac.trim_end_matches('0');
    let mut out = String::with_capacity(int.len() + frac.len() + 2);
    if negative && (int != "0" || !frac.is_empty()) {
        out.push('-');
    }
    out.push_str(int);
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    Some(out)
}

/// Orders two [`canonical_number`]s numerically.
fn cmp_decimal(a: &str, b: &str) -> Ordering {
    let (a_neg, a) = a.strip_prefix('-').map_or((false, a), |r| (true, r));
    let (b_neg, b) = b.strip_prefix('-').map_or((false, b), |r| (true, r));
    let magnitude = || {
        let (ai, af) = a.split_once('.').unwrap_or((a, ""));
        let (bi, bf) = b.split_once('.').unwrap_or((b, ""));
        ai.len().cmp(&bi.len()).then_with(|| ai.cmp(bi)).then_with(|| af.cmp(bf))
    };
    match (a_neg, b_neg) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        (false, false) => magnitude(),
        (true, true) => magnitude().reverse(),
    }
}

fn format_naive(dt: NaiveDateTime) -> String {
    let nanos = dt.nanosecond() % 1_000_000_000;
    let fraction = if nanos == 0 { String::new() } else { format!(".{nanos:09}").trim_end_matches('0').to_owned() };
    format!("{}{fraction}", dt.format("%Y-%m-%d %H:%M:%S"))
}

/// A zoned date-time as UTC wall-clock time, `YYYY-MM-DD HH:MM:SS[.fff]`.
fn utc_naive(text: &str) -> Option<String> {
    let t = text.trim();
    DateTime::parse_from_rfc3339(t)
        .or_else(|_| DateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S%.f%#z"))
        .ok()
        .map(|dt| format_naive(dt.naive_utc()))
}

/// Dates, times and timestamps in one spelling: zoned values in UTC, `T` or
/// space separators alike, no trailing fractional zeros. Values that do not
/// parse (e.g. `infinity`) are kept as they are.
fn canonical_temporal(text: &str) -> String {
    let t = text.trim();
    if let Some(utc) = utc_naive(t) {
        return utc;
    }
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(t, format) {
            return format_naive(dt);
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(t, "%Y-%m-%d") {
        return date.format("%Y-%m-%d").to_string();
    }
    if let Ok(time) = NaiveTime::parse_from_str(t, "%H:%M:%S%.f") {
        return format_naive(NaiveDate::default().and_time(time))[11..].to_owned();
    }
    t.to_owned()
}

/// The truth value of a boolean, a 0/1 number or boolean text.
fn truth(cell: &Cell) -> Option<bool> {
    match cell {
        Cell::Bool { value } => Some(*value),
        Cell::Number { value } => match canonical_number(value)?.as_str() {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        },
        Cell::Text { value, .. } => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "t" | "true" | "y" | "yes" | "on" => Some(true),
            "0" | "f" | "false" | "n" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// How two cells are judged equal.
#[derive(Debug, Clone, Copy, Default)]
pub struct Equality {
    pub ignore_whitespace: bool,
    pub ignore_case: bool,
    pub float_tolerance: f64,
}

impl From<&DataCompareOptions> for Equality {
    fn from(o: &DataCompareOptions) -> Self {
        Self {
            ignore_whitespace: o.ignore_whitespace,
            ignore_case: o.ignore_case,
            float_tolerance: if o.float_tolerance.is_finite() { o.float_tolerance.max(0.0) } else { 0.0 },
        }
    }
}

impl Equality {
    /// Equal after normalising across engines: numbers by value, booleans
    /// against 0/1, temporals in UTC, JSON as documents, binary by bytes.
    /// NULL equals only NULL.
    pub fn same(&self, a: &Cell, b: &Cell) -> bool {
        use Cell::{Binary, Bool, Json, Null, Number, Temporal, Text};
        match (a, b) {
            (Null, Null) => true,
            (Null, _) | (_, Null) => false,
            (Bool { .. }, _) | (_, Bool { .. }) => truth(a).is_some() && truth(a) == truth(b),
            (Number { value: x }, Number { value: y } | Text { value: y, .. })
            | (Text { value: x, .. }, Number { value: y }) => self.numbers(x, y),
            (Temporal { value: x, .. }, Temporal { value: y, .. } | Text { value: y, .. })
            | (Text { value: x, .. }, Temporal { value: y, .. }) => canonical_temporal(x) == canonical_temporal(y),
            (Json { value: x, .. }, Json { value: y, .. } | Text { value: y, .. })
            | (Text { value: x, .. }, Json { value: y, .. }) => {
                match (serde_json::from_str::<serde_json::Value>(x), serde_json::from_str::<serde_json::Value>(y)) {
                    (Ok(x), Ok(y)) => x == y,
                    _ => self.text(x, y),
                }
            }
            (Binary { length: la, head_hex: ha }, Binary { length: lb, head_hex: hb }) => la == lb && ha == hb,
            _ => match (a.to_text(), b.to_text()) {
                (Some(x), Some(y)) => self.text(&x, &y),
                _ => false,
            },
        }
    }

    fn numbers(&self, x: &str, y: &str) -> bool {
        match (canonical_number(x), canonical_number(y)) {
            (Some(cx), Some(cy)) => {
                cx == cy
                    || (self.float_tolerance > 0.0
                        && matches!((cx.parse::<f64>(), cy.parse::<f64>()), (Ok(fx), Ok(fy)) if (fx - fy).abs() <= self.float_tolerance))
            }
            // NaN, Infinity or text that is not a number.
            _ => self.text(x, y),
        }
    }

    fn text(&self, x: &str, y: &str) -> bool {
        if !self.ignore_whitespace && !self.ignore_case {
            return x == y;
        }
        let norm = |s: &str| {
            let s =
                if self.ignore_whitespace { s.split_whitespace().collect::<Vec<_>>().join(" ") } else { s.to_owned() };
            if self.ignore_case { s.to_lowercase() } else { s }
        };
        norm(x) == norm(y)
    }
}

/// A key value in merge order: NULL first, then numbers by value, then text by
/// code point (the order [`builder::compare_select`] asks each engine for).
#[derive(Debug, Clone, PartialEq, Eq)]
enum KeyValue {
    Null,
    Number(String),
    Text(String),
}

impl KeyValue {
    fn of(cell: &Cell) -> Self {
        match cell {
            Cell::Null => Self::Null,
            Cell::Number { value } => canonical_number(value).map_or_else(|| Self::Text(value.clone()), Self::Number),
            Cell::Bool { value } => Self::Number(if *value { "1" } else { "0" }.to_owned()),
            Cell::Temporal { value, .. } => Self::Text(canonical_temporal(value)),
            Cell::Binary { head_hex, .. } => Self::Text(head_hex.clone()),
            other => Self::Text(other.to_text().unwrap_or_default()),
        }
    }

    fn rank(&self) -> u8 {
        match self {
            Self::Null => 0,
            Self::Number(_) => 1,
            Self::Text(_) => 2,
        }
    }
}

impl Ord for KeyValue {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Number(a), Self::Number(b)) => cmp_decimal(a, b),
            (Self::Text(a), Self::Text(b)) => a.cmp(b),
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl PartialOrd for KeyValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Key values for messages, e.g. `(42, 'a')`.
fn describe_key(row: &[Cell], keys: usize) -> String {
    let parts: Vec<String> =
        row[..keys].iter().map(|c| c.to_text().map_or_else(|| "NULL".to_owned(), |v| format!("'{v}'"))).collect();
    format!("({})", parts.join(", "))
}

fn shorten(cell: &Cell) -> Cell {
    match cell {
        Cell::Text { value, truncated: None } => Cell::text(value.clone(), EXAMPLE_LIMITS),
        Cell::Json { value, truncated: None } => Cell::json(value.clone(), EXAMPLE_LIMITS),
        Cell::Binary { length, head_hex } => Cell::Binary {
            length: *length,
            head_hex: head_hex[..head_hex.len().min(EXAMPLE_LIMITS.binary_head_bytes * 2)].to_owned(),
        },
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Changes and their journal
// ---------------------------------------------------------------------------

/// One change the target needs, recorded while comparing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SyncChange {
    /// A source row the target lacks, in [`DataDiff::columns`] order.
    Insert { row: Vec<Cell> },
    /// The target's key values and the source values of the columns that differ.
    Update { key: Vec<Cell>, columns: Vec<u32>, values: Vec<Cell> },
    /// The target's key values of a row the source lacks.
    Delete { key: Vec<Cell> },
}

impl SyncChange {
    fn kind(&self) -> SyncKind {
        match self {
            Self::Insert { .. } => SyncKind::Insert,
            Self::Update { .. } => SyncKind::Update,
            Self::Delete { .. } => SyncKind::Delete,
        }
    }
}

/// Receives the changes found by [`compare`], in key order.
pub trait ChangeSink: Send {
    fn push(&mut self, change: SyncChange) -> DbResult<()>;
}

impl ChangeSink for Vec<SyncChange> {
    fn push(&mut self, change: SyncChange) -> DbResult<()> {
        Vec::push(self, change);
        Ok(())
    }
}

/// Changes as JSON lines in a file, so a compare of any size stays out of memory.
pub struct Journal {
    out: BufWriter<File>,
}

impl Journal {
    pub fn create(path: &Path) -> DbResult<Self> {
        Ok(Self { out: BufWriter::with_capacity(256 * 1024, File::create(path).map_err(io_error)?) })
    }

    pub fn finish(mut self) -> DbResult<()> {
        self.out.flush().map_err(io_error)
    }

    /// Reads the changes back, in the order they were written.
    pub fn read(path: &Path) -> DbResult<Changes> {
        let reader = BufReader::with_capacity(256 * 1024, File::open(path).map_err(io_error)?);
        Ok(Box::new(reader.lines().map(|line| {
            serde_json::from_str(&line.map_err(io_error)?)
                .map_err(|e| DbError::internal(format!("unreadable compare journal: {e}")))
        })))
    }
}

impl ChangeSink for Journal {
    fn push(&mut self, change: SyncChange) -> DbResult<()> {
        serde_json::to_writer(&mut self.out, &change).map_err(|e| DbError::internal(e.to_string()))?;
        self.out.write_all(b"\n").map_err(io_error)
    }
}

fn io_error(e: std::io::Error) -> DbError {
    DbError::new(ErrorKind::Storage, format!("File error: {e}"))
}

// ---------------------------------------------------------------------------
// Merge join
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Source,
    Target,
}

impl Side {
    fn label(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Target => "target",
        }
    }
}

struct Keyed {
    key: Vec<KeyValue>,
    row: Row,
}

#[derive(Default)]
struct Queue {
    rows: VecDeque<Keyed>,
    ended: bool,
    last: Option<Vec<KeyValue>>,
    read: u64,
}

/// Merges two key-ordered row streams. Rows hold the key columns first.
/// Each side must arrive in strictly ascending key order: an equal key is a
/// duplicate and a smaller one means the engine sorted differently.
struct Merge<'a> {
    keys: usize,
    width: usize,
    equality: Equality,
    max_examples: usize,
    source: Queue,
    target: Queue,
    counts: CompareCounts,
    examples: Vec<RowDiff>,
    truncated: bool,
    sink: &'a mut dyn ChangeSink,
}

impl<'a> Merge<'a> {
    fn new(keys: usize, width: usize, options: &DataCompareOptions, sink: &'a mut dyn ChangeSink) -> Self {
        Self {
            keys,
            width,
            equality: Equality::from(options),
            max_examples: options.max_examples.min(MAX_EXAMPLES) as usize,
            source: Queue::default(),
            target: Queue::default(),
            counts: CompareCounts::default(),
            examples: Vec::new(),
            truncated: false,
            sink,
        }
    }

    fn queue(&mut self, side: Side) -> &mut Queue {
        match side {
            Side::Source => &mut self.source,
            Side::Target => &mut self.target,
        }
    }

    /// The side has no buffered rows and more may come.
    fn wants(&self, side: Side) -> bool {
        let q = if side == Side::Source { &self.source } else { &self.target };
        q.rows.is_empty() && !q.ended
    }

    fn done(&self) -> bool {
        self.source.ended && self.target.ended && self.source.rows.is_empty() && self.target.rows.is_empty()
    }

    fn rows_read(&self) -> u64 {
        self.source.read + self.target.read
    }

    fn feed(&mut self, side: Side, batch: Vec<Row>) -> DbResult<()> {
        let (keys, width) = (self.keys, self.width);
        for row in batch {
            if row.len() != width {
                return Err(DbError::internal("compare row width does not match the column list"));
            }
            let key = row[..keys].iter().map(KeyValue::of).collect();
            self.queue(side).rows.push_back(Keyed { key, row });
        }
        Ok(())
    }

    fn end(&mut self, side: Side) {
        self.queue(side).ended = true;
    }

    /// Takes the next row of a side, checking that keys ascend.
    fn pop(&mut self, side: Side) -> DbResult<Keyed> {
        let keys = self.keys;
        let q = self.queue(side);
        let next = q.rows.pop_front().ok_or_else(|| DbError::internal("merge read past the end"))?;
        q.read += 1;
        if let Some(last) = &q.last {
            match last.cmp(&next.key) {
                Ordering::Less => {}
                Ordering::Equal => {
                    return Err(DbError::invalid(format!(
                        "The {} has more than one row with key {}. Choose key columns that identify a row.",
                        side.label(),
                        describe_key(&next.row, keys)
                    )));
                }
                Ordering::Greater => {
                    return Err(DbError::invalid(format!(
                        "The {} returned key {} out of order, so its sort order differs from the other side's. Choose key columns of matching types.",
                        side.label(),
                        describe_key(&next.row, keys)
                    )));
                }
            }
        }
        q.last = Some(next.key.clone());
        Ok(next)
    }

    fn example(&mut self, diff: RowDiff) {
        if self.examples.len() < self.max_examples {
            self.examples.push(diff);
        } else {
            self.truncated = true;
        }
    }

    fn only(&mut self, side: Side) -> DbResult<()> {
        let Keyed { row, .. } = self.pop(side)?;
        let shown = || row.iter().map(shorten).collect::<Vec<_>>();
        if side == Side::Source {
            self.counts.only_in_source += 1;
            self.example(RowDiff {
                kind: RowDiffKind::OnlyInSource,
                source: shown(),
                target: Vec::new(),
                changed: Vec::new(),
            });
            self.sink.push(SyncChange::Insert { row })
        } else {
            self.counts.only_in_target += 1;
            self.example(RowDiff {
                kind: RowDiffKind::OnlyInTarget,
                source: Vec::new(),
                target: shown(),
                changed: Vec::new(),
            });
            let mut key = row;
            key.truncate(self.keys);
            self.sink.push(SyncChange::Delete { key })
        }
    }

    fn matched(&mut self) -> DbResult<()> {
        let source = self.pop(Side::Source)?.row;
        let mut target = self.pop(Side::Target)?.row;
        let changed: Vec<u32> = (self.keys..self.width)
            .filter(|&i| !self.equality.same(&source[i], &target[i]))
            .map(|i| i as u32)
            .collect();
        if changed.is_empty() {
            self.counts.identical += 1;
            return Ok(());
        }
        self.counts.different += 1;
        if self.examples.len() < self.max_examples {
            self.example(RowDiff {
                kind: RowDiffKind::Different,
                source: source.iter().map(shorten).collect(),
                target: target.iter().map(shorten).collect(),
                changed: changed.clone(),
            });
        } else {
            self.truncated = true;
        }
        let values = changed.iter().map(|&i| source[i as usize].clone()).collect();
        target.truncate(self.keys);
        self.sink.push(SyncChange::Update { key: target, columns: changed, values })
    }

    /// Merges buffered rows until a side needs more.
    fn run(&mut self) -> DbResult<()> {
        loop {
            let order = match (self.source.rows.front(), self.target.rows.front()) {
                (Some(s), Some(t)) => s.key.cmp(&t.key),
                (Some(_), None) if self.target.ended => Ordering::Less,
                (None, Some(_)) if self.source.ended => Ordering::Greater,
                _ => return Ok(()),
            };
            match order {
                Ordering::Less => self.only(Side::Source)?,
                Ordering::Greater => self.only(Side::Target)?,
                Ordering::Equal => self.matched()?,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming compare
// ---------------------------------------------------------------------------

/// One side of a compare: an open session and its table.
pub struct CompareSide {
    pub session: Arc<dyn Session>,
    pub object: ObjectRef,
}

/// The result of [`compare`], with what is needed to write the sync SQL.
pub struct Compared {
    pub plan: ComparePlan,
    pub diff: DataDiff,
    pub target: SyncTarget,
}

/// Reads both structures and plans the compare.
pub async fn prepare(
    source: &CompareSide,
    target: &CompareSide,
    options: &DataCompareOptions,
) -> DbResult<(ComparePlan, TableStructure, TableStructure)> {
    check_engine(&*source.session)?;
    check_engine(&*target.session)?;
    let (s, t) = tokio::try_join!(
        crate::ops::structure(&*source.session, &source.object),
        crate::ops::structure(&*target.session, &target.object)
    )?;
    let plan = plan(&s, source.session.dialect(), &t, target.session.dialect(), options)?;
    Ok((plan, s, t))
}

/// Stops a side's stream when the compare ends early.
struct AbortOnDrop(JoinHandle<DbResult<u64>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl AbortOnDrop {
    async fn finish(&mut self) -> DbResult<()> {
        (&mut self.0).await.map_err(|e| DbError::internal(format!("compare stream failed: {e}")))?.map(|_| ())
    }
}

fn stream_side(side: &CompareSide, query: builder::BoundQuery, tx: mpsc::Sender<Vec<Row>>) -> AbortOnDrop {
    let session = side.session.clone();
    let database = side.object.database.clone();
    AbortOnDrop(tokio::spawn(async move {
        session
            .stream_query(&database, &query.sql, &query.params, &mut |_, batch| {
                if batch.is_empty() {
                    return Ok(());
                }
                // Adapters call back synchronously. Waiting for room holds the
                // stream back while the merge catches up, so memory stays bounded.
                tokio::task::block_in_place(|| tx.blocking_send(batch))
                    .map_err(|_| DbError::new(ErrorKind::Cancelled, "The compare stopped."))
            })
            .await
    }))
}

/// The next batch of a side; `None` when its stream ended. Checks for cancel while waiting.
async fn receive(rx: &mut mpsc::Receiver<Vec<Row>>, control: &TransferControl) -> DbResult<Option<Vec<Row>>> {
    loop {
        control.check()?;
        if let Ok(batch) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            return Ok(batch);
        }
    }
}

/// Compares the rows of two tables, streaming both ordered by the key. Counts
/// are complete; changes go to `sink` in key order. Needs the multi-threaded
/// Tokio runtime (each side streams on its own task).
pub async fn compare(
    source: &CompareSide,
    target: &CompareSide,
    options: &DataCompareOptions,
    sink: &mut dyn ChangeSink,
    control: &TransferControl,
) -> DbResult<Compared> {
    if tokio::runtime::Handle::current().runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread {
        return Err(DbError::internal("data compare needs the multi-threaded runtime"));
    }
    let started = Instant::now();
    let (plan, source_structure, target_structure) = prepare(source, target, options).await?;
    let keys = plan.keys();
    let source_columns: Vec<String> = plan.selected().map(|c| c.name.clone()).collect();
    let target_columns: Vec<String> = plan.selected().map(|c| c.target_name.clone()).collect();
    let source_query =
        builder::compare_select(source.session.dialect(), &source_structure, &source_columns, keys, &options.rows)?;
    let target_query =
        builder::compare_select(target.session.dialect(), &target_structure, &target_columns, keys, &options.rows)?;

    let (source_tx, mut source_rx) = mpsc::channel(CHANNEL_BATCHES);
    let (target_tx, mut target_rx) = mpsc::channel(CHANNEL_BATCHES);
    let mut source_task = stream_side(source, source_query, source_tx);
    let mut target_task = stream_side(target, target_query, target_tx);
    let mut merge = Merge::new(keys, source_columns.len(), options, sink);
    loop {
        if let Some(side) = [Side::Source, Side::Target].into_iter().find(|&side| merge.wants(side)) {
            let (rx, task) = match side {
                Side::Source => (&mut source_rx, &mut source_task),
                Side::Target => (&mut target_rx, &mut target_task),
            };
            if let Some(batch) = receive(rx, control).await? {
                merge.feed(side, batch)?;
            } else {
                task.finish().await?;
                merge.end(side);
            }
            continue;
        }
        merge.run()?;
        control.progress(merge.rows_read(), 0);
        if merge.done() {
            break;
        }
    }
    let diff = DataDiff {
        columns: source_columns,
        keys: keys as u32,
        counts: merge.counts,
        examples: std::mem::take(&mut merge.examples),
        examples_truncated: merge.truncated,
        duration_ms: started.elapsed().as_millis() as u64,
    };
    let target = SyncTarget::new(target.session.dialect(), target_structure, &plan);
    Ok(Compared { plan, diff, target })
}

// ---------------------------------------------------------------------------
// Sync SQL
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum SyncKind {
    Insert,
    Update,
    Delete,
}

/// Which kinds of change the sync SQL includes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SyncKinds {
    pub insert: bool,
    pub update: bool,
    pub delete: bool,
}

impl SyncKinds {
    pub const ALL: Self = Self { insert: true, update: true, delete: true };

    /// Passes in run order: deletes free unique values before updates and inserts reuse them.
    fn passes(self) -> VecDeque<SyncKind> {
        [(self.delete, SyncKind::Delete), (self.update, SyncKind::Update), (self.insert, SyncKind::Insert)]
            .into_iter()
            .filter_map(|(on, kind)| on.then_some(kind))
            .collect()
    }
}

/// The target table and how journal rows map onto it.
pub struct SyncTarget {
    dialect: &'static dyn Dialect,
    /// Target structure whose row identity is the compare key.
    structure: TableStructure,
    /// Target names of the compared columns, keys first.
    columns: Vec<String>,
    synced: Vec<bool>,
}

impl SyncTarget {
    pub fn new(dialect: &'static dyn Dialect, mut structure: TableStructure, plan: &ComparePlan) -> Self {
        let selected: Vec<&CompareColumn> = plan.selected().collect();
        structure.row_identity = selected.iter().filter(|c| c.key).map(|c| c.target_name.clone()).collect();
        Self {
            dialect,
            structure,
            columns: selected.iter().map(|c| c.target_name.clone()).collect(),
            synced: selected.iter().map(|c| c.synced).collect(),
        }
    }

    pub fn table(&self) -> &ObjectRef {
        &self.structure.object
    }

    /// A cell as the text parameter the target converts back: booleans as
    /// 1/0, zoned times as UTC wall-clock time where the engine has no zones.
    fn param(&self, cell: &Cell) -> Option<String> {
        match cell {
            Cell::Null => None,
            Cell::Bool { value } => Some(if *value { "1" } else { "0" }.to_owned()),
            Cell::Temporal { value, zoned: true } if self.dialect.naive_temporals() => {
                Some(utc_naive(value).unwrap_or_else(|| value.clone()))
            }
            other => other.to_text(),
        }
    }

    fn values(&self, indexes: impl Iterator<Item = usize>, cells: &[Cell]) -> Vec<ColumnValue> {
        indexes
            .zip(cells)
            .filter(|(i, _)| self.synced[*i])
            .map(|(i, cell)| ColumnValue { column: self.columns[i].clone(), value: self.param(cell) })
            .collect()
    }

    fn key(&self, key: &[Cell]) -> Vec<ColumnValue> {
        key.iter()
            .enumerate()
            .map(|(i, cell)| ColumnValue { column: self.columns[i].clone(), value: self.param(cell) })
            .collect()
    }

    /// The update or delete for a change; `None` when no column it changes is written.
    fn row_change(&self, change: &SyncChange) -> Option<RowChange> {
        match change {
            SyncChange::Update { key, columns, values } => {
                let values = self.values(columns.iter().map(|&i| i as usize), values);
                (!values.is_empty()).then(|| RowChange::Update { key: self.key(key), values })
            }
            SyncChange::Delete { key } => Some(RowChange::Delete { key: self.key(key) }),
            SyncChange::Insert { .. } => None,
        }
    }

    fn insert_columns(&self) -> Vec<String> {
        self.columns.iter().zip(&self.synced).filter(|(_, s)| **s).map(|(c, _)| c.clone()).collect()
    }

    fn insert_batch_rows(&self) -> usize {
        let width = self.insert_columns().len().max(1);
        INSERT_BATCH_ROWS.min(self.dialect.max_insert_rows()).min(self.dialect.max_params() / width).max(1)
    }

    /// Statements for the changes of `open()`, in run order. `inline` writes
    /// literals for review; otherwise values are bound parameters.
    pub fn statements<'a>(
        &'a self,
        open: impl Fn() -> DbResult<Changes> + Send + 'a,
        kinds: SyncKinds,
        inline: bool,
    ) -> Statements<'a> {
        Statements {
            target: self,
            open: Box::new(open),
            passes: kinds.passes(),
            current: None,
            inline,
            insert_columns: self.insert_columns(),
            batch: self.insert_batch_rows(),
        }
    }
}

pub type Changes = Box<dyn Iterator<Item = DbResult<SyncChange>> + Send>;

/// A sync statement and the rows it changes.
pub struct SyncStatement {
    pub kind: SyncKind,
    pub rows: u64,
    pub statement: BoundStatement,
}

/// Sync statements, produced lazily: one pass over the journal per kind of change.
pub struct Statements<'a> {
    target: &'a SyncTarget,
    open: Box<dyn Fn() -> DbResult<Changes> + Send + 'a>,
    passes: VecDeque<SyncKind>,
    current: Option<(SyncKind, Changes)>,
    inline: bool,
    insert_columns: Vec<String>,
    batch: usize,
}

impl Statements<'_> {
    fn next_insert(&mut self, changes: &mut Changes) -> DbResult<Option<SyncStatement>> {
        let t = self.target;
        let mut rows = Vec::with_capacity(self.batch);
        while rows.len() < self.batch {
            match changes.next().transpose()? {
                Some(SyncChange::Insert { row }) => rows.push(
                    row.iter().enumerate().filter(|(i, _)| t.synced[*i]).map(|(_, c)| t.param(c)).collect::<Vec<_>>(),
                ),
                Some(_) => {}
                None => break,
            }
        }
        if rows.is_empty() || self.insert_columns.is_empty() {
            return Ok(None);
        }
        let statement = if self.inline {
            let sql = builder::insert_batch_sql(t.dialect, &t.structure, &self.insert_columns, &rows)?;
            BoundStatement { sql, params: Vec::new(), expect_rows: Some(rows.len() as u64) }
        } else {
            builder::insert_batch(t.dialect, &t.structure, &self.insert_columns, &rows)?
        };
        Ok(Some(SyncStatement { kind: SyncKind::Insert, rows: rows.len() as u64, statement }))
    }

    fn next_change(&mut self, kind: SyncKind, changes: &mut Changes) -> DbResult<Option<SyncStatement>> {
        let t = self.target;
        for change in changes {
            let change = change?;
            if change.kind() != kind {
                continue;
            }
            if let Some(row) = t.row_change(&change) {
                let statement = builder::change_statement(t.dialect, &t.structure, &row, self.inline)?;
                return Ok(Some(SyncStatement { kind, rows: 1, statement }));
            }
        }
        Ok(None)
    }
}

impl Iterator for Statements<'_> {
    type Item = DbResult<SyncStatement>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.current.is_none() {
                let kind = self.passes.pop_front()?;
                match (self.open)() {
                    Ok(changes) => self.current = Some((kind, changes)),
                    Err(e) => return Some(Err(e)),
                }
            }
            let (kind, mut changes) = self.current.take()?;
            let next = if kind == SyncKind::Insert {
                self.next_insert(&mut changes)
            } else {
                self.next_change(kind, &mut changes)
            };
            match next {
                Ok(Some(statement)) => {
                    self.current = Some((kind, changes));
                    return Some(Ok(statement));
                }
                // This pass is over; the next one reads the journal again.
                Ok(None) => {}
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

/// Writes the statements as a script, each ending with `;`. Returns the statement count.
pub fn write_sql(statements: Statements<'_>, header: &str, out: &mut dyn Write) -> DbResult<u64> {
    for line in header.lines() {
        writeln!(out, "-- {line}").map_err(io_error)?;
    }
    let mut count = 0;
    for statement in statements {
        writeln!(out, "{};", statement?.statement.sql).map_err(io_error)?;
        count += 1;
    }
    out.flush().map_err(io_error)?;
    Ok(count)
}

/// The script in memory, or `None` when it is longer than `limit` bytes (save it to a file instead).
pub fn sql_text(statements: Statements<'_>, header: &str, limit: usize) -> DbResult<Option<String>> {
    struct Capped {
        text: Vec<u8>,
        limit: usize,
        overflow: bool,
    }
    impl Write for Capped {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.text.len() + buf.len() > self.limit {
                self.overflow = true;
                return Err(std::io::Error::new(std::io::ErrorKind::FileTooLarge, "the script is too large"));
            }
            self.text.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut out = Capped { text: Vec::new(), limit, overflow: false };
    match write_sql(statements, header, &mut out) {
        Ok(_) => Ok(Some(String::from_utf8(out.text).map_err(|e| DbError::internal(e.to_string()))?)),
        Err(_) if out.overflow => Ok(None),
        Err(e) => Err(e),
    }
}

/// Runs the statements in one transaction on the target; any failure changes nothing.
pub async fn apply(
    session: &dyn Session,
    database: &str,
    statements: Statements<'_>,
    control: &TransferControl,
) -> DbResult<ApplyReport> {
    let started = Instant::now();
    let mut report = ApplyReport::default();
    let mut done = 0u64;
    let mut bound = statements.map(|s| {
        control.check()?;
        let s = s?;
        let rows = u32::try_from(s.rows).unwrap_or(u32::MAX);
        match s.kind {
            SyncKind::Insert => report.inserted += rows,
            SyncKind::Update => report.updated += rows,
            SyncKind::Delete => report.deleted += rows,
        }
        done += s.rows;
        control.progress(done, 0);
        Ok(s.statement)
    });
    session.run_in_transaction(database, &mut bound).await?;
    drop(bound);
    report.duration_ms = started.elapsed().as_millis() as u64;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::IndexInfo;
    use crate::sql::{MysqlDialect, OracleDialect, PostgresDialect, SqlServerDialect, SqliteDialect};

    fn col(name: &str, ty: &str, pk: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: ty.into(),
            nullable: !pk,
            default: None,
            primary_key: pk,
            generated: false,
            identity: false,
            editable: ty != "bytea",
            numeric: false,
            comment: None,
            read_as_text: false,
        }
    }

    fn table(schema: Option<&str>, columns: Vec<ColumnInfo>) -> TableStructure {
        let pk: Vec<String> = columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
        TableStructure {
            object: ObjectRef::new(NodeKind::Table, "app", schema.map(Into::into), "items"),
            indexes: vec![IndexInfo {
                name: "items_pkey".into(),
                definition: String::new(),
                unique: true,
                primary: true,
                columns: pk.clone(),
                size_bytes: None,
            }],
            columns,
            constraints: Vec::new(),
            row_identity: pk,
            full_row_key: Vec::new(),
            comment: None,
            row_estimate: None,
            foreign_keys: Vec::new(),
            size: None,
        }
    }

    fn items(schema: Option<&str>) -> TableStructure {
        table(schema, vec![col("id", "bigint", true), col("name", "text", false), col("price", "numeric(10,2)", false)])
    }

    fn num(v: &str) -> Cell {
        Cell::number(v)
    }

    fn text(v: &str) -> Cell {
        Cell::text(v, CellLimits::FULL)
    }

    /// Merges batches the way [`compare`] feeds them.
    fn merge(
        options: &DataCompareOptions,
        width: usize,
        source: Vec<Vec<Row>>,
        target: Vec<Vec<Row>>,
    ) -> DbResult<(CompareCounts, Vec<RowDiff>, bool, Vec<SyncChange>)> {
        let mut changes = Vec::new();
        let mut m = Merge::new(1, width, options, &mut changes);
        let (mut source, mut target) = (source.into_iter(), target.into_iter());
        loop {
            if m.wants(Side::Source) {
                match source.next() {
                    Some(b) => m.feed(Side::Source, b)?,
                    None => m.end(Side::Source),
                }
                continue;
            }
            if m.wants(Side::Target) {
                match target.next() {
                    Some(b) => m.feed(Side::Target, b)?,
                    None => m.end(Side::Target),
                }
                continue;
            }
            m.run()?;
            if m.done() {
                break;
            }
        }
        let (counts, examples, truncated) = (m.counts, std::mem::take(&mut m.examples), m.truncated);
        drop(m);
        Ok((counts, examples, truncated, changes))
    }

    fn row(id: &str, name: Cell) -> Row {
        vec![num(id), name]
    }

    #[test]
    fn merge_finds_inserts_updates_and_deletes_across_batches() {
        let source = vec![
            vec![row("1", text("a")), row("2", text("b"))],
            vec![row("4", text("d")), row("10", text("j"))],
            vec![row("11", Cell::Null)],
        ];
        let target = vec![
            vec![row("2", text("B"))],
            vec![row("3", text("c")), row("4", text("d"))],
            vec![row("11", Cell::Null), row("12", text("l"))],
        ];
        let (counts, examples, truncated, changes) = merge(&DataCompareOptions::default(), 2, source, target).unwrap();
        assert_eq!(counts, CompareCounts { identical: 2, only_in_source: 2, only_in_target: 2, different: 1 });
        assert!(!truncated);
        assert_eq!(examples.len(), 5);
        assert_eq!(examples[1].kind, RowDiffKind::Different);
        assert_eq!(examples[1].changed, vec![1]);
        assert_eq!(
            changes,
            vec![
                SyncChange::Insert { row: row("1", text("a")) },
                SyncChange::Update { key: vec![num("2")], columns: vec![1], values: vec![text("b")] },
                SyncChange::Delete { key: vec![num("3")] },
                SyncChange::Insert { row: row("10", text("j")) },
                SyncChange::Delete { key: vec![num("12")] },
            ],
            "numeric keys merge by value: 10 after 4"
        );
    }

    #[test]
    fn empty_sides() {
        let (counts, ..) = merge(&DataCompareOptions::default(), 2, vec![], vec![vec![row("1", text("a"))]]).unwrap();
        assert_eq!(counts.only_in_target, 1);
        let (counts, ..) = merge(&DataCompareOptions::default(), 2, vec![vec![]], vec![]).unwrap();
        assert_eq!(counts, CompareCounts::default());
    }

    #[test]
    fn duplicate_and_unordered_keys_are_errors() {
        let dup =
            merge(&DataCompareOptions::default(), 2, vec![vec![row("1", text("a")), row("1", text("b"))]], vec![]);
        let e = dup.unwrap_err();
        assert!(e.message.contains("more than one row with key ('1')"), "{}", e.message);
        let unordered = merge(
            &DataCompareOptions::default(),
            2,
            vec![],
            vec![vec![row("2", text("a"))], vec![row("1", text("b"))]],
        );
        assert!(unordered.unwrap_err().message.contains("out of order"));
    }

    #[test]
    fn examples_are_capped_but_counts_complete() {
        let options = DataCompareOptions { max_examples: 2, ..DataCompareOptions::default() };
        let source = vec![(1..=5).map(|i| row(&i.to_string(), text("x"))).collect()];
        let (counts, examples, truncated, changes) = merge(&options, 2, source, vec![]).unwrap();
        assert_eq!((counts.only_in_source, examples.len(), truncated, changes.len()), (5, 2, true, 5));
    }

    #[test]
    fn null_handling() {
        let e = Equality::default();
        assert!(e.same(&Cell::Null, &Cell::Null));
        assert!(!e.same(&Cell::Null, &text("")));
        assert!(!e.same(&num("0"), &Cell::Null));
        // NULL keys sort first and match each other.
        assert!(KeyValue::of(&Cell::Null) < KeyValue::of(&num("-5")));
        let source = vec![vec![vec![Cell::Null, text("a")], row("1", text("b"))]];
        let target = vec![vec![vec![Cell::Null, text("a")]]];
        let (counts, ..) = merge(&DataCompareOptions::default(), 2, source, target).unwrap();
        assert_eq!((counts.identical, counts.only_in_source), (1, 1));
    }

    #[test]
    fn values_normalise_across_engines() {
        let e = Equality::default();
        assert!(e.same(&num("1.50"), &num("1.5")));
        assert!(e.same(&num("007"), &num("7")));
        assert!(e.same(&num("-0.0"), &num("0")));
        assert!(e.same(&num("1e3"), &num("1000")));
        assert!(e.same(&num("12.30"), &text("12.3")), "decimal read as text on one side");
        assert!(!e.same(&num("1.5"), &num("1.51")));
        assert!(e.same(&Cell::Bool { value: true }, &num("1")), "MySQL tinyint(1)");
        assert!(e.same(&Cell::Bool { value: false }, &text("f")));
        assert!(!e.same(&Cell::Bool { value: true }, &num("2")));
        let zoned = Cell::Temporal { value: "2024-05-01T10:00:00.500+02:00".into(), zoned: true };
        let naive = Cell::Temporal { value: "2024-05-01 08:00:00.5".into(), zoned: false };
        assert!(e.same(&zoned, &naive), "zoned values compare in UTC");
        assert!(
            e.same(&Cell::Temporal { value: "2024-05-01T08:00:00".into(), zoned: false }, &text("2024-05-01 08:00:00"))
        );
        assert!(e.same(&Cell::Temporal { value: "10:00:00.000".into(), zoned: false }, &text("10:00:00")));
        assert!(e.same(
            &Cell::json(r#"{"a":1,"b":[2]}"#, CellLimits::FULL),
            &Cell::json(r#"{"b": [2], "a": 1}"#, CellLimits::FULL)
        ));
        assert!(e.same(&Cell::binary(&[1, 2], CellLimits::FULL), &Cell::binary(&[1, 2], CellLimits::FULL)));
        assert!(!e.same(&Cell::binary(&[1, 2], CellLimits::FULL), &Cell::binary(&[1, 3], CellLimits::FULL)));
        assert!(!e.same(&text("a "), &text("a")));
    }

    #[test]
    fn text_options_and_tolerance() {
        let loose = Equality { ignore_whitespace: true, ignore_case: true, float_tolerance: 0.01 };
        assert!(loose.same(&text("  Hello   World "), &text("hello world")));
        assert!(loose.same(&num("1.005"), &num("1.0")));
        assert!(!loose.same(&num("1.02"), &num("1.0")));
        assert!(!Equality::default().same(&text("A"), &text("a")));
        let options = DataCompareOptions { ignore_case: true, ..DataCompareOptions::default() };
        let (counts, ..) =
            merge(&options, 2, vec![vec![row("1", text("ABC"))]], vec![vec![row("1", text("abc"))]]).unwrap();
        assert_eq!(counts.identical, 1);
    }

    #[test]
    fn numbers_order_by_value() {
        let mut values = ["10", "-2", "9.5", "-10", "0", "100", "-2.5", "9.45"].map(|v| KeyValue::of(&num(v)));
        values.sort();
        let sorted: Vec<String> = values
            .iter()
            .map(|k| match k {
                KeyValue::Number(n) => n.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(sorted, ["-10", "-2.5", "-2", "0", "9.45", "9.5", "10", "100"]);
        assert!(KeyValue::of(&text("B")) < KeyValue::of(&text("a")), "text keys order by code point");
    }

    #[test]
    fn plan_resolves_keys_and_columns() {
        let source = items(Some("public"));
        let mut target = items(None);
        target.columns[1].name = "NAME".into();
        target.columns.push(col("extra", "text", false));
        target.columns[2].data_type = "varchar(10)".into();
        let plan = plan(&source, &PostgresDialect, &target, &MysqlDialect, &DataCompareOptions::default()).unwrap();
        assert_eq!(plan.key_source, KeySource::Primary);
        assert_eq!(plan.columns.iter().map(|c| c.target_name.as_str()).collect::<Vec<_>>(), ["id", "NAME", "price"]);
        assert!(plan.columns[2].type_mismatch && !plan.columns[1].type_mismatch);
        for (a, b) in [
            ("int4", "bigint unsigned"),
            ("boolean", "tinyint(1)"),
            ("timestamp with time zone", "datetime(6)"),
            ("character varying(5)", "TEXT"),
            ("numeric(10,2)", "float8"),
        ] {
            assert_eq!(type_family(a), type_family(b), "{a} ~ {b}");
        }
        for (a, b) in [("interval", "int"), ("point", "integer"), ("date", "timestamp"), ("bytea", "text")] {
            assert_ne!(type_family(a), type_family(b), "{a} differs from {b}");
        }
        assert_eq!(plan.only_in_target, ["extra"]);

        let chosen = DataCompareOptions {
            key_columns: vec!["name".into()],
            columns: vec!["price".into()],
            ..DataCompareOptions::default()
        };
        let p = super::plan(&source, &PostgresDialect, &target, &MysqlDialect, &chosen).unwrap();
        assert_eq!(p.key_source, KeySource::Chosen);
        assert_eq!(p.selected().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["name", "price"]);

        let mut keyless = items(None);
        keyless.row_identity.clear();
        keyless.indexes.clear();
        let e = super::plan(&keyless, &PostgresDialect, &target, &MysqlDialect, &DataCompareOptions::default());
        assert!(e.unwrap_err().message.contains("Choose key columns"));
        let bad = DataCompareOptions { key_columns: vec!["nope".into()], ..DataCompareOptions::default() };
        assert!(super::plan(&source, &PostgresDialect, &target, &MysqlDialect, &bad).is_err());
    }

    fn sync_sql(d: &'static dyn Dialect, schema: Option<&str>, changes: Vec<SyncChange>, kinds: SyncKinds) -> String {
        let structure = items(schema);
        let plan =
            plan(&items(Some("public")), &PostgresDialect, &structure, d, &DataCompareOptions::default()).unwrap();
        let target = SyncTarget::new(d, structure, &plan);
        let statements =
            target.statements(move || Ok(Box::new(changes.clone().into_iter().map(Ok)) as Changes), kinds, true);
        sql_text(statements, "header", 1 << 20).unwrap().unwrap()
    }

    fn sample_changes() -> Vec<SyncChange> {
        vec![
            SyncChange::Insert { row: vec![num("1"), text("it's"), num("2.50")] },
            SyncChange::Update { key: vec![num("2")], columns: vec![2], values: vec![num("3")] },
            SyncChange::Insert { row: vec![num("3"), Cell::Null, num("1")] },
            SyncChange::Delete { key: vec![num("4")] },
        ]
    }

    #[test]
    fn sync_sql_per_dialect() {
        assert_eq!(
            sync_sql(&PostgresDialect, Some("public"), sample_changes(), SyncKinds::ALL),
            "-- header\n\
             DELETE FROM \"public\".\"items\" WHERE \"id\" = '4';\n\
             UPDATE \"public\".\"items\" SET \"price\" = '3' WHERE \"id\" = '2';\n\
             INSERT INTO \"public\".\"items\" (\"id\", \"name\", \"price\") VALUES ('1', 'it''s', '2.50'), ('3', NULL, '1');\n"
        );
        let mysql = sync_sql(&MysqlDialect, None, sample_changes(), SyncKinds { delete: false, ..SyncKinds::ALL });
        assert_eq!(
            mysql,
            "-- header\n\
             UPDATE `app`.`items` SET `price` = '3' WHERE `id` = '2';\n\
             INSERT INTO `app`.`items` (`id`, `name`, `price`) VALUES ('1', 'it''s', '2.50'), ('3', NULL, '1');\n"
        );
        let sqlite =
            sync_sql(&SqliteDialect, None, sample_changes(), SyncKinds { insert: false, update: false, delete: true });
        assert_eq!(sqlite, "-- header\nDELETE FROM \"app\".\"items\" WHERE \"id\" = '4';\n");
        let mssql = sync_sql(&SqlServerDialect, Some("dbo"), sample_changes(), SyncKinds::ALL);
        assert!(mssql.contains("DELETE FROM [dbo].[items] WHERE [id] = N'4';"), "{mssql}");
        // Oracle inserts several rows with INSERT ALL.
        let oracle = sync_sql(&OracleDialect, Some("APP"), sample_changes(), SyncKinds::ALL);
        assert!(oracle.contains("INSERT ALL INTO \"APP\".\"items\""), "{oracle}");
    }

    #[test]
    fn sync_statements_bind_values() {
        let structure = items(None);
        let plan =
            plan(&items(None), &MysqlDialect, &structure, &MysqlDialect, &DataCompareOptions::default()).unwrap();
        let target = SyncTarget::new(&MysqlDialect, structure, &plan);
        let changes = vec![
            SyncChange::Insert {
                row: vec![
                    num("1"),
                    Cell::Temporal { value: "2024-05-01T10:00:00+02:00".into(), zoned: true },
                    Cell::Bool { value: true },
                ],
            },
            SyncChange::Update { key: vec![num("2")], columns: vec![1], values: vec![Cell::Null] },
        ];
        let statements: Vec<SyncStatement> = target
            .statements(move || Ok(Box::new(changes.clone().into_iter().map(Ok)) as Changes), SyncKinds::ALL, false)
            .collect::<DbResult<_>>()
            .unwrap();
        assert_eq!(statements.len(), 2);
        assert_eq!(statements[0].statement.sql, "UPDATE `app`.`items` SET `name` = ? WHERE `id` = ?");
        assert_eq!(statements[0].statement.params, vec![None, Some("2".into())]);
        assert_eq!(statements[0].statement.expect_rows, Some(1));
        assert_eq!(
            statements[1].statement.params,
            vec![Some("1".into()), Some("2024-05-01 08:00:00".into()), Some("1".into())],
            "UTC wall-clock time for MySQL, booleans as 1/0"
        );
    }

    #[test]
    fn unsynced_columns_are_left_out() {
        let mut structure = items(None);
        structure.columns[2].editable = false;
        let plan =
            plan(&items(None), &PostgresDialect, &structure, &PostgresDialect, &DataCompareOptions::default()).unwrap();
        assert!(!plan.columns[2].synced);
        let target = SyncTarget::new(&PostgresDialect, structure, &plan);
        let changes = sample_changes();
        let sql = sql_text(
            target.statements(
                move || Ok(Box::new(changes.clone().into_iter().map(Ok)) as Changes),
                SyncKinds::ALL,
                true,
            ),
            "",
            1 << 20,
        )
        .unwrap()
        .unwrap();
        assert!(!sql.contains("UPDATE"), "the update only touched price: {sql}");
        assert!(sql.contains("(\"id\", \"name\") VALUES ('1', 'it''s'), ('3', NULL)"), "{sql}");
    }

    #[test]
    fn large_sql_is_refused_in_memory_and_journal_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("compare.jsonl");
        let mut journal = Journal::create(&path).unwrap();
        for change in sample_changes() {
            ChangeSink::push(&mut journal, change).unwrap();
        }
        journal.finish().unwrap();
        let read: Vec<SyncChange> = Journal::read(&path).unwrap().collect::<DbResult<_>>().unwrap();
        assert_eq!(read, sample_changes());

        let structure = items(Some("public"));
        let plan =
            plan(&structure, &PostgresDialect, &structure, &PostgresDialect, &DataCompareOptions::default()).unwrap();
        let target = SyncTarget::new(&PostgresDialect, structure, &plan);
        let open = || Journal::read(&path);
        assert!(sql_text(target.statements(open, SyncKinds::ALL, true), "", 40).unwrap().is_none());
        let mut file = Vec::new();
        assert_eq!(write_sql(target.statements(open, SyncKinds::ALL, true), "", &mut file).unwrap(), 3);
    }

    #[test]
    fn compare_select_orders_by_binary_keys() {
        let mut structure = items(Some("public"));
        structure.columns[1].nullable = true;
        let columns = ["name".to_owned(), "price".to_owned()];
        let rows = RowFilter {
            conditions: vec![crate::table::FilterCondition {
                column: "price".into(),
                op: crate::table::FilterOp::Gt,
                value: "5".into(),
            }],
            ..RowFilter::default()
        };
        let q = builder::compare_select(&PostgresDialect, &structure, &columns, 1, &rows).unwrap();
        assert_eq!(
            q.sql,
            "SELECT \"name\", \"price\" FROM \"public\".\"items\" WHERE \"price\" > $1::text::numeric(10,2) ORDER BY CASE WHEN \"name\" IS NULL THEN 0 ELSE 1 END, \"name\" COLLATE \"C\""
        );
        let q = builder::compare_select(&MysqlDialect, &items(None), &columns, 1, &RowFilter::default()).unwrap();
        assert!(
            q.sql.ends_with("ORDER BY CASE WHEN `name` IS NULL THEN 0 ELSE 1 END, CAST(`name` AS BINARY)"),
            "{}",
            q.sql
        );
        let ids = ["id".to_owned()];
        let q = builder::compare_select(&SqliteDialect, &items(None), &ids, 1, &RowFilter::default()).unwrap();
        assert!(q.sql.ends_with("ORDER BY \"id\" COLLATE BINARY"), "{}", q.sql);
    }
}
