//! `MongoDB` shell statements: where they end and what they do.
//!
//! Only the shape matters here (`db.<collection>.<method>(…)`, `db.<method>(…)`,
//! `show …`, `use …`); the adapter parses the arguments itself.

use super::analyze::{DestructiveReason, StatementKind};
use super::lexer::{Token, TokenKind};

/// Bracket depth change of one token: `(`, `{`, `[` open; `)`, `}`, `]` close.
pub(super) fn depth_delta(src: &str, t: &Token) -> i32 {
    match t.kind {
        TokenKind::OpenParen => 1,
        TokenKind::CloseParen => -1,
        TokenKind::Punct => match t.text(src) {
            "{" | "[" => 1,
            "}" | "]" => -1,
            _ => 0,
        },
        _ => 0,
    }
}

/// A line break ends a statement when brackets are balanced, the line does not
/// end in an operator, and the next line does not continue a method chain.
pub(super) fn line_ends_statement(src: &str, tokens: &[Token], i: usize, depth: i32) -> bool {
    if depth > 0 || tokens[i].kind != TokenKind::Whitespace || !tokens[i].text(src).contains('\n') {
        return false;
    }
    let prev = tokens[..i].iter().rev().find(|t| !t.is_trivia());
    let next = tokens[i + 1..].iter().find(|t| !t.is_trivia());
    let (Some(prev), Some(next)) = (prev, next) else {
        return false;
    };
    let continues = |t: &Token| t.kind == TokenKind::Punct && matches!(t.text(src), "." | "," | ":" | "=" | "+" | "?");
    !continues(prev) && !continues(next)
}

/// What a shell statement does, from its method names.
pub(super) fn classify(src: &str, sig: &[Token]) -> (StatementKind, bool, Option<DestructiveReason>) {
    let words: Vec<&str> = sig.iter().filter(|t| t.kind == TokenKind::Word).map(|t| t.text(src)).collect();
    match words.first().copied() {
        Some(w) if w.eq_ignore_ascii_case("show") => return (StatementKind::Show, false, None),
        Some(w) if w.eq_ignore_ascii_case("use") => return (StatementKind::Use, false, None),
        Some(w) if ["begin", "commit", "rollback"].iter().any(|k| k.eq_ignore_ascii_case(w)) => {
            return (StatementKind::Transaction, false, None);
        }
        Some("db") => {}
        _ => return (StatementKind::Other, true, None),
    }
    // Every `.name(` call in the chain, in order.
    let calls: Vec<(&str, usize)> = sig
        .windows(3)
        .enumerate()
        .filter(|(_, w)| {
            w[0].kind == TokenKind::Punct
                && w[0].text(src) == "."
                && w[1].kind == TokenKind::Word
                && w[2].kind == TokenKind::OpenParen
        })
        .map(|(i, w)| (w[1].text(src), i + 2))
        .collect();
    if calls.iter().any(|(m, _)| *m == "explain") {
        return (StatementKind::Explain, false, None);
    }
    let Some(&(method, open)) =
        calls.iter().find(|(m, _)| !matches!(*m, "getCollection" | "getSiblingDB" | "getMongo"))
    else {
        return (StatementKind::Other, true, None);
    };
    let first_arg_empty = || {
        let arg = first_argument(src, &sig[open..]);
        arg.is_empty() || arg == "{}"
    };
    match method {
        "find" | "findOne" | "countDocuments" | "estimatedDocumentCount" | "count" | "distinct" | "getIndexes"
        | "indexes" | "stats" | "dataSize" | "storageSize" | "totalIndexSize" | "getCollectionNames"
        | "getCollectionInfos" | "listCollections" | "getName" | "version" | "serverStatus" | "hostInfo"
        | "isMaster" | "hello" | "currentOp" | "getUsers" | "getRoles" | "validate" => (StatementKind::Select, false, None),
        "aggregate" => {
            let writes = sig.iter().any(|t| matches!(t.text(src), "$out" | "$merge" | "\"$out\"" | "\"$merge\""));
            (if writes { StatementKind::Insert } else { StatementKind::Select }, writes, None)
        }
        "insertOne" | "insertMany" | "insert" => (StatementKind::Insert, true, None),
        "bulkWrite" => (StatementKind::Merge, true, None),
        "updateOne" | "replaceOne" | "findOneAndUpdate" | "findOneAndReplace" => (StatementKind::Update, true, None),
        "updateMany" | "update" => {
            (StatementKind::Update, true, first_arg_empty().then_some(DestructiveReason::UpdateAllDocuments))
        }
        "deleteOne" | "findOneAndDelete" => (StatementKind::Delete, true, None),
        "deleteMany" | "remove" => {
            (StatementKind::Delete, true, first_arg_empty().then_some(DestructiveReason::DeleteAllDocuments))
        }
        "createCollection" | "createView" | "createIndex" | "createIndexes" | "createUser" => {
            (StatementKind::Create, true, None)
        }
        "drop" | "dropDatabase" => (StatementKind::Drop, true, Some(DestructiveReason::Drop)),
        "dropIndex" | "dropIndexes" => (StatementKind::Alter, true, Some(DestructiveReason::AlterDrop)),
        "renameCollection" | "hideIndex" | "unhideIndex" => (StatementKind::Alter, true, None),
        // `runCommand` and anything unknown may write.
        _ => (StatementKind::Other, true, None),
    }
}

/// Text of the first argument after the `(` at `open`, trimmed.
fn first_argument<'a>(src: &'a str, from_open: &[Token]) -> &'a str {
    let start = from_open[0].end;
    let mut depth = 0;
    for t in from_open {
        depth += depth_delta(src, t);
        let comma = depth == 1 && t.kind == TokenKind::Punct && t.text(src) == ",";
        if comma || depth == 0 {
            return src[start..t.start].trim();
        }
    }
    src[start..].trim()
}

#[cfg(test)]
mod tests {
    use crate::sql::{DestructiveReason as D, SqlFlavor, StatementKind as K, split_statements};

    fn split(src: &str) -> Vec<(&str, K, bool, Option<D>)> {
        split_statements(src, SqlFlavor::Mongodb)
            .into_iter()
            .map(|s| (s.text(src), s.kind, s.writes, s.destructive))
            .collect()
    }

    #[test]
    fn lines_and_semicolons_split() {
        let src = "db.orders.find({ status: 'paid' })\n  .sort({ total: -1 })\n  .limit(5)\nshow collections; use app\ndb.a.insertOne({\n  x: 1\n})";
        let s = split(src);
        assert_eq!(s.len(), 4);
        assert_eq!(s[0].0, "db.orders.find({ status: 'paid' })\n  .sort({ total: -1 })\n  .limit(5)");
        assert_eq!((s[0].1, s[0].2), (K::Select, false));
        assert_eq!(s[1].1, K::Show);
        assert_eq!(s[2].1, K::Use);
        assert_eq!((s[3].1, s[3].2), (K::Insert, true));
    }

    #[test]
    fn strings_and_comments_hide_separators() {
        let s = split("db.a.find({ note: \"x;\\\"y\\n\" }) // ; z\ndb.b.find({})");
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn writes_and_destructive() {
        assert_eq!(split("db.a.deleteMany({})")[0].3, Some(D::DeleteAllDocuments));
        assert_eq!(split("db.a.deleteMany( )")[0].3, Some(D::DeleteAllDocuments));
        assert_eq!(split("db.a.deleteMany({ x: 1 })")[0].3, None);
        assert_eq!(split("db.a.updateMany({}, { $set: { y: 1 } })")[0].3, Some(D::UpdateAllDocuments));
        assert_eq!(split("db.getCollection('a b').drop()")[0].3, Some(D::Drop));
        assert_eq!(split("db.a.aggregate([{ $match: {} }, { $out: 'b' }])")[0].2, true);
        assert_eq!(split("db.a.aggregate([{ $match: {} }])")[0].2, false);
        assert_eq!(split("db.a.find().explain('executionStats')")[0].1, K::Explain);
        assert!(split("db.runCommand({ ping: 1 })")[0].2);
        assert!(split("ObjectId()")[0].2);
    }
}
