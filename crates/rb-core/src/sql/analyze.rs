//! Statement splitting and the safety classification behind confirmations.

use serde::Serialize;
use ts_rs::TS;

use super::dialect::SqlFlavor;
use super::lexer::{Token, TokenKind, tokenize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum StatementKind {
    Select,
    Insert,
    Update,
    Delete,
    Merge,
    Create,
    Alter,
    Drop,
    Truncate,
    Grant,
    Transaction,
    Set,
    Show,
    Explain,
    Call,
    Use,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum DestructiveReason {
    Drop,
    Truncate,
    AlterDrop,
    DeleteWithoutWhere,
    UpdateWithoutWhere,
}

impl DestructiveReason {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Drop => "DROP removes the object and all of its data.",
            Self::Truncate => "TRUNCATE removes every row.",
            Self::AlterDrop => "ALTER … DROP removes part of the object and its data.",
            Self::DeleteWithoutWhere => "DELETE without WHERE removes every row.",
            Self::UpdateWithoutWhere => "UPDATE without WHERE changes every row.",
        }
    }
}

/// One statement inside a script. Offsets are UTF-16 code units so the editor
/// can use them directly; `byte_start`/`byte_end` index the Rust string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
pub struct StatementInfo {
    pub from: u32,
    pub to: u32,
    #[serde(skip)]
    #[ts(skip)]
    pub byte_start: usize,
    #[serde(skip)]
    #[ts(skip)]
    pub byte_end: usize,
    pub kind: StatementKind,
    /// The statement may change data, schema or permissions.
    pub writes: bool,
    pub destructive: Option<DestructiveReason>,
    /// Uses `$1` / `?` placeholders.
    pub has_params: bool,
}

impl StatementInfo {
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.byte_start..self.byte_end]
    }
}

/// Splits `src` into statements, skipping empty and comment-only ones.
pub fn split_statements(src: &str, flavor: SqlFlavor) -> Vec<StatementInfo> {
    let tokens = tokenize(src, flavor);
    let mut out = Vec::new();
    let mut utf16 = Utf16Index::new(src);
    let mut chunk_start = 0;
    for (i, tok) in tokens.iter().enumerate() {
        if tok.kind == TokenKind::Semicolon {
            push_statement(src, &tokens[chunk_start..i], &mut utf16, &mut out);
            chunk_start = i + 1;
        }
    }
    push_statement(src, &tokens[chunk_start..], &mut utf16, &mut out);
    out
}

/// Classifies a single statement (any trailing `;` is ignored).
pub fn analyze(sql: &str, flavor: SqlFlavor) -> Option<StatementInfo> {
    split_statements(sql, flavor).into_iter().next()
}

fn push_statement(src: &str, tokens: &[Token], utf16: &mut Utf16Index, out: &mut Vec<StatementInfo>) {
    let Some(first) = tokens.iter().position(|t| !t.is_trivia()) else {
        return;
    };
    let last = tokens.iter().rposition(|t| !t.is_trivia()).unwrap_or(first);
    let body = &tokens[first..=last];
    let sig: Vec<Token> = body.iter().copied().filter(|t| !t.is_trivia()).collect();
    let (kind, writes, destructive) = classify(src, &sig);
    let byte_start = body[0].start;
    let byte_end = body[body.len() - 1].end;
    out.push(StatementInfo {
        from: utf16.at(byte_start),
        to: utf16.at(byte_end),
        byte_start,
        byte_end,
        kind,
        writes,
        destructive,
        has_params: sig.iter().any(|t| t.kind == TokenKind::Param),
    });
}

fn classify(src: &str, sig: &[Token]) -> (StatementKind, bool, Option<DestructiveReason>) {
    let word = |i: usize| sig.get(i).filter(|t| t.kind == TokenKind::Word).map(|t| t.text(src).to_ascii_uppercase());
    let head = word(0).unwrap_or_default();
    let mut kind = keyword_kind(&head);

    if head == "WITH" {
        kind = main_keyword_after_with(src, sig).map_or(StatementKind::Select, keyword_kind);
    }
    if matches!(head.as_str(), "EXPLAIN" | "DESCRIBE" | "DESC") && word(1).is_none_or(|w| w != "ANALYZE") {
        // Plain EXPLAIN does not execute the statement.
        return (StatementKind::Explain, false, None);
    }

    let contains_write_head =
        (0..sig.len()).any(|i| is_statement_head(src, sig, i) && is_write_keyword(sig[i].text(src)));
    let select_into =
        kind == StatementKind::Select && depth_zero_words(src, sig).any(|w| w.eq_ignore_ascii_case("INTO"));
    let writes = match kind {
        StatementKind::Select
        | StatementKind::Show
        | StatementKind::Set
        | StatementKind::Transaction
        | StatementKind::Use => contains_write_head || select_into,
        StatementKind::Explain => contains_write_head,
        _ => true,
    };

    let destructive = match kind {
        StatementKind::Drop => Some(DestructiveReason::Drop),
        StatementKind::Truncate => Some(DestructiveReason::Truncate),
        StatementKind::Alter if depth_zero_words(src, sig).any(|w| w.eq_ignore_ascii_case("DROP")) => {
            Some(DestructiveReason::AlterDrop)
        }
        _ => unbounded_write(src, sig),
    };
    (kind, writes, destructive)
}

fn keyword_kind(word: &str) -> StatementKind {
    match word.to_ascii_uppercase().as_str() {
        "SELECT" | "VALUES" | "TABLE" | "WITH" => StatementKind::Select,
        "INSERT" | "REPLACE" | "COPY" | "LOAD" => StatementKind::Insert,
        "UPDATE" => StatementKind::Update,
        "DELETE" => StatementKind::Delete,
        "MERGE" | "UPSERT" => StatementKind::Merge,
        "CREATE" => StatementKind::Create,
        "ALTER" | "RENAME" | "COMMENT" | "REINDEX" | "CLUSTER" | "VACUUM" | "ANALYZE" | "REFRESH" => {
            StatementKind::Alter
        }
        "DROP" => StatementKind::Drop,
        "TRUNCATE" => StatementKind::Truncate,
        "GRANT" | "REVOKE" => StatementKind::Grant,
        "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" | "END" | "ABORT" => {
            StatementKind::Transaction
        }
        "SET" | "RESET" => StatementKind::Set,
        "SHOW" => StatementKind::Show,
        "EXPLAIN" | "DESCRIBE" | "DESC" => StatementKind::Explain,
        "CALL" | "DO" | "EXECUTE" => StatementKind::Call,
        "USE" => StatementKind::Use,
        _ => StatementKind::Other,
    }
}

fn is_write_keyword(word: &str) -> bool {
    ["INSERT", "UPDATE", "DELETE", "MERGE"].iter().any(|k| k.eq_ignore_ascii_case(word))
}

/// The keyword that starts the main statement after `WITH a AS (...), b AS (...)`.
fn main_keyword_after_with<'a>(src: &'a str, sig: &[Token]) -> Option<&'a str> {
    depth_zero_tokens(sig).skip(1).filter(|t| t.kind == TokenKind::Word).map(|t| t.text(src)).find(|w| {
        ["SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "VALUES", "TABLE"].iter().any(|k| k.eq_ignore_ascii_case(w))
    })
}

fn depth_zero_tokens(sig: &[Token]) -> impl Iterator<Item = &Token> {
    let mut depth = 0i32;
    sig.iter().filter(move |t| {
        match t.kind {
            TokenKind::OpenParen => depth += 1,
            TokenKind::CloseParen => depth -= 1,
            _ => return depth == 0,
        }
        false
    })
}

fn depth_zero_words<'a>(src: &'a str, sig: &'a [Token]) -> impl Iterator<Item = &'a str> {
    depth_zero_tokens(sig).filter(|t| t.kind == TokenKind::Word).map(move |t| t.text(src))
}

/// A DML keyword starts a (sub)statement when it opens the statement or a
/// parenthesis. This excludes `ON DELETE CASCADE`, `FOR UPDATE`,
/// `DO UPDATE SET` and `ON DUPLICATE KEY UPDATE`.
fn is_statement_head(src: &str, sig: &[Token], i: usize) -> bool {
    if sig[i].kind != TokenKind::Word {
        return false;
    }
    i == 0
        || sig[i - 1].kind == TokenKind::OpenParen
        || (i == 1 && sig[0].is_keyword(src, "EXPLAIN"))
        || (i >= 1 && sig[i - 1].is_keyword(src, "ANALYZE"))
}

/// Finds a DELETE or UPDATE head whose scope has no WHERE at the same depth.
fn unbounded_write(src: &str, sig: &[Token]) -> Option<DestructiveReason> {
    for i in 0..sig.len() {
        if !is_statement_head(src, sig, i) {
            continue;
        }
        let reason = if sig[i].is_keyword(src, "DELETE") {
            DestructiveReason::DeleteWithoutWhere
        } else if sig[i].is_keyword(src, "UPDATE") {
            DestructiveReason::UpdateWithoutWhere
        } else {
            continue;
        };
        let mut depth = 0i32;
        let mut has_where = false;
        for t in &sig[i + 1..] {
            match t.kind {
                TokenKind::OpenParen => depth += 1,
                TokenKind::CloseParen if depth == 0 => break,
                TokenKind::CloseParen => depth -= 1,
                TokenKind::Word if depth == 0 && t.is_keyword(src, "WHERE") => {
                    has_where = true;
                    break;
                }
                _ => {}
            }
        }
        if !has_where {
            return Some(reason);
        }
    }
    None
}

/// Converts byte offsets to UTF-16 offsets, walking forward only.
struct Utf16Index<'a> {
    src: &'a str,
    byte: usize,
    units: u32,
}

impl<'a> Utf16Index<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, byte: 0, units: 0 }
    }

    fn at(&mut self, byte: usize) -> u32 {
        if byte < self.byte {
            self.byte = 0;
            self.units = 0;
        }
        self.units += self.src[self.byte..byte].chars().map(|c| c.len_utf16() as u32).sum::<u32>();
        self.byte = byte;
        self.units
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: SqlFlavor = SqlFlavor::Postgres;
    const MY: SqlFlavor = SqlFlavor::Mysql;

    fn one(sql: &str) -> StatementInfo {
        analyze(sql, PG).expect("one statement")
    }

    #[test]
    fn splits_and_skips_empty() {
        let src = "select 1;\n -- only comment\n;  select ';' ; ;";
        let parts = split_statements(src, PG);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].text(src), "select 1");
        assert_eq!(parts[1].text(src), "select ';'");
    }

    #[test]
    fn utf16_offsets() {
        let src = "select '✓😀'; select 2";
        let parts = split_statements(src, PG);
        // '😀' is two UTF-16 units, '✓' one.
        assert_eq!(parts[0].to, 12);
        assert_eq!(parts[1].from, 14);
    }

    #[test]
    fn reads_are_not_writes() {
        for sql in [
            "SELECT * FROM t",
            "with x as (select 1) select * from x",
            "SHOW search_path",
            "EXPLAIN DELETE FROM t",
            "VALUES (1)",
            "SELECT * FROM t FOR UPDATE",
        ] {
            let s = one(sql);
            assert!(!s.writes, "{sql}");
            assert_eq!(s.destructive, None, "{sql}");
        }
    }

    #[test]
    fn writes_are_detected() {
        for sql in [
            "insert into t values (1)",
            "update t set a=1 where id=1",
            "create table x(a int)",
            "WITH d AS (DELETE FROM t WHERE a=1 RETURNING *) SELECT * FROM d",
            "select * into newt from t",
            "EXPLAIN ANALYZE UPDATE t SET a = 1 WHERE b",
        ] {
            assert!(one(sql).writes, "{sql}");
        }
    }

    #[test]
    fn destructive_statements() {
        assert_eq!(one("DROP TABLE t").destructive, Some(DestructiveReason::Drop));
        assert_eq!(one("truncate t").destructive, Some(DestructiveReason::Truncate));
        assert_eq!(one("ALTER TABLE t DROP COLUMN a").destructive, Some(DestructiveReason::AlterDrop));
        assert_eq!(one("delete from t").destructive, Some(DestructiveReason::DeleteWithoutWhere));
        assert_eq!(
            one("UPDATE t SET a = (SELECT 1 WHERE true)").destructive,
            Some(DestructiveReason::UpdateWithoutWhere)
        );
        assert_eq!(one("WITH d AS (DELETE FROM t) SELECT 1").destructive, Some(DestructiveReason::DeleteWithoutWhere));
        assert_eq!(one("delete from t where id = 1").destructive, None);
        assert_eq!(one("update t set a = 1 where id in (select 1)").destructive, None);
        assert_eq!(one("-- DROP TABLE x\nselect 'drop table y'").destructive, None);
    }

    #[test]
    fn dml_keywords_in_other_positions_are_ignored() {
        assert_eq!(one("CREATE TABLE c (id int REFERENCES p ON DELETE CASCADE ON UPDATE SET NULL)").destructive, None);
        assert_eq!(one("INSERT INTO t VALUES (1) ON CONFLICT (id) DO UPDATE SET a = 1").destructive, None);
        let my = analyze("INSERT INTO t VALUES (1) ON DUPLICATE KEY UPDATE a = 1", MY).unwrap();
        assert_eq!(my.destructive, None);
        assert!(my.writes);
    }

    #[test]
    fn params_flag() {
        assert!(one("select * from t where id = $1").has_params);
        assert!(!one("select '$1'").has_params);
        assert!(analyze("select ?", MY).unwrap().has_params);
    }
}
