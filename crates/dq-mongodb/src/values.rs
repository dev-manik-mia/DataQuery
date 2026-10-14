//! BSON values for the grid and the Documents view: type labels, exact
//! numbers and shell text.

use dq_core::document::{DocField, Document as DqDocument};
use dq_core::value::{Cell, CellLimits};
use mongodb::bson::{Bson, Document};

use crate::literal::{compact, format_double, iso_date, pretty_document};

/// The shell's name for a value's type.
pub fn type_label(value: &Bson) -> &'static str {
    match value {
        Bson::Double(_) => "Double",
        Bson::String(_) => "String",
        Bson::Array(_) => "Array",
        Bson::Document(_) => "Object",
        Bson::Boolean(_) => "Boolean",
        Bson::Null => "Null",
        Bson::RegularExpression(_) => "Regex",
        Bson::JavaScriptCode(_) | Bson::JavaScriptCodeWithScope(_) => "JavaScript",
        Bson::Int32(_) => "Int32",
        Bson::Int64(_) => "Int64",
        Bson::Timestamp(_) => "Timestamp",
        Bson::Binary(_) => "Binary",
        Bson::ObjectId(_) => "ObjectId",
        Bson::DateTime(_) => "Date",
        Bson::Symbol(_) => "Symbol",
        Bson::Decimal128(_) => "Decimal128",
        Bson::Undefined => "Undefined",
        Bson::MaxKey => "MaxKey",
        Bson::MinKey => "MinKey",
        Bson::DbPointer(_) => "DbPointer",
    }
}

pub fn is_numeric(label: &str) -> bool {
    dq_core::document::is_numeric_label(label)
}

/// One value as a grid cell. Numbers keep every digit; objects and arrays
/// are compact Extended JSON.
pub fn cell(value: &Bson, limits: CellLimits) -> Cell {
    match value {
        Bson::Null | Bson::Undefined => Cell::Null,
        Bson::Boolean(v) => Cell::Bool { value: *v },
        Bson::Int32(v) => Cell::number(v.to_string()),
        Bson::Int64(v) => Cell::number(v.to_string()),
        Bson::Double(v) if v.is_finite() => Cell::number(format_double(*v)),
        Bson::Double(v) => Cell::text(format_double(*v), limits),
        Bson::Decimal128(v) => Cell::number(v.to_string()),
        Bson::String(s) | Bson::Symbol(s) => Cell::text(s.as_str(), limits),
        Bson::ObjectId(id) => Cell::text(id.to_hex(), limits),
        Bson::DateTime(d) => match iso_date(*d) {
            Some(iso) => Cell::Temporal { value: iso, zoned: true },
            None => Cell::text(compact(value), limits),
        },
        Bson::Binary(b) => Cell::binary(&b.bytes, limits),
        Bson::Document(_) | Bson::Array(_) => {
            Cell::json(serde_json::to_string(&value.clone().into_relaxed_extjson()).unwrap_or_default(), limits)
        }
        other => Cell::text(compact(other), limits),
    }
}

/// A field with its children (for objects and arrays) when `deep`.
pub fn field(key: &str, value: &Bson, deep: bool, limits: CellLimits) -> DocField {
    let label = type_label(value);
    let summary = cell(value, limits);
    match value {
        Bson::Document(d) if deep => {
            DocField::container(key, label, summary, d.iter().map(|(k, v)| field(k, v, true, limits)).collect::<Vec<_>>().into_iter())
        }
        Bson::Array(items) if deep => DocField::container(
            key,
            label,
            summary,
            items.iter().enumerate().map(|(i, v)| field(&i.to_string(), v, true, limits)),
        ),
        _ => DocField::scalar(key, label, summary),
    }
}

/// Top-level fields only, for grid rows.
pub fn fields(doc: &Document, limits: CellLimits) -> Vec<DocField> {
    doc.iter().map(|(k, v)| field(k, v, false, limits)).collect()
}

/// A document for the Documents view.
pub fn document(doc: &Document) -> DqDocument {
    let id = doc.get("_id").cloned().unwrap_or(Bson::Null);
    let key = compact(&id);
    let size = mongodb::bson::to_vec(doc).map(|b| b.len() as u64).ok();
    let fields = doc.iter().map(|(k, v)| field(k, v, true, CellLimits::GRID)).collect();
    DqDocument::new(key.clone(), format!("_id: {key}"), size, pretty_document(doc), fields)
}

/// Grid-shaped result rows from documents.
pub fn grid(docs: &[Document]) -> (Vec<dq_core::result::ColumnMeta>, Vec<dq_core::result::Row>) {
    let fields: Vec<Vec<DocField>> = docs.iter().map(|d| fields(d, CellLimits::GRID)).collect();
    dq_core::document::to_grid(&fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::{DateTime, doc, oid::ObjectId};

    #[test]
    fn exact_cells() {
        assert_eq!(cell(&Bson::Int64(9_007_199_254_740_993), CellLimits::GRID), Cell::number("9007199254740993"));
        assert_eq!(cell(&Bson::Decimal128("120.50".parse().unwrap()), CellLimits::GRID), Cell::number("120.50"));
        assert_eq!(cell(&Bson::Double(0.1), CellLimits::GRID), Cell::number("0.1"));
        assert_eq!(
            cell(&Bson::DateTime(DateTime::from_millis(1_790_000_000_123)), CellLimits::GRID),
            Cell::Temporal { value: "2026-09-21T14:13:20.123Z".into(), zoned: true }
        );
    }

    #[test]
    fn documents_have_keys_and_trees() {
        let id = ObjectId::parse_str("66f000000000000000000001").unwrap();
        let d = document(&doc! { "_id": id, "items": [{ "sku": "A" }], "n": Bson::Null });
        assert_eq!(d.key, "ObjectId('66f000000000000000000001')");
        assert_eq!(d.fields[1].type_label, "Array");
        assert_eq!(d.fields[1].children[0].children[0].key, "sku");
        assert_eq!(d.fields[2].value, Cell::Null);
        assert!(d.text.starts_with("{\n  _id: ObjectId("));
    }
}
