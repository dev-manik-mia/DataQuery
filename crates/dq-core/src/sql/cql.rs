//! CQL read analysis (screen 14's banner): does a `SELECT` read one partition,
//! several, or scan the table (`ALLOW FILTERING`, no `WHERE`)?

use serde::Serialize;
use ts_rs::TS;

use super::dialect::SqlFlavor;
use super::lexer::{Token, TokenKind, tokenize};
use crate::node::{NodeKind, ObjectRef};
use crate::schema::{KeyRole, KeyRoleKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum AnalysisLevel {
    Good,
    Info,
    Caution,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
pub struct QueryAnalysis {
    pub level: AnalysisLevel,
    pub message: String,
    /// The table read, with its key columns for the result headers.
    pub object: ObjectRef,
    pub keys: Vec<KeyRole>,
}

/// How a `WHERE` term restricts a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Restriction {
    Eq,
    In,
    Other,
}

/// The table a `SELECT` reads: `FROM ks.table` or `FROM table` in `keyspace`.
pub fn select_target(sql: &str, keyspace: &str) -> Option<ObjectRef> {
    let tokens = significant(sql);
    if !tokens.first().is_some_and(|t| t.is_keyword(sql, "SELECT")) {
        return None;
    }
    let from = tokens.iter().position(|t| t.is_keyword(sql, "FROM"))?;
    let first = name(sql, tokens.get(from + 1)?)?;
    let dotted = tokens.get(from + 2).is_some_and(|t| t.text(sql) == ".");
    let (ks, table) = if dotted { (first, name(sql, tokens.get(from + 3)?)?) } else { (keyspace.to_owned(), first) };
    (!ks.is_empty()).then(|| ObjectRef::new(NodeKind::Table, ks, None, table))
}

/// Classifies a `SELECT` on a table with the given key columns.
pub fn analyze_select(sql: &str, object: ObjectRef, keys: Vec<KeyRole>) -> Option<QueryAnalysis> {
    let tokens = significant(sql);
    if !tokens.first().is_some_and(|t| t.is_keyword(sql, "SELECT")) || keys.is_empty() {
        return None;
    }
    let allow_filtering = tokens.windows(2).any(|w| w[0].is_keyword(sql, "ALLOW") && w[1].is_keyword(sql, "FILTERING"));
    let partition: Vec<&str> =
        keys.iter().filter(|k| k.role == KeyRoleKind::Partition).map(|k| k.column.as_str()).collect();
    let terms = where_terms(sql, &tokens);
    let token_scan = terms.iter().any(|(c, _)| c == "token");
    let of = |col: &str| terms.iter().find(|(c, _)| c == col).map(|(_, r)| *r);
    let (level, message) = if allow_filtering {
        (
            AnalysisLevel::Caution,
            "ALLOW FILTERING: Cassandra reads whole partitions and discards rows that do not match. It can be slow and time out on large tables.".to_owned(),
        )
    } else if terms.is_empty() {
        (AnalysisLevel::Caution, "Full table scan: without WHERE every partition on every node is read.".to_owned())
    } else if token_scan {
        (AnalysisLevel::Info, "Token range scan: reads the partitions whose token falls in the range.".to_owned())
    } else if partition.iter().all(|c| of(c) == Some(Restriction::Eq)) {
        (AnalysisLevel::Good, "Single-partition read. Partition key fully specified, no ALLOW FILTERING.".to_owned())
    } else if partition.iter().all(|c| matches!(of(c), Some(Restriction::Eq | Restriction::In))) {
        (
            AnalysisLevel::Info,
            "Multi-partition read: IN on the partition key queries several partitions, often on several nodes."
                .to_owned(),
        )
    } else {
        (
            AnalysisLevel::Caution,
            format!(
                "The partition key ({}) is not fully restricted with = or IN; Cassandra rejects this without ALLOW FILTERING or an index.",
                partition.join(", ")
            ),
        )
    };
    Some(QueryAnalysis { level, message, object, keys })
}

fn significant(sql: &str) -> Vec<Token> {
    tokenize(sql, SqlFlavor::Cql).into_iter().filter(|t| !t.is_trivia()).collect()
}

/// A name as Cassandra resolves it: unquoted names fold to lower case.
fn name(sql: &str, t: &Token) -> Option<String> {
    match t.kind {
        TokenKind::Word => Some(t.text(sql).to_ascii_lowercase()),
        TokenKind::QuotedIdent => {
            let s = t.text(sql);
            Some(s[1..s.len().saturating_sub(1)].replace("\"\"", "\""))
        }
        _ => None,
    }
}

/// `column → restriction` for each top-level `AND` term of the `WHERE` clause.
fn where_terms(sql: &str, tokens: &[Token]) -> Vec<(String, Restriction)> {
    let Some(start) = tokens.iter().position(|t| t.is_keyword(sql, "WHERE")) else {
        return Vec::new();
    };
    let ends = ["ORDER", "GROUP", "LIMIT", "PER", "ALLOW", "BYPASS"];
    let mut terms = Vec::new();
    let mut depth = 0i32;
    let mut term: Vec<&Token> = Vec::new();
    let mut flush = |term: &mut Vec<&Token>| {
        if let Some(first) = term.first() {
            let restriction = match term.iter().find(|t| {
                matches!(t.text(sql), "=" | "<" | ">") || t.is_keyword(sql, "IN") || t.is_keyword(sql, "CONTAINS")
            }) {
                Some(t) if t.text(sql) == "=" && !term.iter().any(|x| matches!(x.text(sql), "<" | ">" | "!")) => {
                    Restriction::Eq
                }
                Some(t) if t.is_keyword(sql, "IN") => Restriction::In,
                _ => Restriction::Other,
            };
            // `(a, b) = (…)` restricts each listed column the same way.
            if first.kind == TokenKind::OpenParen {
                for t in term.iter().skip(1).take_while(|t| t.kind != TokenKind::CloseParen) {
                    if let Some(n) = name(sql, t) {
                        terms.push((n, restriction));
                    }
                }
            } else if let Some(n) = name(sql, first) {
                terms.push((n, restriction));
            }
        }
        term.clear();
    };
    for t in &tokens[start + 1..] {
        match t.kind {
            TokenKind::OpenParen => depth += 1,
            TokenKind::CloseParen => depth -= 1,
            TokenKind::Semicolon if depth == 0 => break,
            TokenKind::Word if depth == 0 && ends.iter().any(|e| t.is_keyword(sql, e)) => break,
            TokenKind::Word if depth == 0 && t.is_keyword(sql, "AND") => {
                flush(&mut term);
                continue;
            }
            _ => {}
        }
        term.push(t);
    }
    flush(&mut term);
    terms
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Vec<KeyRole> {
        let k = |c: &str, role, descending| KeyRole { column: c.into(), role, descending };
        vec![
            k("device_id", KeyRoleKind::Partition, false),
            k("bucket", KeyRoleKind::Partition, false),
            k("ts", KeyRoleKind::Clustering, true),
        ]
    }

    fn level(sql: &str) -> AnalysisLevel {
        let object = select_target(sql, "telemetry").unwrap();
        analyze_select(sql, object, keys()).unwrap().level
    }

    #[test]
    fn finds_the_table() {
        let t = select_target("SELECT * FROM telemetry.\"Readings\" WHERE x = 1", "other").unwrap();
        assert_eq!((t.database.as_str(), t.name.as_str()), ("telemetry", "Readings"));
        assert_eq!(select_target("select a from Readings", "ks").unwrap().name, "readings");
        assert!(select_target("INSERT INTO t (a) VALUES (1)", "ks").is_none());
        assert!(select_target("SELECT now() FROM", "ks").is_none());
    }

    #[test]
    fn classifies_reads() {
        assert_eq!(
            level(
                "SELECT * FROM readings_by_device WHERE device_id = 'dev-88f2' AND bucket = '2026-10-14' ORDER BY ts DESC;"
            ),
            AnalysisLevel::Good
        );
        assert_eq!(level("SELECT * FROM r WHERE device_id IN ('a','b') AND bucket = ?"), AnalysisLevel::Info);
        assert_eq!(level("SELECT * FROM r WHERE device_id = 'a'"), AnalysisLevel::Caution);
        assert_eq!(level("SELECT * FROM r"), AnalysisLevel::Caution);
        assert_eq!(level("SELECT * FROM r WHERE metric = 'cpu' ALLOW FILTERING"), AnalysisLevel::Caution);
        assert_eq!(level("SELECT * FROM r WHERE token(device_id, bucket) > 0"), AnalysisLevel::Info);
        assert_eq!(level("SELECT * FROM r WHERE (device_id, bucket) = ('a', 'b')"), AnalysisLevel::Good);
        assert_eq!(level("SELECT * FROM r WHERE device_id = 'a' AND bucket >= 'b'"), AnalysisLevel::Caution);
    }

    #[test]
    fn batches_stay_whole() {
        use crate::sql::{StatementKind, split_statements};
        let src = "BEGIN BATCH\n  INSERT INTO t (a) VALUES (1);\n  DELETE FROM t WHERE a = 2;\nAPPLY BATCH;\nCONSISTENCY QUORUM;\nSELECT * FROM t -- c;\n";
        let s = split_statements(src, SqlFlavor::Cql);
        assert_eq!(s.len(), 3);
        assert!(s[0].text(src).ends_with("APPLY BATCH"));
        assert!(s[0].writes);
        assert_eq!((s[1].kind, s[1].writes), (StatementKind::Set, false));
        assert!(!s[2].writes);
        assert_eq!(split_statements("SELECT * FROM t WHERE a = ? AND b = ?", SqlFlavor::Cql)[0].param_count, 2);
    }
}
