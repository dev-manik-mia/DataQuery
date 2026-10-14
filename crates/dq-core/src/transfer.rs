//! Streamed export (CSV, JSON, XLSX, SQL INSERT) and import (CSV, JSON) with
//! per-row validation, encodings, progress and cancellation.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use ts_rs::TS;

use crate::error::{DbError, DbResult, ErrorKind};
use crate::node::ObjectRef;
use crate::result::{ColumnMeta, Row};
use crate::schema::ColumnInfo;
use crate::sql::Dialect;
use crate::table::{RowFilter, SortSpec};
use crate::value::Cell;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ExportFormat {
    Csv,
    Json,
    Xlsx,
    /// `INSERT` statements for the engine's dialect.
    SqlInsert,
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
            Self::Xlsx => "xlsx",
            Self::SqlInsert => "sql",
        }
    }
}

/// Text encodings for CSV and SQL files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum TextEncoding {
    #[default]
    Utf8,
    /// UTF-8 with a byte-order mark, which Excel uses to detect UTF-8.
    Utf8Bom,
    Utf16le,
    Windows1252,
    Latin1,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(default)]
#[ts(export)]
pub struct ExportOptions {
    pub delimiter: String,
    pub quote: String,
    /// Quote every field, not only those that need it.
    pub quote_all: bool,
    pub header: bool,
    /// Written for SQL NULL. Empty strings are always written as empty.
    pub null_text: String,
    pub crlf: bool,
    pub encoding: TextEncoding,
    /// Table name in `INSERT` statements; defaults to the source table.
    pub sql_table: Option<String>,
    /// Rows per `INSERT` statement.
    pub sql_batch_rows: u32,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            delimiter: ",".into(),
            quote: "\"".into(),
            quote_all: false,
            header: true,
            null_text: String::new(),
            crlf: false,
            encoding: TextEncoding::Utf8,
            sql_table: None,
            sql_batch_rows: 100,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum ExportSource {
    /// Runs on a read-only connection, so an export can never change data.
    Query { sql: String },
    Table {
        object: ObjectRef,
        #[serde(flatten)]
        #[ts(flatten)]
        rows: RowFilter,
        sort: Option<SortSpec>,
    },
    /// Rows already loaded in the grid, e.g. a selection. Rejected when any
    /// cell was shortened for display, so an export never silently loses data.
    Rows { columns: Vec<ColumnMeta>, rows: Vec<Row> },
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ExportReport {
    #[ts(type = "number")]
    pub rows: u64,
    #[ts(type = "number")]
    pub bytes: u64,
    #[ts(type = "number")]
    pub duration_ms: u64,
}

/// True when a grid cell holds its whole value (text not truncated, binary not cut to a head).
pub fn is_complete(cell: &Cell) -> bool {
    match cell {
        Cell::Text { truncated, .. } | Cell::Json { truncated, .. } => truncated.is_none(),
        Cell::Binary { length, head_hex } => head_hex.len() as u64 >= length * 2,
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// Progress and cancellation
// ---------------------------------------------------------------------------

type ProgressFn = dyn Fn(u64, u64) + Send + Sync;

/// Shared by a running transfer and the UI: cancel flag and progress callback.
#[derive(Clone)]
pub struct TransferControl {
    cancelled: Arc<AtomicBool>,
    progress: Arc<ProgressFn>,
}

impl TransferControl {
    pub fn new(cancelled: Arc<AtomicBool>, progress: impl Fn(u64, u64) + Send + Sync + 'static) -> Self {
        Self { cancelled, progress: Arc::new(progress) }
    }

    pub fn none() -> Self {
        Self::new(Arc::new(AtomicBool::new(false)), |_, _| {})
    }

    pub fn check(&self) -> DbResult<()> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(DbError::new(ErrorKind::Cancelled, "The transfer was cancelled."));
        }
        Ok(())
    }

    /// Reports rows done and bytes written (export) or read (import).
    pub fn progress(&self, rows: u64, bytes: u64) {
        (self.progress)(rows, bytes);
    }
}

// ---------------------------------------------------------------------------
// Encodings
// ---------------------------------------------------------------------------

/// Encodes UTF-8 written to it into `encoding`, failing on characters the
/// encoding cannot represent (instead of silently replacing them).
struct EncodingWriter {
    inner: CountingWriter,
    encoding: TextEncoding,
    /// Incomplete UTF-8 sequence carried to the next write.
    pending: Vec<u8>,
}

impl EncodingWriter {
    fn new(inner: CountingWriter, encoding: TextEncoding) -> std::io::Result<Self> {
        let mut w = Self { inner, encoding, pending: Vec::new() };
        match encoding {
            TextEncoding::Utf8Bom => w.inner.write_all(&[0xEF, 0xBB, 0xBF])?,
            TextEncoding::Utf16le => w.inner.write_all(&[0xFF, 0xFE])?,
            _ => {}
        }
        Ok(w)
    }

    fn encode(&mut self, text: &str) -> std::io::Result<()> {
        match self.encoding {
            TextEncoding::Utf8 | TextEncoding::Utf8Bom => self.inner.write_all(text.as_bytes()),
            TextEncoding::Utf16le => {
                let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
                self.inner.write_all(&bytes)
            }
            TextEncoding::Latin1 => {
                let mut bytes = Vec::with_capacity(text.len());
                for c in text.chars() {
                    let b = u8::try_from(u32::from(c)).map_err(|_| unmappable(c, "Latin-1"))?;
                    bytes.push(b);
                }
                self.inner.write_all(&bytes)
            }
            TextEncoding::Windows1252 => {
                let mut encoder = encoding_rs::WINDOWS_1252.new_encoder();
                let mut out = vec![0u8; text.len() + 16];
                let (result, _, written) = encoder.encode_from_utf8_without_replacement(text, &mut out, true);
                if let encoding_rs::EncoderResult::Unmappable(c) = result {
                    return Err(unmappable(c, "Windows-1252"));
                }
                self.inner.write_all(&out[..written])
            }
        }
    }
}

fn unmappable(c: char, encoding: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("`{c}` cannot be written in {encoding}. Choose UTF-8."),
    )
}

impl Write for EncodingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(buf);
        let valid = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid UTF-8 in export")),
        };
        let text = String::from_utf8(self.pending.drain(..valid).collect()).expect("checked above");
        self.encode(&text)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

struct CountingWriter {
    inner: BufWriter<File>,
    bytes: u64,
}

impl CountingWriter {
    fn new(file: File) -> Self {
        Self { inner: BufWriter::with_capacity(256 * 1024, file), bytes: 0 }
    }
}

impl Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

// ---------------------------------------------------------------------------
// Sinks
// ---------------------------------------------------------------------------

/// Receives rows as they stream from the server.
pub trait RowSink: Send {
    fn begin(&mut self, columns: &[ColumnMeta]) -> DbResult<()>;
    fn write(&mut self, rows: &[Row]) -> DbResult<()>;
    /// Bytes written so far (an estimate for XLSX until `finish`).
    fn bytes(&self) -> u64;
    fn finish(&mut self) -> DbResult<u64>;
}

fn single_byte(value: &str, what: &str) -> DbResult<u8> {
    match value {
        "\\t" | "\t" => Ok(b'\t'),
        v if v.len() == 1 && v.is_ascii() => Ok(v.as_bytes()[0]),
        _ => Err(DbError::invalid(format!("The {what} must be a single ASCII character."))),
    }
}

/// Where SQL INSERT exports write to.
pub struct SqlTarget<'a> {
    pub dialect: &'static dyn Dialect,
    pub table: &'a ObjectRef,
}

pub fn file_sink(
    path: &Path,
    format: ExportFormat,
    options: &ExportOptions,
    sql: Option<SqlTarget<'_>>,
) -> DbResult<Box<dyn RowSink>> {
    let open = || File::create(path).map(CountingWriter::new).map_err(io_error);
    Ok(match format {
        ExportFormat::Csv => {
            let writer = EncodingWriter::new(open()?, options.encoding).map_err(io_error)?;
            let out = csv::WriterBuilder::new()
                .delimiter(single_byte(&options.delimiter, "delimiter")?)
                .quote(single_byte(&options.quote, "quote")?)
                .quote_style(if options.quote_all { csv::QuoteStyle::Always } else { csv::QuoteStyle::Necessary })
                .terminator(if options.crlf { csv::Terminator::CRLF } else { csv::Terminator::Any(b'\n') })
                .from_writer(writer);
            Box::new(CsvSink { out: Some(out), bytes: 0, header: options.header, null_text: options.null_text.clone() })
        }
        ExportFormat::Json => Box::new(JsonSink { out: open()?, columns: Vec::new(), first: true }),
        ExportFormat::Xlsx => Box::new(XlsxSink::new(path)),
        ExportFormat::SqlInsert => {
            let target = sql.ok_or_else(|| DbError::invalid("SQL export needs a target table."))?;
            let table = match options.sql_table.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                Some(name) => target.dialect.quote_ident(name),
                None => target.dialect.quoted_name(target.table),
            };
            Box::new(SqlSink {
                out: EncodingWriter::new(open()?, options.encoding).map_err(io_error)?,
                dialect: target.dialect,
                table,
                columns: String::new(),
                batch: options.sql_batch_rows.clamp(1, 10_000) as usize,
            })
        }
    })
}

struct CsvSink {
    out: Option<csv::Writer<EncodingWriter>>,
    bytes: u64,
    header: bool,
    null_text: String,
}

impl RowSink for CsvSink {
    fn begin(&mut self, columns: &[ColumnMeta]) -> DbResult<()> {
        let out = self.out.as_mut().ok_or_else(|| DbError::internal("export already finished"))?;
        if self.header {
            out.write_record(columns.iter().map(|c| c.name.as_str())).map_err(csv_error)?;
        }
        Ok(())
    }

    fn write(&mut self, rows: &[Row]) -> DbResult<()> {
        let out = self.out.as_mut().ok_or_else(|| DbError::internal("export already finished"))?;
        for row in rows {
            out.write_record(row.iter().map(|c| c.to_text().unwrap_or_else(|| self.null_text.clone())))
                .map_err(csv_error)?;
        }
        Ok(())
    }

    fn bytes(&self) -> u64 {
        self.out.as_ref().map_or(self.bytes, |o| o.get_ref().inner.bytes)
    }

    fn finish(&mut self) -> DbResult<u64> {
        if let Some(out) = self.out.take() {
            let mut inner = out.into_inner().map_err(|e| io_error(e.into_error()))?;
            inner.flush().map_err(io_error)?;
            self.bytes = inner.inner.bytes;
        }
        Ok(self.bytes)
    }
}

struct JsonSink {
    out: CountingWriter,
    columns: Vec<String>,
    first: bool,
}

impl RowSink for JsonSink {
    fn begin(&mut self, columns: &[ColumnMeta]) -> DbResult<()> {
        self.columns = columns.iter().map(|c| c.name.clone()).collect();
        self.out.write_all(b"[").map_err(io_error)
    }

    fn write(&mut self, rows: &[Row]) -> DbResult<()> {
        for row in rows {
            let object: serde_json::Map<String, serde_json::Value> =
                self.columns.iter().cloned().zip(row.iter().map(Cell::to_json)).collect();
            self.out.write_all(if self.first { b"\n  " } else { b",\n  " }).map_err(io_error)?;
            serde_json::to_writer(&mut self.out, &object).map_err(|e| DbError::internal(e.to_string()))?;
            self.first = false;
        }
        Ok(())
    }

    fn bytes(&self) -> u64 {
        self.out.bytes
    }

    fn finish(&mut self) -> DbResult<u64> {
        self.out.write_all(b"\n]\n").map_err(io_error)?;
        self.out.flush().map_err(io_error)?;
        Ok(self.out.bytes)
    }
}

/// Excel sheet, written row by row in constant memory. Numbers that a double
/// cannot hold exactly are written as text so no digit is lost.
struct XlsxSink {
    path: PathBuf,
    workbook: rust_xlsxwriter::Workbook,
    row: u32,
    estimate: u64,
}

const XLSX_MAX_ROWS: u32 = 1_048_576;

impl XlsxSink {
    fn new(path: &Path) -> Self {
        let mut workbook = rust_xlsxwriter::Workbook::new();
        workbook.add_worksheet_with_constant_memory();
        Self { path: path.to_owned(), workbook, row: 0, estimate: 0 }
    }

    fn sheet(&mut self) -> DbResult<&mut rust_xlsxwriter::Worksheet> {
        self.workbook.worksheet_from_index(0).map_err(xlsx_error)
    }
}

/// The number as an `f64` when that keeps every digit.
fn exact_f64(value: &str) -> Option<f64> {
    let f: f64 = value.parse().ok()?;
    let digits = value.trim_start_matches(['-', '+']).chars().filter(char::is_ascii_digit).count();
    (f.is_finite() && digits <= 15).then_some(f)
}

impl RowSink for XlsxSink {
    fn begin(&mut self, columns: &[ColumnMeta]) -> DbResult<()> {
        let sheet = self.sheet()?;
        for (i, c) in columns.iter().enumerate() {
            sheet.write_string(0, col_index(i)?, &c.name).map_err(xlsx_error)?;
        }
        self.row = 1;
        Ok(())
    }

    fn write(&mut self, rows: &[Row]) -> DbResult<()> {
        if u64::from(self.row) + rows.len() as u64 > u64::from(XLSX_MAX_ROWS) {
            return Err(DbError::invalid("Excel sheets hold at most 1,048,576 rows. Export CSV instead."));
        }
        let mut row_number = self.row;
        let mut estimate = 0u64;
        let sheet = self.sheet()?;
        for row in rows {
            for (i, cell) in row.iter().enumerate() {
                let col = col_index(i)?;
                match cell {
                    Cell::Null => {}
                    Cell::Bool { value } => {
                        sheet.write_boolean(row_number, col, *value).map_err(xlsx_error)?;
                    }
                    Cell::Number { value } if exact_f64(value).is_some() => {
                        sheet
                            .write_number(row_number, col, exact_f64(value).unwrap_or_default())
                            .map_err(xlsx_error)?;
                    }
                    other => {
                        let text = other.to_text().unwrap_or_default();
                        estimate += text.len() as u64;
                        sheet.write_string(row_number, col, text).map_err(xlsx_error)?;
                    }
                }
            }
            row_number += 1;
        }
        self.row = row_number;
        self.estimate += estimate + rows.len() as u64 * 8;
        Ok(())
    }

    fn bytes(&self) -> u64 {
        self.estimate
    }

    fn finish(&mut self) -> DbResult<u64> {
        self.workbook.save(&self.path).map_err(xlsx_error)?;
        std::fs::metadata(&self.path).map(|m| m.len()).map_err(io_error)
    }
}

fn col_index(i: usize) -> DbResult<u16> {
    u16::try_from(i)
        .ok()
        .filter(|c| *c < 16_384)
        .ok_or_else(|| DbError::invalid("Excel sheets hold at most 16,384 columns."))
}

/// `INSERT` statements with literal values, `batch` rows per statement.
struct SqlSink {
    out: EncodingWriter,
    dialect: &'static dyn Dialect,
    table: String,
    columns: String,
    batch: usize,
}

impl SqlSink {
    fn literal(&self, cell: &Cell) -> String {
        let d = self.dialect;
        match cell {
            Cell::Null | Cell::Unsupported { value: None, .. } => "NULL".into(),
            Cell::Bool { value } => if *value { "TRUE" } else { "FALSE" }.into(),
            Cell::Number { value } if value.chars().all(|c| c.is_ascii_digit() || "+-.eE".contains(c)) => value.clone(),
            Cell::Binary { head_hex, .. } => match d.flavor() {
                crate::sql::SqlFlavor::Postgres => format!("'\\x{head_hex}'::bytea"),
                crate::sql::SqlFlavor::Mysql => format!("X'{head_hex}'"),
            },
            other => d.quote_literal(&other.to_text().unwrap_or_default()),
        }
    }
}

impl RowSink for SqlSink {
    fn begin(&mut self, columns: &[ColumnMeta]) -> DbResult<()> {
        self.columns = columns.iter().map(|c| self.dialect.quote_ident(&c.name)).collect::<Vec<_>>().join(", ");
        Ok(())
    }

    fn write(&mut self, rows: &[Row]) -> DbResult<()> {
        for chunk in rows.chunks(self.batch) {
            let values: Vec<String> = chunk
                .iter()
                .map(|row| format!("({})", row.iter().map(|c| self.literal(c)).collect::<Vec<_>>().join(", ")))
                .collect();
            let statement =
                format!("INSERT INTO {} ({}) VALUES\n  {};\n", self.table, self.columns, values.join(",\n  "));
            self.out.write_all(statement.as_bytes()).map_err(io_error)?;
        }
        Ok(())
    }

    fn bytes(&self) -> u64 {
        self.out.inner.bytes
    }

    fn finish(&mut self) -> DbResult<u64> {
        self.out.flush().map_err(io_error)?;
        Ok(self.out.inner.bytes)
    }
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(default)]
#[ts(export)]
pub struct CsvOptions {
    pub delimiter: String,
    pub quote: String,
    pub has_header: bool,
    /// Empty fields become NULL instead of an empty string.
    pub empty_as_null: bool,
    /// Fields equal to this text become NULL (e.g. `NULL` or `\N`); empty to disable.
    pub null_text: String,
    pub encoding: TextEncoding,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            delimiter: ",".into(),
            quote: "\"".into(),
            has_header: true,
            empty_as_null: false,
            null_text: String::new(),
            encoding: TextEncoding::Utf8,
        }
    }
}

/// Decodes the file into UTF-8. A byte-order mark overrides the chosen encoding.
fn decoded(path: &Path, encoding: TextEncoding) -> DbResult<Box<dyn Read + Send>> {
    let file = File::open(path).map_err(io_error)?;
    let encoding = match encoding {
        TextEncoding::Utf8 | TextEncoding::Utf8Bom => encoding_rs::UTF_8,
        TextEncoding::Utf16le => encoding_rs::UTF_16LE,
        TextEncoding::Windows1252 | TextEncoding::Latin1 => encoding_rs::WINDOWS_1252,
    };
    Ok(Box::new(
        encoding_rs_io::DecodeReaderBytesBuilder::new().encoding(Some(encoding)).bom_override(true).build(file),
    ))
}

impl CsvOptions {
    pub fn reader(&self, path: &Path) -> DbResult<csv::Reader<Box<dyn Read + Send>>> {
        Ok(csv::ReaderBuilder::new()
            .delimiter(single_byte(&self.delimiter, "delimiter")?)
            .quote(single_byte(&self.quote, "quote")?)
            .has_headers(self.has_header)
            .flexible(true)
            .from_reader(decoded(path, self.encoding)?))
    }

    fn value(&self, raw: &str) -> Option<String> {
        let null = (self.empty_as_null && raw.is_empty()) || (!self.null_text.is_empty() && raw == self.null_text);
        (!null).then(|| raw.to_owned())
    }
}

/// What to import: a CSV file or a JSON file (array of objects, or one object per line).
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum ImportSource {
    Csv { path: String, options: CsvOptions },
    Json { path: String },
}

impl ImportSource {
    pub fn path(&self) -> &str {
        match self {
            Self::Csv { path, .. } | Self::Json { path } => path,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(default)]
#[ts(export)]
pub struct ImportOptions {
    /// Rows per `INSERT` statement.
    pub batch_rows: u32,
    /// Rows per transaction; 0 imports everything in one transaction.
    pub commit_rows: u32,
    /// Import the valid rows and report the others, instead of importing nothing.
    pub skip_invalid: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self { batch_rows: 250, commit_rows: 0, skip_invalid: false }
    }
}

/// Header and first rows of a file, for the mapping step.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ImportPreview {
    /// CSV header names (or `column_N`), or the JSON keys seen in the first objects.
    pub headers: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

/// Where a target column's value comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldRef {
    Index(usize),
    Key(String),
}

pub type RowIter = Box<dyn Iterator<Item = DbResult<Vec<Option<String>>>> + Send>;

pub fn preview(source: &ImportSource, limit: usize) -> DbResult<ImportPreview> {
    match source {
        ImportSource::Csv { path, options } => {
            let mut reader = options.reader(Path::new(path))?;
            let mut rows = Vec::new();
            for record in reader.records().take(limit) {
                rows.push(record.map_err(csv_error)?.iter().map(|v| options.value(v)).collect::<Vec<_>>());
            }
            let width = rows.iter().map(Vec::len).max().unwrap_or(0);
            let headers = if options.has_header {
                reader.headers().map_err(csv_error)?.iter().map(str::to_owned).collect()
            } else {
                (1..=width).map(|i| format!("column_{i}")).collect()
            };
            Ok(ImportPreview { headers, rows })
        }
        ImportSource::Json { path } => {
            let mut headers: Vec<String> = Vec::new();
            let mut objects = Vec::new();
            for object in json_objects(Path::new(path))?.take(limit) {
                let object = object?;
                for key in object.keys() {
                    if !headers.contains(key) {
                        headers.push(key.clone());
                    }
                }
                objects.push(object);
            }
            let rows =
                objects.iter().map(|o| headers.iter().map(|h| o.get(h).and_then(|v| json_text(v))).collect()).collect();
            Ok(ImportPreview { headers, rows })
        }
    }
}

/// Iterates source rows mapped onto target columns (`fields[i]` feeds column `i`).
pub fn rows(source: &ImportSource, fields: Vec<FieldRef>) -> DbResult<RowIter> {
    match source {
        ImportSource::Csv { path, options } => {
            let indexes: Vec<usize> = fields
                .iter()
                .map(|f| match f {
                    FieldRef::Index(i) => Ok(*i),
                    FieldRef::Key(_) => Err(DbError::invalid("CSV columns are mapped by position.")),
                })
                .collect::<DbResult<_>>()?;
            let options = options.clone();
            let reader = options.reader(Path::new(path))?;
            Ok(Box::new(reader.into_records().enumerate().map(move |(i, record)| {
                let record = record.map_err(csv_error)?;
                indexes
                    .iter()
                    .map(|&field| {
                        let value = record
                            .get(field)
                            .ok_or_else(|| DbError::invalid(format!("Row {} has no field {}.", i + 1, field + 1)))?;
                        Ok(options.value(value))
                    })
                    .collect()
            })))
        }
        ImportSource::Json { path } => {
            let keys: Vec<String> = fields
                .into_iter()
                .map(|f| match f {
                    FieldRef::Key(k) => Ok(k),
                    FieldRef::Index(_) => Err(DbError::invalid("JSON fields are mapped by key.")),
                })
                .collect::<DbResult<_>>()?;
            Ok(Box::new(json_objects(Path::new(path))?.map(move |object| {
                let object = object?;
                Ok(keys.iter().map(|k| object.get(k).and_then(|v| json_text(v))).collect())
            })))
        }
    }
}

type JsonObject = std::collections::HashMap<String, Box<RawValue>>;

/// The text stored for a JSON value: strings unquoted, numbers exactly as
/// written, objects and arrays as JSON text, `null` as NULL.
fn json_text(raw: &RawValue) -> Option<String> {
    let text = raw.get().trim();
    if text == "null" {
        return None;
    }
    if text.starts_with('"') {
        return serde_json::from_str::<String>(text).ok();
    }
    Some(text.to_owned())
}

/// Streams objects from a JSON array (`[{…}, …]`) or JSON lines (`{…}\n{…}`).
/// Arrays are parsed on a helper thread so huge files never sit in memory.
fn json_objects(path: &Path) -> DbResult<Box<dyn Iterator<Item = DbResult<JsonObject>> + Send>> {
    let mut reader = BufReader::with_capacity(256 * 1024, decoded(path, TextEncoding::Utf8)?);
    let first = loop {
        let buf = reader.fill_buf().map_err(io_error)?;
        match buf.iter().position(|b| !b.is_ascii_whitespace()) {
            Some(i) => {
                let b = buf[i];
                reader.consume(i);
                break Some(b);
            }
            None if buf.is_empty() => break None,
            None => {
                let n = buf.len();
                reader.consume(n);
            }
        }
    };
    let json_error = |e: serde_json::Error| DbError::invalid(format!("JSON error: {e}"));
    match first {
        None => Ok(Box::new(std::iter::empty())),
        Some(b'{') => Ok(Box::new(
            serde_json::Deserializer::from_reader(reader).into_iter::<JsonObject>().map(move |r| r.map_err(json_error)),
        )),
        Some(b'[') => {
            let (tx, rx) = std::sync::mpsc::sync_channel::<DbResult<JsonObject>>(256);
            std::thread::spawn(move || {
                let mut de = serde_json::Deserializer::from_reader(reader);
                let sender = tx.clone();
                if let Err(e) = serde::Deserializer::deserialize_seq(&mut de, ArrayVisitor(sender)) {
                    let _ = tx.send(Err(json_error(e)));
                }
            });
            Ok(Box::new(rx.into_iter()))
        }
        Some(_) => Err(DbError::invalid("The JSON file must hold an array of objects or one object per line.")),
    }
}

/// Sends each array element to the importing thread; stops when it hangs up.
struct ArrayVisitor(std::sync::mpsc::SyncSender<DbResult<JsonObject>>);

impl<'de> serde::de::Visitor<'de> for ArrayVisitor {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an array of objects")
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while let Some(object) = seq.next_element::<JsonObject>()? {
            if self.0.send(Ok(object)).is_err() {
                return Ok(());
            }
        }
        Ok(())
    }
}

/// One rejected row in an import report.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct RowError {
    /// 1-based data row (header excluded).
    #[ts(type = "number")]
    pub row: u64,
    pub column: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, TS)]
#[ts(export)]
pub struct ImportReport {
    /// Rows written and committed.
    #[ts(type = "number")]
    pub rows: u64,
    /// Invalid rows left out (`skip_invalid`).
    #[ts(type = "number")]
    pub skipped: u64,
    /// The first rejected rows, at most [`MAX_REPORTED_ERRORS`].
    pub errors: Vec<RowError>,
    #[ts(type = "number")]
    pub error_count: u64,
    /// The import stopped early (validation errors, cancel or a server error after some commits).
    pub stopped: bool,
    pub message: Option<String>,
    #[ts(type = "number")]
    pub duration_ms: u64,
}

pub const MAX_REPORTED_ERRORS: usize = 1_000;

fn base_type(data_type: &str) -> String {
    data_type.split(['(', '[']).next().unwrap_or(data_type).trim().to_ascii_lowercase()
}

fn declared_length(data_type: &str) -> Option<usize> {
    let lower = data_type.to_ascii_lowercase();
    let base = base_type(&lower);
    if !["varchar", "character varying", "char", "character", "nvarchar", "nchar"].contains(&base.as_str()) {
        return None;
    }
    lower.split_once('(')?.1.split(')').next()?.trim().parse().ok()
}

/// Checks a value against a column before it is sent, so a bad row is
/// reported with its number instead of failing a whole batch.
pub fn validate_value(column: &ColumnInfo, value: Option<&str>) -> Result<(), String> {
    let Some(value) = value else {
        return if column.nullable { Ok(()) } else { Err("cannot be NULL".into()) };
    };
    let base = base_type(&column.data_type);
    let integer = [
        "smallint",
        "integer",
        "int",
        "bigint",
        "tinyint",
        "mediumint",
        "int2",
        "int4",
        "int8",
        "serial",
        "bigserial",
        "smallserial",
    ];
    let decimal = ["numeric", "decimal", "real", "double precision", "double", "float", "float4", "float8", "money"];
    if integer.iter().any(|t| base.starts_with(t)) && !base.starts_with("interval") {
        let digits = value.trim().trim_start_matches(['-', '+']);
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("`{value}` is not a whole number"));
        }
    } else if decimal.contains(&base.as_str()) {
        let ok = value.trim().parse::<f64>().is_ok()
            || ["nan", "infinity", "-infinity"].contains(&value.trim().to_ascii_lowercase().as_str());
        if !ok {
            return Err(format!("`{value}` is not a number"));
        }
    } else if base == "boolean" || base == "bool" {
        let v = value.trim().to_ascii_lowercase();
        if !["t", "f", "true", "false", "yes", "no", "y", "n", "on", "off", "1", "0"].contains(&v.as_str()) {
            return Err(format!("`{value}` is not a boolean"));
        }
    } else if base == "json" || base == "jsonb" {
        if serde_json::from_str::<serde::de::IgnoredAny>(value).is_err() {
            return Err("is not valid JSON".into());
        }
    } else if base == "uuid" {
        let hex: String = value.chars().filter(|c| *c != '-').collect();
        if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("`{value}` is not a UUID"));
        }
    }
    if let Some(max) = declared_length(&column.data_type)
        && value.chars().count() > max
    {
        return Err(format!("is longer than {max} characters"));
    }
    Ok(())
}

fn io_error(e: std::io::Error) -> DbError {
    DbError::new(ErrorKind::Storage, format!("File error: {e}"))
}

fn csv_error(e: csv::Error) -> DbError {
    DbError::invalid(format!("CSV error: {e}"))
}

fn xlsx_error(e: rust_xlsxwriter::XlsxError) -> DbError {
    DbError::new(ErrorKind::Storage, format!("Excel export failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeKind;
    use crate::sql::{MysqlDialect, PostgresDialect};
    use crate::value::CellLimits;

    fn cols() -> Vec<ColumnMeta> {
        ["id", "name", "doc"]
            .iter()
            .map(|n| ColumnMeta { name: (*n).into(), type_name: "text".into(), numeric: false })
            .collect()
    }

    fn sample_rows() -> Vec<Row> {
        vec![
            vec![
                Cell::number("9007199254740993"),
                Cell::text("a,\"b\"", CellLimits::FULL),
                Cell::json(r#"{"x":1}"#, CellLimits::FULL),
            ],
            vec![Cell::number("2"), Cell::Null, Cell::Null],
        ]
    }

    /// A unique temp path; tests run in parallel.
    fn temp(name: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("dq-transfer-{}-{n}-{name}", std::process::id()))
    }

    fn export(format: ExportFormat, options: &ExportOptions) -> Vec<u8> {
        let path = temp(&format!("{format:?}"));
        let table = ObjectRef::new(NodeKind::Table, "app", Some("public".into()), "orders");
        let target = SqlTarget { dialect: &PostgresDialect, table: &table };
        let mut sink = file_sink(&path, format, options, Some(target)).unwrap();
        sink.begin(&cols()).unwrap();
        sink.write(&sample_rows()).unwrap();
        let bytes = sink.finish().unwrap();
        let out = std::fs::read(&path).unwrap();
        assert_eq!(bytes as usize, out.len());
        std::fs::remove_file(path).unwrap();
        out
    }

    #[test]
    fn csv_export_quotes_and_keeps_digits() {
        let out = String::from_utf8(export(ExportFormat::Csv, &ExportOptions::default())).unwrap();
        assert_eq!(out, "id,name,doc\n9007199254740993,\"a,\"\"b\"\"\",\"{\"\"x\"\":1}\"\n2,,\n");
    }

    #[test]
    fn csv_export_options() {
        let options = ExportOptions {
            delimiter: ";".into(),
            quote_all: true,
            header: false,
            null_text: "\\N".into(),
            crlf: true,
            ..ExportOptions::default()
        };
        let out = String::from_utf8(export(ExportFormat::Csv, &options)).unwrap();
        assert!(out.ends_with("\"2\";\"\\N\";\"\\N\"\r\n"), "{out}");
        let bom =
            export(ExportFormat::Csv, &ExportOptions { encoding: TextEncoding::Utf8Bom, ..ExportOptions::default() });
        assert_eq!(&bom[..3], [0xEF, 0xBB, 0xBF]);
        let utf16 =
            export(ExportFormat::Csv, &ExportOptions { encoding: TextEncoding::Utf16le, ..ExportOptions::default() });
        assert_eq!(&utf16[..4], [0xFF, 0xFE, b'i', 0]);
    }

    #[test]
    fn json_export_is_valid() {
        let v: serde_json::Value =
            serde_json::from_slice(&export(ExportFormat::Json, &ExportOptions::default())).unwrap();
        assert_eq!(v[0]["id"], "9007199254740993");
        assert_eq!(v[0]["doc"]["x"], 1);
        assert!(v[1]["name"].is_null());
    }

    #[test]
    fn sql_insert_export() {
        let out = String::from_utf8(export(ExportFormat::SqlInsert, &ExportOptions::default())).unwrap();
        assert_eq!(
            out,
            "INSERT INTO \"public\".\"orders\" (\"id\", \"name\", \"doc\") VALUES\n  (9007199254740993, 'a,\"b\"', '{\"x\":1}'),\n  (2, NULL, NULL);\n"
        );
        let renamed = String::from_utf8(export(
            ExportFormat::SqlInsert,
            &ExportOptions { sql_table: Some("copy".into()), sql_batch_rows: 1, ..ExportOptions::default() },
        ))
        .unwrap();
        assert_eq!(renamed.matches("INSERT INTO \"copy\"").count(), 2);
    }

    #[test]
    fn xlsx_export_writes_a_workbook() {
        let out = export(ExportFormat::Xlsx, &ExportOptions::default());
        assert_eq!(&out[..2], b"PK", "xlsx is a zip file");
        assert_eq!(exact_f64("12.50"), Some(12.5));
        assert_eq!(exact_f64("9007199254740993"), None, "kept as text");
    }

    #[test]
    fn windows_1252_refuses_unmappable_characters() {
        let path = temp("1252");
        let options = ExportOptions { encoding: TextEncoding::Windows1252, ..ExportOptions::default() };
        let mut sink = file_sink(&path, ExportFormat::Csv, &options, None).unwrap();
        sink.begin(&cols()).unwrap();
        sink.write(&[vec![Cell::text("café", CellLimits::FULL), Cell::Null, Cell::Null]]).unwrap();
        assert!(
            sink.write(&[vec![Cell::text("✓", CellLimits::FULL), Cell::Null, Cell::Null]])
                .and_then(|()| sink.finish())
                .is_err()
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn csv_import_maps_columns_with_options() {
        let path = temp("import.csv");
        std::fs::write(&path, "a;b;c\n1;;x\n2;\\N;'z;z'\n").unwrap();
        let options =
            CsvOptions { delimiter: ";".into(), quote: "'".into(), null_text: "\\N".into(), ..CsvOptions::default() };
        let source = ImportSource::Csv { path: path.to_string_lossy().into_owned(), options };
        assert_eq!(preview(&source, 10).unwrap().headers, ["a", "b", "c"]);
        let got: Vec<_> =
            rows(&source, vec![FieldRef::Index(2), FieldRef::Index(1)]).unwrap().collect::<DbResult<_>>().unwrap();
        assert_eq!(got, vec![vec![Some("x".into()), Some(String::new())], vec![Some("z;z".into()), None]]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn csv_import_decodes_utf16_and_latin1() {
        let path = temp("utf16.csv");
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend("n\nZoë\n".encode_utf16().flat_map(u16::to_le_bytes));
        std::fs::write(&path, bytes).unwrap();
        let source = ImportSource::Csv { path: path.to_string_lossy().into_owned(), options: CsvOptions::default() };
        let got: Vec<_> = rows(&source, vec![FieldRef::Index(0)]).unwrap().collect::<DbResult<_>>().unwrap();
        assert_eq!(got, vec![vec![Some("Zoë".into())]]);
        std::fs::write(&path, b"n\ncaf\xe9\n").unwrap();
        let latin = ImportSource::Csv {
            path: path.to_string_lossy().into_owned(),
            options: CsvOptions { encoding: TextEncoding::Latin1, ..CsvOptions::default() },
        };
        let got: Vec<_> = rows(&latin, vec![FieldRef::Index(0)]).unwrap().collect::<DbResult<_>>().unwrap();
        assert_eq!(got, vec![vec![Some("café".into())]]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn json_import_reads_arrays_and_lines_exactly() {
        let path = temp("import.json");
        std::fs::write(
            &path,
            r#" [ {"id": 9007199254740993, "name": "Zoë", "tags": ["a"], "gone": null}, {"id": 2.50} ] "#,
        )
        .unwrap();
        let source = ImportSource::Json { path: path.to_string_lossy().into_owned() };
        let p = preview(&source, 10).unwrap();
        assert_eq!(p.headers.len(), 4);
        let got: Vec<_> = rows(
            &source,
            vec![
                FieldRef::Key("id".into()),
                FieldRef::Key("name".into()),
                FieldRef::Key("tags".into()),
                FieldRef::Key("gone".into()),
            ],
        )
        .unwrap()
        .collect::<DbResult<_>>()
        .unwrap();
        assert_eq!(got[0], vec![Some("9007199254740993".into()), Some("Zoë".into()), Some(r#"["a"]"#.into()), None]);
        assert_eq!(got[1][0].as_deref(), Some("2.50"));
        std::fs::write(&path, "{\"id\": 1}\n{\"id\": 2}\n").unwrap();
        assert_eq!(rows(&source, vec![FieldRef::Key("id".into())]).unwrap().count(), 2);
        std::fs::write(&path, "[{\"id\": 1}, {\"id\": ").unwrap();
        assert!(rows(&source, vec![FieldRef::Key("id".into())]).unwrap().any(|r| r.is_err()));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn validates_values_per_type() {
        let col = |ty: &str, nullable: bool| ColumnInfo {
            name: "c".into(),
            data_type: ty.into(),
            nullable,
            default: None,
            primary_key: false,
            generated: false,
            identity: false,
            editable: true,
            numeric: false,
            comment: None,
            read_as_text: false,
        };
        assert!(validate_value(&col("bigint", true), Some("-12")).is_ok());
        assert!(validate_value(&col("bigint", true), Some("1.5")).is_err());
        assert!(validate_value(&col("numeric(12,2)", true), Some("1e3")).is_ok());
        assert!(validate_value(&col("int", false), None).is_err());
        assert!(validate_value(&col("varchar(3)", true), Some("abcd")).is_err());
        assert!(validate_value(&col("character varying(3)", true), Some("abc")).is_ok());
        assert!(validate_value(&col("jsonb", true), Some("{")).is_err());
        assert!(validate_value(&col("boolean", true), Some("yes")).is_ok());
        assert!(validate_value(&col("uuid", true), Some("not-a-uuid")).is_err());
        assert!(validate_value(&col("interval", true), Some("1 day")).is_ok());
        assert!(validate_value(&col("text", true), Some("anything")).is_ok());
        let _ = MysqlDialect;
    }
}
