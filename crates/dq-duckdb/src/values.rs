//! Turns `DuckDB`'s Arrow results into cells. Arrow's own formatter renders
//! values, so decimals keep every digit and nested values read naturally.

use dq_core::result::{ColumnMeta, Row as CellRow};
use dq_core::value::{Cell, CellLimits};
use duckdb::arrow::array::{Array, ArrayRef, AsArray, BooleanArray};
use duckdb::arrow::compute::cast;
use duckdb::arrow::datatypes::{DataType, Decimal128Type, Schema};
use duckdb::arrow::record_batch::RecordBatch;
use duckdb::arrow::util::display::{ArrayFormatter, FormatOptions};

const FORMAT: FormatOptions<'static> = FormatOptions::new()
    .with_null("")
    .with_date_format(Some("%Y-%m-%d"))
    .with_datetime_format(Some("%Y-%m-%d %H:%M:%S%.f"))
    .with_timestamp_format(Some("%Y-%m-%d %H:%M:%S%.f"))
    .with_timestamp_tz_format(Some("%Y-%m-%d %H:%M:%S%.f%:z"))
    .with_time_format(Some("%H:%M:%S%.f"));

/// SQL-style name of an Arrow type as `DuckDB` produced it.
pub fn type_label(t: &DataType) -> String {
    match t {
        DataType::Boolean => "BOOLEAN".into(),
        DataType::Int8 => "TINYINT".into(),
        DataType::Int16 => "SMALLINT".into(),
        DataType::Int32 => "INTEGER".into(),
        DataType::Int64 => "BIGINT".into(),
        DataType::UInt8 => "UTINYINT".into(),
        DataType::UInt16 => "USMALLINT".into(),
        DataType::UInt32 => "UINTEGER".into(),
        DataType::UInt64 => "UBIGINT".into(),
        DataType::Float16 | DataType::Float32 => "FLOAT".into(),
        DataType::Float64 => "DOUBLE".into(),
        DataType::Decimal128(p, s) | DataType::Decimal256(p, s) => format!("DECIMAL({p},{s})"),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => "VARCHAR".into(),
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView | DataType::FixedSizeBinary(_) => "BLOB".into(),
        DataType::Date32 | DataType::Date64 => "DATE".into(),
        DataType::Time32(_) | DataType::Time64(_) => "TIME".into(),
        DataType::Timestamp(_, Some(_)) => "TIMESTAMP WITH TIME ZONE".into(),
        DataType::Timestamp(_, None) => "TIMESTAMP".into(),
        DataType::Interval(_) | DataType::Duration(_) => "INTERVAL".into(),
        DataType::List(_) | DataType::LargeList(_) | DataType::FixedSizeList(..) => "LIST".into(),
        DataType::Struct(_) => "STRUCT".into(),
        DataType::Map(..) => "MAP".into(),
        DataType::Dictionary(_, _) => "ENUM".into(),
        DataType::Union(..) => "UNION".into(),
        other => other.to_string(),
    }
}

fn is_numeric(t: &DataType) -> bool {
    t.is_numeric() || matches!(t, DataType::Decimal128(..) | DataType::Decimal256(..))
}

pub fn column_meta(schema: &Schema) -> Vec<ColumnMeta> {
    schema
        .fields()
        .iter()
        .map(|f| ColumnMeta { name: f.name().clone(), type_name: type_label(f.data_type()), numeric: is_numeric(f.data_type()) })
        .collect()
}

/// One cell of `column` at `row`.
fn cell(column: &dyn Array, formatter: &ArrayFormatter<'_>, row: usize, limits: CellLimits) -> Cell {
    if column.is_null(row) {
        return Cell::Null;
    }
    let t = column.data_type();
    let text = || formatter.value(row).to_string();
    match t {
        DataType::Boolean => Cell::Bool { value: column.as_any().downcast_ref::<BooleanArray>().is_some_and(|b| b.value(row)) },
        DataType::Binary => Cell::binary(column.as_binary::<i32>().value(row), limits),
        DataType::LargeBinary => Cell::binary(column.as_binary::<i64>().value(row), limits),
        DataType::BinaryView => Cell::binary(column.as_binary_view().value(row), limits),
        DataType::FixedSizeBinary(_) => Cell::binary(column.as_fixed_size_binary().value(row), limits),
        DataType::Date32 | DataType::Date64 | DataType::Time32(_) | DataType::Time64(_) | DataType::Timestamp(_, None) => {
            Cell::Temporal { value: text(), zoned: false }
        }
        DataType::Timestamp(_, Some(_)) => Cell::Temporal { value: text(), zoned: true },
        // Exact text from the raw integer: Arrow's formatter can drop a digit of a 39-digit HUGEINT.
        DataType::Decimal128(_, scale) => Cell::number(decimal_text(column.as_primitive::<Decimal128Type>().value(row), *scale)),
        t if is_numeric(t) => Cell::number(text()),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View | DataType::Dictionary(..) => Cell::text(text(), limits),
        _ => Cell::text(text(), limits),
    }
}

/// `value / 10^scale` written out exactly.
pub fn decimal_text(value: i128, scale: i8) -> String {
    if scale <= 0 {
        return (value * 10i128.pow(u32::from(scale.unsigned_abs()))).to_string();
    }
    let sign = if value < 0 { "-" } else { "" };
    let abs = value.unsigned_abs();
    let pow = 10u128.pow(scale.unsigned_abs().into());
    format!("{sign}{}.{:0width$}", abs / pow, abs % pow, width = scale.unsigned_abs() as usize)
}

/// Time-zone-aware timestamps arrive tagged with the session's IANA zone;
/// they are shown in UTC with an explicit offset, which needs no zone database.
fn displayable(column: &ArrayRef) -> ArrayRef {
    match column.data_type() {
        DataType::Timestamp(unit, Some(tz)) if tz.as_ref() != "+00:00" => {
            cast(column, &DataType::Timestamp(*unit, Some("+00:00".into()))).unwrap_or_else(|_| column.clone())
        }
        _ => column.clone(),
    }
}

/// Rows of a batch as cells.
pub fn batch_rows(batch: &RecordBatch, limits: CellLimits) -> Vec<CellRow> {
    let columns: Vec<ArrayRef> = batch.columns().iter().map(displayable).collect();
    let formatters: Vec<Option<ArrayFormatter<'_>>> =
        columns.iter().map(|c| ArrayFormatter::try_new(c.as_ref(), &FORMAT).ok()).collect();
    (0..batch.num_rows())
        .map(|row| {
            columns
                .iter()
                .zip(&formatters)
                .map(|(c, f)| match f {
                    Some(f) => cell(c.as_ref(), f, row, limits),
                    None if c.is_null(row) => Cell::Null,
                    None => Cell::Unsupported { type_name: type_label(c.data_type()), value: None },
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use duckdb::arrow::array::{Decimal128Array, Int64Array, StringArray};
    use duckdb::arrow::datatypes::Field;

    use super::*;

    #[test]
    fn decimals_and_nulls() {
        let decimals = Decimal128Array::from(vec![Some(-1), Some(123_456)]).with_precision_and_scale(38, 10).unwrap();
        let schema = Schema::new(vec![
            Field::new("d", DataType::Decimal128(38, 10), true),
            Field::new("n", DataType::Int64, true),
            Field::new("s", DataType::Utf8, true),
        ]);
        let batch = RecordBatch::try_new(
            Arc::new(schema.clone()),
            vec![Arc::new(decimals), Arc::new(Int64Array::from(vec![None, Some(7)])), Arc::new(StringArray::from(vec![Some(""), None]))],
        )
        .unwrap();
        let rows = batch_rows(&batch, CellLimits::GRID);
        assert_eq!(rows[0][0], Cell::number("-0.0000000001"));
        assert_eq!(rows[0][1], Cell::Null);
        assert_eq!(rows[0][2], Cell::Text { value: String::new(), truncated: None });
        assert_eq!(rows[1][1], Cell::number("7"));
        assert_eq!(column_meta(&schema)[0].type_name, "DECIMAL(38,10)");
        assert_eq!(decimal_text(i128::MAX, 0), "170141183460469231731687303715884105727");
        assert_eq!(decimal_text(-1230, 2), "-12.30");
    }
}
