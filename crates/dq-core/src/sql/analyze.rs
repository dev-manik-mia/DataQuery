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
    /// `MongoDB` `deleteMany({})`.
    DeleteAllDocuments,
    /// `MongoDB` `updateMany({}, …)`.
    UpdateAllDocuments,
    /// Redis `FLUSHALL` / `FLUSHDB`.
    FlushData,
    /// Redis commands that change or stop the server (`DEBUG`, `SHUTDOWN`, `CONFIG SET`, `REPLICAOF`, …).
    ServerCommand,
    /// Redis `KEYS`, which blocks the server while it walks every key.
    BlockingKeys,
}

impl DestructiveReason {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Drop => "DROP removes the object and all of its data.",
            Self::Truncate => "TRUNCATE removes every row.",
            Self::AlterDrop => "ALTER … DROP removes part of the object and its data.",
            Self::DeleteWithoutWhere => "DELETE without WHERE removes every row.",
            Self::UpdateWithoutWhere => "UPDATE without WHERE changes every row.",
            Self::DeleteAllDocuments => "An empty filter deletes every document in the collection.",
            Self::UpdateAllDocuments => "An empty filter changes every document in the collection.",
            Self::FlushData => "FLUSHALL and FLUSHDB delete every key.",
            Self::ServerCommand => "This command changes, blocks or stops the server.",
            Self::BlockingKeys => "KEYS blocks the server while it walks every key. The key browser uses SCAN instead.",
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
    /// Values the statement needs: the highest `$n`, or the number of `?`.
    pub param_count: u32,
}

impl StatementInfo {
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.byte_start..self.byte_end]
    }
}

/// How a statement changes the session's transaction state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxEffect {
    /// `BEGIN`, `START TRANSACTION`, `COMMIT AND CHAIN`.
    Begin,
    /// `COMMIT`, `ROLLBACK`, `END`, `ABORT`, or a MySQL statement that commits implicitly.
    End,
    /// `ROLLBACK TO SAVEPOINT`: a failed transaction becomes usable again.
    RollbackToSavepoint,
    None,
}

/// The transaction effect of one statement.
pub fn transaction_effect(sql: &str, flavor: SqlFlavor) -> TxEffect {
    let tokens = tokenize(sql, flavor);
    let words: Vec<String> =
        tokens.iter().filter(|t| !t.is_trivia()).take(4).map(|t| t.text(sql).to_ascii_uppercase()).collect();
    let word = |i: usize| words.get(i).map_or("", String::as_str);
    match flavor {
        SqlFlavor::Sqlserver => match word(0) {
            "BEGIN" if matches!(word(1), "TRAN" | "TRANSACTION" | "DISTRIBUTED") => TxEffect::Begin,
            "COMMIT" | "ROLLBACK" => TxEffect::End,
            _ => TxEffect::None,
        },
        SqlFlavor::Oracle => match word(0) {
            "SET" if word(1) == "TRANSACTION" => TxEffect::Begin,
            "ROLLBACK" if word(1) == "TO" || (word(1) == "WORK" && word(2) == "TO") => TxEffect::RollbackToSavepoint,
            // DDL commits the open transaction before and after it runs.
            "COMMIT" | "ROLLBACK" | "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "GRANT" | "REVOKE"
            | "COMMENT" | "AUDIT" | "NOAUDIT" | "ANALYZE" | "PURGE" | "FLASHBACK" => TxEffect::End,
            _ => TxEffect::None,
        },
        SqlFlavor::Snowflake => match word(0) {
            "BEGIN" if matches!(word(1), "" | ";" | "TRANSACTION" | "WORK" | "NAME") => TxEffect::Begin,
            "START" if word(1) == "TRANSACTION" => TxEffect::Begin,
            // DDL commits the open transaction.
            "COMMIT" | "ROLLBACK" | "CREATE" | "ALTER" | "DROP" | "UNDROP" | "GRANT" | "REVOKE" | "COMMENT" => {
                TxEffect::End
            }
            _ => TxEffect::None,
        },
        SqlFlavor::Clickhouse => TxEffect::None,
        SqlFlavor::Redis => match word(0) {
            "MULTI" => TxEffect::Begin,
            "EXEC" | "DISCARD" => TxEffect::End,
            _ => TxEffect::None,
        },
        SqlFlavor::Mongodb | SqlFlavor::Partiql => match word(0) {
            "BEGIN" => TxEffect::Begin,
            "COMMIT" | "ROLLBACK" => TxEffect::End,
            _ => TxEffect::None,
        },
        SqlFlavor::Postgres | SqlFlavor::Mysql | SqlFlavor::Duckdb => match word(0) {
            "BEGIN" | "START" => TxEffect::Begin,
            "COMMIT" | "END" | "ROLLBACK" if word(1) == "AND" && word(2) == "CHAIN" => TxEffect::Begin,
            "ROLLBACK" if word(1) == "TO" || (word(2) == "TO" && matches!(word(1), "WORK" | "TRANSACTION")) => {
                TxEffect::RollbackToSavepoint
            }
            "COMMIT" | "ROLLBACK" | "END" | "ABORT" => TxEffect::End,
            "PREPARE" if word(1) == "TRANSACTION" => TxEffect::End,
            // MySQL commits the open transaction before DDL and account statements.
            "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "GRANT" | "REVOKE" | "LOCK" | "UNLOCK"
                if flavor == SqlFlavor::Mysql =>
            {
                TxEffect::End
            }
            _ => TxEffect::None,
        },
    }
}

/// Replaces string literals with `'…'`, for history entries when the user
/// asked not to keep values.
pub fn redact_literals(sql: &str, flavor: SqlFlavor) -> String {
    let mut out = String::with_capacity(sql.len());
    for token in tokenize(sql, flavor) {
        if token.kind == TokenKind::String {
            out.push_str("'…'");
        } else {
            out.push_str(token.text(sql));
        }
    }
    out
}

/// Splits `src` into statements, skipping empty and comment-only ones.
///
/// `;` ends a statement except inside T-SQL `BEGIN … END` blocks and routine
/// bodies, and inside Oracle PL/SQL units. A line holding only `GO` (T-SQL) or
/// `/` (Oracle) also ends one, and is the only end of a routine body.
pub fn split_statements(src: &str, flavor: SqlFlavor) -> Vec<StatementInfo> {
    let tokens = tokenize(src, flavor);
    let mut out = Vec::new();
    let mut utf16 = Utf16Index::new(src);
    let mut chunk_start = 0;
    let mut block = Block::default();
    let mongo = flavor == SqlFlavor::Mongodb;
    let redis = flavor == SqlFlavor::Redis;
    let mut depth = 0;
    for (i, tok) in tokens.iter().enumerate() {
        if mongo {
            depth += super::mongo::depth_delta(src, tok);
        }
        let ends = match tok.kind {
            TokenKind::Semicolon if mongo => depth <= 0,
            // One command per line; `;` is an ordinary character.
            TokenKind::Semicolon if redis => false,
            TokenKind::Whitespace if redis => tok.text(src).contains('\n'),
            TokenKind::Whitespace if mongo => super::mongo::line_ends_statement(src, &tokens, i, depth),
            TokenKind::Semicolon => !block.inside(),
            TokenKind::Word | TokenKind::Punct => is_separator_line(src, &tokens, i, flavor),
            _ => false,
        };
        if ends {
            push_statement(src, &tokens[chunk_start..i], flavor, &mut utf16, &mut out);
            chunk_start = i + 1;
            block = Block::default();
            depth = 0;
        } else if tok.kind == TokenKind::Word {
            block.observe(src, &tokens, i, flavor);
        }
    }
    push_statement(src, &tokens[chunk_start..], flavor, &mut utf16, &mut out);
    out
}

/// Tracks whether the statement being read is inside a block whose `;` do not end it.
#[derive(Default)]
struct Block {
    /// Significant words seen so far, up to a few, to recognise routine headers.
    head: Vec<String>,
    /// T-SQL `BEGIN`/`CASE` depth.
    depth: u32,
    /// A routine or PL/SQL unit: only `GO` / `/` ends it.
    routine: bool,
    /// A Snowflake Scripting `DECLARE` section, open until its block's `END`.
    declare: bool,
}

impl Block {
    fn inside(&self) -> bool {
        self.routine || self.depth > 0 || self.declare
    }

    fn observe(&mut self, src: &str, tokens: &[Token], i: usize, flavor: SqlFlavor) {
        let word = tokens[i].text(src).to_ascii_uppercase();
        if self.head.len() < 6 {
            self.head.push(word.clone());
            self.routine = self.routine || starts_routine(&self.head, flavor);
        }
        if flavor == SqlFlavor::Snowflake {
            self.observe_scripting(src, tokens, i, &word);
            return;
        }
        if flavor != SqlFlavor::Sqlserver {
            return;
        }
        let next = tokens[i + 1..]
            .iter()
            .find(|t| !t.is_trivia())
            .filter(|t| t.kind == TokenKind::Word)
            .map(|t| t.text(src).to_ascii_uppercase());
        match word.as_str() {
            "BEGIN"
                if !matches!(
                    next.as_deref(),
                    Some("TRAN" | "TRANSACTION" | "DISTRIBUTED" | "DIALOG" | "CONVERSATION")
                ) =>
            {
                self.depth += 1;
            }
            "CASE" => self.depth += 1,
            "END" if next.as_deref() != Some("CONVERSATION") => self.depth = self.depth.saturating_sub(1),
            _ => {}
        }
    }
}

impl Block {
    /// Snowflake Scripting: `DECLARE … BEGIN … END` and `BEGIN … END` blocks.
    /// `BEGIN` alone or with `TRANSACTION` / `WORK` / `NAME` starts a transaction instead.
    fn observe_scripting(&mut self, src: &str, tokens: &[Token], i: usize, word: &str) {
        let next = tokens[i + 1..].iter().find(|t| !t.is_trivia());
        let next_word = next.filter(|t| t.kind == TokenKind::Word).map(|t| t.text(src).to_ascii_uppercase());
        match word {
            "DECLARE" if self.head.len() == 1 => self.declare = true,
            "BEGIN"
                if next.is_some_and(|t| t.kind != TokenKind::Semicolon)
                    && !matches!(next_word.as_deref(), Some("TRANSACTION" | "WORK" | "NAME")) =>
            {
                self.depth += 1;
            }
            "CASE" => self.depth += 1,
            "END" if !matches!(next_word.as_deref(), Some("IF" | "FOR" | "LOOP" | "WHILE" | "REPEAT")) => {
                self.depth = self.depth.saturating_sub(1);
                if self.depth == 0 {
                    self.declare = false;
                }
            }
            _ => {}
        }
    }
}

/// The statement's first words open a routine body (T-SQL) or a PL/SQL unit (Oracle).
fn starts_routine(head: &[String], flavor: SqlFlavor) -> bool {
    let words: Vec<&str> = head
        .iter()
        .map(String::as_str)
        .filter(|w| !matches!(*w, "OR" | "REPLACE" | "EDITIONABLE" | "NONEDITIONABLE" | "EDITIONING"))
        .collect();
    match (flavor, words.as_slice()) {
        (SqlFlavor::Sqlserver, ["CREATE" | "ALTER", "ALTER", kind, ..] | ["CREATE" | "ALTER", kind, ..]) => {
            matches!(*kind, "PROCEDURE" | "PROC" | "FUNCTION" | "TRIGGER")
        }
        (SqlFlavor::Oracle, ["DECLARE" | "BEGIN", ..]) => true,
        (SqlFlavor::Oracle, ["CREATE", kind, ..]) => {
            matches!(*kind, "PROCEDURE" | "FUNCTION" | "PACKAGE" | "TRIGGER" | "TYPE" | "LIBRARY")
        }
        _ => false,
    }
}

/// A line holding only `GO` (T-SQL) or `/` (Oracle).
fn is_separator_line(src: &str, tokens: &[Token], i: usize, flavor: SqlFlavor) -> bool {
    let text = tokens[i].text(src);
    let separator = match flavor {
        SqlFlavor::Sqlserver => tokens[i].kind == TokenKind::Word && text.eq_ignore_ascii_case("GO"),
        SqlFlavor::Oracle => tokens[i].kind == TokenKind::Punct && text == "/",
        SqlFlavor::Postgres
        | SqlFlavor::Mysql
        | SqlFlavor::Clickhouse
        | SqlFlavor::Duckdb
        | SqlFlavor::Snowflake
        | SqlFlavor::Mongodb
        | SqlFlavor::Partiql
        | SqlFlavor::Redis => false,
    };
    if !separator {
        return false;
    }
    let line_start = src[..tokens[i].start].rfind('\n').map_or(0, |n| n + 1);
    let line_end = src[tokens[i].end..].find('\n').map_or(src.len(), |n| tokens[i].end + n);
    src[line_start..tokens[i].start].trim().is_empty() && {
        let rest = src[tokens[i].end..line_end].trim();
        rest.is_empty() || rest.starts_with("--")
    }
}

/// Classifies a single statement (any trailing `;` is ignored).
pub fn analyze(sql: &str, flavor: SqlFlavor) -> Option<StatementInfo> {
    split_statements(sql, flavor).into_iter().next()
}

/// Upper-cased first keyword of a statement, e.g. `SELECT`, used as the command tag.
pub fn command_tag(sql: &str, flavor: SqlFlavor) -> String {
    analyze(sql, flavor)
        .map(|s| sql[s.byte_start..].split_whitespace().next().unwrap_or("").to_ascii_uppercase())
        .unwrap_or_default()
}

fn push_statement(
    src: &str,
    tokens: &[Token],
    flavor: SqlFlavor,
    utf16: &mut Utf16Index,
    out: &mut Vec<StatementInfo>,
) {
    let Some(first) = tokens.iter().position(|t| !t.is_trivia()) else {
        return;
    };
    let last = tokens.iter().rposition(|t| !t.is_trivia()).unwrap_or(first);
    let body = &tokens[first..=last];
    let sig: Vec<Token> = body.iter().copied().filter(|t| !t.is_trivia()).collect();
    let (kind, writes, destructive) = classify(src, &sig, flavor);
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
        param_count: param_count(src, &sig),
    });
}

/// Distinct `ClickHouse` `{name:Type}` parameters, in order of first appearance.
pub fn named_params(sql: &str, flavor: SqlFlavor) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for t in tokenize(sql, flavor).iter().filter(|t| t.kind == TokenKind::Param) {
        let text = t.text(sql);
        if let Some(name) = text.strip_prefix('{').and_then(|r| r.split(':').next())
            && !names.iter().any(|n| n == name)
        {
            names.push(name.to_owned());
        }
    }
    names
}

fn param_count(src: &str, sig: &[Token]) -> u32 {
    let params = sig.iter().filter(|t| t.kind == TokenKind::Param).map(|t| t.text(src));
    let mut questions = 0;
    let mut highest = 0;
    let mut names: Vec<&str> = Vec::new();
    for p in params {
        if let Some(name) = p.strip_prefix('{').and_then(|r| r.split(':').next()) {
            if !names.contains(&name) {
                names.push(name);
            }
            continue;
        }
        let numbered =
            p.strip_prefix('$').or_else(|| p.strip_prefix(':')).or_else(|| p.get(2..).filter(|_| p.starts_with('@')));
        match numbered.and_then(|n| n.parse::<u32>().ok()) {
            Some(n) => highest = highest.max(n),
            None => questions += 1,
        }
    }
    highest.max(questions).max(names.len() as u32)
}

fn classify(src: &str, sig: &[Token], flavor: SqlFlavor) -> (StatementKind, bool, Option<DestructiveReason>) {
    if flavor == SqlFlavor::Mongodb {
        return super::mongo::classify(src, sig);
    }
    if flavor == SqlFlavor::Redis {
        let text = sig.first().zip(sig.last()).map_or("", |(a, b)| &src[a.start..b.end]);
        return super::redis::classify(text);
    }
    let word = |i: usize| sig.get(i).filter(|t| t.kind == TokenKind::Word).map(|t| t.text(src).to_ascii_uppercase());
    let head = word(0).unwrap_or_default();
    let mut kind = keyword_kind(&head);
    match (flavor, head.as_str()) {
        // A T-SQL `BEGIN` without `TRAN` opens a block; Oracle `BEGIN`/`DECLARE` run PL/SQL.
        (SqlFlavor::Sqlserver, "BEGIN")
            if !matches!(word(1).as_deref(), Some("TRAN" | "TRANSACTION" | "DISTRIBUTED")) =>
        {
            kind = StatementKind::Other;
        }
        (SqlFlavor::Sqlserver, "SAVE") => kind = StatementKind::Transaction,
        (SqlFlavor::Sqlserver, "END") => kind = StatementKind::Other,
        (SqlFlavor::Oracle, "BEGIN" | "DECLARE") | (SqlFlavor::Snowflake, "DECLARE") => kind = StatementKind::Call,
        (SqlFlavor::Snowflake, "BEGIN")
            if word(1).is_some_and(|w| !matches!(w.as_str(), "TRANSACTION" | "WORK" | "NAME")) =>
        {
            kind = StatementKind::Call;
        }
        (SqlFlavor::Oracle, "SET") if word(1).as_deref() == Some("TRANSACTION") => kind = StatementKind::Transaction,
        _ => {}
    }

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
        "CALL" | "DO" | "EXECUTE" | "EXEC" => StatementKind::Call,
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
    fn transaction_effects() {
        assert_eq!(transaction_effect("begin", PG), TxEffect::Begin);
        assert_eq!(transaction_effect("START TRANSACTION READ ONLY", MY), TxEffect::Begin);
        assert_eq!(transaction_effect("commit", PG), TxEffect::End);
        assert_eq!(transaction_effect("COMMIT AND CHAIN", PG), TxEffect::Begin);
        assert_eq!(transaction_effect("rollback to savepoint s1", PG), TxEffect::RollbackToSavepoint);
        assert_eq!(transaction_effect("ROLLBACK WORK TO s1", MY), TxEffect::RollbackToSavepoint);
        assert_eq!(transaction_effect("rollback", MY), TxEffect::End);
        assert_eq!(transaction_effect("create table t (a int)", MY), TxEffect::End);
        assert_eq!(transaction_effect("create table t (a int)", PG), TxEffect::None);
        assert_eq!(transaction_effect("select 1", PG), TxEffect::None);
    }

    #[test]
    fn literals_are_redacted() {
        assert_eq!(
            redact_literals("select * from t where a = 'x''y' and b = 1", PG),
            "select * from t where a = '…' and b = 1"
        );
        assert_eq!(redact_literals("select $$secret$$", PG), "select '…'");
    }

    #[test]
    fn params_flag() {
        assert_eq!(one("select * from t where id = $2 or id = $1").param_count, 2);
        assert_eq!(analyze("select ?, ?, '?'", MY).unwrap().param_count, 2);
        assert!(one("select * from t where id = $1").has_params);
        assert!(!one("select '$1'").has_params);
        assert!(analyze("select ?", MY).unwrap().has_params);
    }

    const MS: SqlFlavor = SqlFlavor::Sqlserver;
    const ORA: SqlFlavor = SqlFlavor::Oracle;

    fn texts(src: &str, flavor: SqlFlavor) -> Vec<&str> {
        split_statements(src, flavor).iter().map(|s| s.text(src)).collect::<Vec<_>>()
    }

    #[test]
    fn tsql_blocks_and_go() {
        let src = "IF 1 = 1 BEGIN UPDATE t SET a = 1; DELETE FROM u WHERE id = 2; END;\nSELECT [a;b] FROM t\nGO\nCREATE PROCEDURE p AS\nSELECT 1;\nSELECT 2;\ngo\nBEGIN TRAN; COMMIT";
        assert_eq!(
            texts(src, MS),
            vec![
                "IF 1 = 1 BEGIN UPDATE t SET a = 1; DELETE FROM u WHERE id = 2; END",
                "SELECT [a;b] FROM t",
                "CREATE PROCEDURE p AS\nSELECT 1;\nSELECT 2;",
                "BEGIN TRAN",
                "COMMIT",
            ]
        );
        assert_eq!(analyze("SELECT @P1, @P3, @x", MS).unwrap().param_count, 3);
        assert_eq!(transaction_effect("begin transaction", MS), TxEffect::Begin);
        assert_eq!(transaction_effect("BEGIN SELECT 1 END", MS), TxEffect::None);
        assert_eq!(analyze("BEGIN TRAN", MS).unwrap().kind, StatementKind::Transaction);
        assert!(analyze("EXEC dbo.purge", MS).unwrap().writes);
        // `#temp` is a table name, not a comment.
        assert_eq!(texts("SELECT * FROM #t; SELECT 1", MS).len(), 2);
    }

    #[test]
    fn plsql_units_end_at_slash() {
        let src = "SELECT q'[it's; fine]' FROM dual;\nBEGIN\n  UPDATE t SET a = :1;\n  COMMIT;\nEND;\n/\nCREATE OR REPLACE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW\nBEGIN :new.id := 1; END;\n/\nSELECT 10 / 2 FROM dual";
        assert_eq!(
            texts(src, ORA),
            vec![
                "SELECT q'[it's; fine]' FROM dual",
                "BEGIN\n  UPDATE t SET a = :1;\n  COMMIT;\nEND;",
                "CREATE OR REPLACE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW\nBEGIN :new.id := 1; END;",
                "SELECT 10 / 2 FROM dual",
            ]
        );
        let block = split_statements(src, ORA);
        assert_eq!(block[1].param_count, 1);
        assert_eq!(block[2].param_count, 0, ":new is not a bind");
        assert_eq!(block[1].kind, StatementKind::Call);
        assert_eq!(transaction_effect("CREATE TABLE x (a NUMBER)", ORA), TxEffect::End);
        assert_eq!(transaction_effect("SET TRANSACTION READ WRITE", ORA), TxEffect::Begin);
        assert_eq!(transaction_effect("rollback to savepoint a", ORA), TxEffect::RollbackToSavepoint);
    }

    #[test]
    fn snowflake_scripting_and_clickhouse_params() {
        const SF: SqlFlavor = SqlFlavor::Snowflake;
        const CH: SqlFlavor = SqlFlavor::Clickhouse;
        let src =
            "BEGIN;\nDECLARE n INT;\nBEGIN\n  n := 1;\n  IF (n = 1) THEN RETURN n; END IF;\nEND;\nSELECT 1;\nCOMMIT";
        assert_eq!(
            texts(src, SF),
            vec![
                "BEGIN",
                "DECLARE n INT;\nBEGIN\n  n := 1;\n  IF (n = 1) THEN RETURN n; END IF;\nEND",
                "SELECT 1",
                "COMMIT"
            ]
        );
        assert_eq!(transaction_effect("BEGIN", SF), TxEffect::Begin);
        assert_eq!(transaction_effect("CREATE TABLE t (a INT)", SF), TxEffect::End);
        assert_eq!(analyze("SELECT ?, :2", SF).unwrap().param_count, 2);
        let sql = "SELECT * FROM t WHERE a = {a:UInt64} AND b = {b:String} OR a = {a:UInt64}";
        assert_eq!(analyze(sql, CH).unwrap().param_count, 2);
        assert_eq!(named_params(sql, CH), ["a", "b"]);
        assert_eq!(transaction_effect("BEGIN TRANSACTION", SqlFlavor::Duckdb), TxEffect::Begin);
    }
}
