//! The value rendering contract between Rust and the UI (feature list §5.1).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// One result cell. Numbers are always strings so `int8`/`numeric` keep every digit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum Cell {
    Null,
    Text {
        value: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        truncated: Option<bool>,
    },
    Number {
        value: String,
    },
    Bool {
        value: bool,
    },
    /// Dates, times and timestamps. Zoned values are RFC 3339 with an offset.
    Temporal {
        value: String,
        zoned: bool,
    },
    Json {
        value: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        truncated: Option<bool>,
    },
    Binary {
        #[ts(type = "number")]
        length: u64,
        head_hex: String,
    },
    /// A type without a dedicated decoder. `value` is the server's text form when available.
    Unsupported {
        type_name: String,
        value: Option<String>,
    },
}

/// Size limits applied while decoding, so a single cell cannot blow up the grid.
#[derive(Debug, Clone, Copy)]
pub struct CellLimits {
    pub max_text_bytes: usize,
    pub binary_head_bytes: usize,
}

impl CellLimits {
    /// Interactive grids: generous but bounded.
    pub const GRID: Self = Self { max_text_bytes: 256 * 1024, binary_head_bytes: 32 };
    /// Exports and copies: full values.
    pub const FULL: Self = Self { max_text_bytes: usize::MAX, binary_head_bytes: usize::MAX };
}

impl Cell {
    pub fn text(value: impl Into<String>, limits: CellLimits) -> Self {
        let (value, truncated) = truncate(value.into(), limits.max_text_bytes);
        Self::Text { value, truncated }
    }

    pub fn json(value: impl Into<String>, limits: CellLimits) -> Self {
        let (value, truncated) = truncate(value.into(), limits.max_text_bytes);
        Self::Json { value, truncated }
    }

    pub fn number(value: impl Into<String>) -> Self {
        Self::Number { value: value.into() }
    }

    pub fn binary(bytes: &[u8], limits: CellLimits) -> Self {
        let head = &bytes[..bytes.len().min(limits.binary_head_bytes)];
        Self::Binary { length: bytes.len() as u64, head_hex: hex(head) }
    }

    /// Text form used for CSV export, copy and edit round-trips. `None` is SQL NULL.
    pub fn to_text(&self) -> Option<String> {
        match self {
            Self::Null => None,
            Self::Text { value, .. }
            | Self::Number { value }
            | Self::Temporal { value, .. }
            | Self::Json { value, .. } => Some(value.clone()),
            Self::Bool { value } => Some(value.to_string()),
            Self::Binary { head_hex, .. } => Some(format!("\\x{head_hex}")),
            Self::Unsupported { value, type_name } => Some(value.clone().unwrap_or_else(|| format!("<{type_name}>"))),
        }
    }

    /// JSON value for JSON export: numbers stay strings, JSON documents are embedded.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::Value as J;
        match self {
            Self::Null => J::Null,
            Self::Bool { value } => J::Bool(*value),
            Self::Json { value, truncated: None } => {
                serde_json::from_str(value).unwrap_or_else(|_| J::String(value.clone()))
            }
            other => other.to_text().map_or(J::Null, J::String),
        }
    }
}

fn truncate(mut value: String, max: usize) -> (String, Option<bool>) {
    if value.len() <= max {
        return (value, None);
    }
    let mut cut = max;
    while !value.is_char_boundary(cut) {
        cut -= 1;
    }
    value.truncate(cut);
    (value, Some(true))
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_and_empty_are_distinct_on_the_wire() {
        assert_eq!(serde_json::to_string(&Cell::Null).unwrap(), r#"{"kind":"null"}"#);
        assert_eq!(serde_json::to_string(&Cell::text("", CellLimits::GRID)).unwrap(), r#"{"kind":"text","value":""}"#);
    }

    #[test]
    fn big_numbers_stay_exact() {
        let json = serde_json::to_string(&Cell::number("9007199254740993")).unwrap();
        assert_eq!(json, r#"{"kind":"number","value":"9007199254740993"}"#);
    }

    #[test]
    fn binary_head() {
        let c = Cell::binary(&[0x89, 0x50, 0x4e, 0x47, 1, 2], CellLimits { max_text_bytes: 10, binary_head_bytes: 4 });
        assert_eq!(c, Cell::Binary { length: 6, head_hex: "89504e47".into() });
    }

    #[test]
    fn truncates_on_char_boundary() {
        let c = Cell::text("ab✓cd", CellLimits { max_text_bytes: 3, binary_head_bytes: 0 });
        assert_eq!(c, Cell::Text { value: "ab".into(), truncated: Some(true) });
    }

    #[test]
    fn json_export_embeds_documents() {
        let c = Cell::json(r#"{"name":"Zoë"}"#, CellLimits::FULL);
        assert_eq!(c.to_json()["name"], "Zoë");
        assert_eq!(Cell::number("1.10").to_json(), serde_json::json!("1.10"));
    }
}
