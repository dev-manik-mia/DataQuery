//! Decodes TDS values. Decimals keep every digit; dates and times are shown
//! as the server writes them.

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use dq_core::result::{ColumnMeta, Row as CellRow};
use dq_core::value::{Cell, CellLimits};
use tiberius::numeric::Numeric;
use tiberius::{Column, ColumnData, ColumnType, FromSql, Row};

pub fn type_label(t: ColumnType) -> &'static str {
    match t {
        ColumnType::Null => "null",
        ColumnType::Bit | ColumnType::Bitn => "bit",
        ColumnType::Int1 => "tinyint",
        ColumnType::Int2 => "smallint",
        ColumnType::Int4 | ColumnType::Intn => "int",
        ColumnType::Int8 => "bigint",
        ColumnType::Float4 => "real",
        ColumnType::Float8 | ColumnType::Floatn => "float",
        ColumnType::Money => "money",
        ColumnType::Money4 => "smallmoney",
        ColumnType::Datetime | ColumnType::Datetimen => "datetime",
        ColumnType::Datetime4 => "smalldatetime",
        ColumnType::Daten => "date",
        ColumnType::Timen => "time",
        ColumnType::Datetime2 => "datetime2",
        ColumnType::DatetimeOffsetn => "datetimeoffset",
        ColumnType::Guid => "uniqueidentifier",
        ColumnType::Decimaln => "decimal",
        ColumnType::Numericn => "numeric",
        ColumnType::BigVarBin => "varbinary",
        ColumnType::BigBinary => "binary",
        ColumnType::BigVarChar => "varchar",
        ColumnType::BigChar => "char",
        ColumnType::NVarchar => "nvarchar",
        ColumnType::NChar => "nchar",
        ColumnType::Xml => "xml",
        ColumnType::Udt => "udt",
        ColumnType::Text => "text",
        ColumnType::NText => "ntext",
        ColumnType::Image => "image",
        ColumnType::SSVariant => "sql_variant",
    }
}

pub fn is_numeric(t: ColumnType) -> bool {
    matches!(
        t,
        ColumnType::Int1
            | ColumnType::Int2
            | ColumnType::Int4
            | ColumnType::Int8
            | ColumnType::Intn
            | ColumnType::Float4
            | ColumnType::Float8
            | ColumnType::Floatn
            | ColumnType::Money
            | ColumnType::Money4
            | ColumnType::Decimaln
            | ColumnType::Numericn
    )
}

pub fn column_meta(columns: &[Column]) -> Vec<ColumnMeta> {
    columns
        .iter()
        .map(|c| ColumnMeta {
            name: c.name().to_owned(),
            type_name: type_label(c.column_type()).to_owned(),
            numeric: is_numeric(c.column_type()),
        })
        .collect()
}

/// Exact decimal text: `-12.30`, `7`.
pub fn numeric_text(n: Numeric) -> String {
    let scale = u32::from(n.scale());
    let sign = if n.value() < 0 { "-" } else { "" };
    let abs = n.value().unsigned_abs();
    if scale == 0 {
        return format!("{sign}{abs}");
    }
    let pow = 10u128.pow(scale);
    format!("{sign}{}.{:0width$}", abs / pow, abs % pow, width = scale as usize)
}

fn temporal<'a, T: FromSql<'a>>(data: &'a ColumnData<'static>, format: impl Fn(T) -> String) -> Option<String> {
    T::from_sql(data).ok().flatten().map(format)
}

const DATE_TIME: &str = "%Y-%m-%d %H:%M:%S%.f";

pub fn decode(data: &ColumnData<'static>, limits: CellLimits) -> Cell {
    match data {
        ColumnData::U8(Some(v)) => Cell::number(v.to_string()),
        ColumnData::I16(Some(v)) => Cell::number(v.to_string()),
        ColumnData::I32(Some(v)) => Cell::number(v.to_string()),
        ColumnData::I64(Some(v)) => Cell::number(v.to_string()),
        ColumnData::F32(Some(v)) => Cell::number(v.to_string()),
        ColumnData::F64(Some(v)) => Cell::number(v.to_string()),
        ColumnData::Bit(Some(v)) => Cell::Bool { value: *v },
        ColumnData::String(Some(v)) => Cell::text(v.as_ref(), limits),
        ColumnData::Guid(Some(v)) => Cell::text(v.to_string().to_ascii_uppercase(), limits),
        ColumnData::Binary(Some(v)) => Cell::binary(v, limits),
        ColumnData::Numeric(Some(v)) => Cell::number(numeric_text(*v)),
        ColumnData::Xml(Some(v)) => Cell::text(v.as_ref().as_ref(), limits),
        ColumnData::DateTime(Some(_)) | ColumnData::SmallDateTime(Some(_)) | ColumnData::DateTime2(Some(_)) => {
            temporal(data, |v: NaiveDateTime| v.format(DATE_TIME).to_string())
                .map_or(Cell::Null, |value| Cell::Temporal { value, zoned: false })
        }
        ColumnData::Date(Some(_)) => temporal(data, |v: NaiveDate| v.format("%Y-%m-%d").to_string())
            .map_or(Cell::Null, |value| Cell::Temporal { value, zoned: false }),
        ColumnData::Time(Some(_)) => temporal(data, |v: NaiveTime| v.format("%H:%M:%S%.f").to_string())
            .map_or(Cell::Null, |value| Cell::Temporal { value, zoned: false }),
        ColumnData::DateTimeOffset(Some(_)) => {
            temporal(data, |v: DateTime<FixedOffset>| v.format("%Y-%m-%d %H:%M:%S%.f %:z").to_string())
                .map_or(Cell::Null, |value| Cell::Temporal { value, zoned: true })
        }
        _ => Cell::Null,
    }
}

/// Catalog value as text; binary values become lower-case hex.
pub fn cell_text(data: &ColumnData<'static>, limits: CellLimits) -> Option<String> {
    match decode(data, limits) {
        Cell::Binary { head_hex, .. } => Some(head_hex),
        other => other.to_text(),
    }
}

pub fn decode_row(row: &Row, limits: CellLimits) -> CellRow {
    row.cells().map(|(_, data)| decode(data, limits)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_stay_exact() {
        assert_eq!(numeric_text(Numeric::new_with_scale(-1230, 2)), "-12.30");
        assert_eq!(numeric_text(Numeric::new_with_scale(7, 0)), "7");
        assert_eq!(numeric_text(Numeric::new_with_scale(-1, 10)), "-0.0000000001");
        let big = 12_345_678_901_234_567_890_123_456_780_123_456_789_i128;
        assert_eq!(numeric_text(Numeric::new_with_scale(big, 10)), "1234567890123456789012345678.0123456789");
    }

    #[test]
    fn decodes_temporal_and_guid() {
        let d = ColumnData::Guid(Some("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".parse().unwrap()));
        assert_eq!(cell_text(&d, CellLimits::GRID).as_deref(), Some("A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11"));
        assert_eq!(decode(&ColumnData::I32(None), CellLimits::GRID), Cell::Null);
        assert_eq!(decode(&ColumnData::Bit(Some(true)), CellLimits::GRID), Cell::Bool { value: true });
    }
}
