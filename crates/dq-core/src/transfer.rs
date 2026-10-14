//! Streamed export to CSV/JSON and CSV import parsing.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::error::{DbError, DbResult, ErrorKind};
use crate::node::ObjectRef;
use crate::result::{ColumnMeta, Row};
use crate::table::SortSpec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ExportFormat {
    Csv,
    Json,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum ExportSource {
    /// Runs on a read-only connection, so an export can never change data.
    Query {
        sql: String,
    },
    Table {
        object: ObjectRef,
        filter: Option<String>,
        sort: Option<SortSpec>,
    },
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

/// Receives rows as they stream from the server.
pub trait RowSink: Send {
    fn begin(&mut self, columns: &[ColumnMeta]) -> DbResult<()>;
    fn write(&mut self, rows: &[Row]) -> DbResult<()>;
    fn finish(&mut self) -> DbResult<u64>;
}

pub fn file_sink(path: &Path, format: ExportFormat) -> DbResult<Box<dyn RowSink>> {
    let file = File::create(path).map_err(io_error)?;
    Ok(match format {
        ExportFormat::Csv => {
            Box::new(CsvSink { out: Some(csv::Writer::from_writer(CountingWriter::new(file))), bytes: 0 })
        }
        ExportFormat::Json => Box::new(JsonSink { out: CountingWriter::new(file), columns: Vec::new(), first: true }),
    })
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

struct CsvSink {
    out: Option<csv::Writer<CountingWriter>>,
    bytes: u64,
}

impl RowSink for CsvSink {
    fn begin(&mut self, columns: &[ColumnMeta]) -> DbResult<()> {
        let out = self.out.as_mut().ok_or_else(|| DbError::internal("export already finished"))?;
        out.write_record(columns.iter().map(|c| c.name.as_str())).map_err(csv_error)
    }

    fn write(&mut self, rows: &[Row]) -> DbResult<()> {
        let out = self.out.as_mut().ok_or_else(|| DbError::internal("export already finished"))?;
        for row in rows {
            out.write_record(row.iter().map(|c| c.to_text().unwrap_or_default())).map_err(csv_error)?;
        }
        Ok(())
    }

    fn finish(&mut self) -> DbResult<u64> {
        if let Some(out) = self.out.take() {
            let mut inner = out.into_inner().map_err(|e| io_error(e.into_error()))?;
            inner.flush().map_err(io_error)?;
            self.bytes = inner.bytes;
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
                self.columns.iter().cloned().zip(row.iter().map(crate::value::Cell::to_json)).collect();
            self.out.write_all(if self.first { b"\n  " } else { b",\n  " }).map_err(io_error)?;
            serde_json::to_writer(&mut self.out, &object).map_err(|e| DbError::internal(e.to_string()))?;
            self.first = false;
        }
        Ok(())
    }

    fn finish(&mut self) -> DbResult<u64> {
        self.out.write_all(b"\n]\n").map_err(io_error)?;
        self.out.flush().map_err(io_error)?;
        Ok(self.out.bytes)
    }
}

// ---------------------------------------------------------------------------
// CSV import
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CsvOptions {
    pub delimiter: String,
    pub has_header: bool,
    /// Empty fields become NULL instead of an empty string.
    pub empty_as_null: bool,
}

impl CsvOptions {
    fn delimiter_byte(&self) -> DbResult<u8> {
        match self.delimiter.as_str() {
            "\\t" | "\t" => Ok(b'\t'),
            d if d.len() == 1 && d.is_ascii() => Ok(d.as_bytes()[0]),
            _ => Err(DbError::invalid("The delimiter must be a single character.")),
        }
    }

    pub fn reader(&self, path: &Path) -> DbResult<csv::Reader<File>> {
        csv::ReaderBuilder::new()
            .delimiter(self.delimiter_byte()?)
            .has_headers(self.has_header)
            .flexible(true)
            .from_path(path)
            .map_err(csv_error)
    }
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct CsvPreview {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

pub fn preview_csv(path: &Path, options: &CsvOptions, limit: usize) -> DbResult<CsvPreview> {
    let mut reader = options.reader(path)?;
    let mut rows = Vec::new();
    for record in reader.records().take(limit) {
        rows.push(record.map_err(csv_error)?.iter().map(str::to_owned).collect::<Vec<_>>());
    }
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    let headers = if options.has_header {
        reader.headers().map_err(csv_error)?.iter().map(str::to_owned).collect()
    } else {
        (1..=width).map(|i| format!("column_{i}")).collect()
    };
    Ok(CsvPreview { headers, rows })
}

/// Iterates CSV records, mapping them onto target columns. `mapping[i]` is the
/// CSV field index for target column `i`.
pub fn csv_rows(
    path: &Path,
    options: &CsvOptions,
    mapping: Vec<usize>,
) -> DbResult<impl Iterator<Item = DbResult<Vec<Option<String>>>> + Send + use<>> {
    let reader = options.reader(path)?;
    let empty_as_null = options.empty_as_null;
    Ok(reader.into_records().enumerate().map(move |(i, record)| {
        let record = record.map_err(csv_error)?;
        mapping
            .iter()
            .map(|&field| {
                let value = record
                    .get(field)
                    .ok_or_else(|| DbError::invalid(format!("Row {} has no field {}.", i + 1, field + 1)))?;
                Ok((!(empty_as_null && value.is_empty())).then(|| value.to_owned()))
            })
            .collect()
    }))
}

fn io_error(e: std::io::Error) -> DbError {
    DbError::new(ErrorKind::Storage, format!("File error: {e}"))
}

fn csv_error(e: csv::Error) -> DbError {
    DbError::invalid(format!("CSV error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{Cell, CellLimits};

    fn cols() -> Vec<ColumnMeta> {
        ["id", "name", "doc"]
            .iter()
            .map(|n| ColumnMeta { name: (*n).into(), type_name: "text".into(), numeric: false })
            .collect()
    }

    fn rows() -> Vec<Row> {
        vec![
            vec![
                Cell::number("9007199254740993"),
                Cell::text("a,\"b\"", CellLimits::FULL),
                Cell::json(r#"{"x":1}"#, CellLimits::FULL),
            ],
            vec![Cell::number("2"), Cell::Null, Cell::Null],
        ]
    }

    fn export(format: ExportFormat) -> String {
        let dir = std::env::temp_dir().join(format!("dq-export-{}-{format:?}", std::process::id()));
        let mut sink = file_sink(&dir, format).unwrap();
        sink.begin(&cols()).unwrap();
        sink.write(&rows()).unwrap();
        let bytes = sink.finish().unwrap();
        let out = std::fs::read_to_string(&dir).unwrap();
        assert_eq!(bytes as usize, out.len());
        std::fs::remove_file(dir).unwrap();
        out
    }

    #[test]
    fn csv_export_quotes_and_keeps_digits() {
        assert_eq!(export(ExportFormat::Csv), "id,name,doc\n9007199254740993,\"a,\"\"b\"\"\",\"{\"\"x\"\":1}\"\n2,,\n");
    }

    #[test]
    fn json_export_is_valid() {
        let v: serde_json::Value = serde_json::from_str(&export(ExportFormat::Json)).unwrap();
        assert_eq!(v[0]["id"], "9007199254740993");
        assert_eq!(v[0]["doc"]["x"], 1);
        assert!(v[1]["name"].is_null());
    }

    #[test]
    fn csv_import_maps_columns() {
        let path = std::env::temp_dir().join(format!("dq-import-{}.csv", std::process::id()));
        std::fs::write(&path, "a;b;c\n1;;x\n2;y;z\n").unwrap();
        let opts = CsvOptions { delimiter: ";".into(), has_header: true, empty_as_null: true };
        let preview = preview_csv(&path, &opts, 10).unwrap();
        assert_eq!(preview.headers, ["a", "b", "c"]);
        let rows: Vec<_> = csv_rows(&path, &opts, vec![2, 1]).unwrap().collect::<DbResult<_>>().unwrap();
        assert_eq!(rows, vec![vec![Some("x".into()), None], vec![Some("z".into()), Some("y".into())]]);
        std::fs::remove_file(path).unwrap();
    }
}
