//! A basic SQL formatter: uppercases keywords, puts major clauses on their own
//! lines and indents boolean conditions. It works on lexer tokens, so strings,
//! quoted identifiers, comments and dollar-quoted bodies are never altered.

use super::dialect::SqlFlavor;
use super::lexer::{Token, TokenKind, tokenize};

/// Clauses that start a new line at the current nesting level.
const CLAUSES: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "GROUP BY",
    "ORDER BY",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "UNION",
    "UNION ALL",
    "INTERSECT",
    "EXCEPT",
    "VALUES",
    "SET",
    "RETURNING",
    "WINDOW",
    "INSERT INTO",
    "UPDATE",
    "DELETE FROM",
    "ON CONFLICT",
    "WITH",
];

const JOINS: &[&str] = &[
    "JOIN",
    "INNER JOIN",
    "LEFT JOIN",
    "RIGHT JOIN",
    "FULL JOIN",
    "CROSS JOIN",
    "LEFT OUTER JOIN",
    "RIGHT OUTER JOIN",
    "FULL OUTER JOIN",
    "NATURAL JOIN",
];

const KEYWORDS: &[&str] = &[
    "ADD",
    "ALL",
    "ALTER",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASE",
    "CAST",
    "COLUMN",
    "COMMIT",
    "CONFLICT",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DO",
    "DROP",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "FALSE",
    "FETCH",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "GROUP",
    "HAVING",
    "ILIKE",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTERVAL",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LATERAL",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NATURAL",
    "NOT",
    "NOTHING",
    "NULL",
    "NULLS",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PRIMARY",
    "RECURSIVE",
    "REFERENCES",
    "RETURNING",
    "RIGHT",
    "ROLLBACK",
    "SELECT",
    "SET",
    "TABLE",
    "THEN",
    "TO",
    "TRUE",
    "TRUNCATE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

const INDENT: &str = "  ";

pub fn format_sql(src: &str, flavor: SqlFlavor) -> String {
    let tokens: Vec<Token> = tokenize(src, flavor).into_iter().filter(|t| t.kind != TokenKind::Whitespace).collect();
    let mut out = String::with_capacity(src.len() + src.len() / 4);
    let mut depth = 0usize;
    let mut line_start = true;
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        let text = t.text(src);
        let (phrase, consumed) = keyword_phrase(src, &tokens, i);

        let newline_before = match t.kind {
            TokenKind::Word if CLAUSES.contains(&phrase.as_str()) || JOINS.contains(&phrase.as_str()) => true,
            TokenKind::Word if matches!(phrase.as_str(), "AND" | "OR") && !inside_between(src, &tokens, i) => true,
            _ => false,
        };
        if newline_before && !out.is_empty() && !line_start {
            newline(&mut out, depth);
            if matches!(phrase.as_str(), "AND" | "OR") {
                out.push_str(INDENT);
            }
            line_start = true;
        }

        match t.kind {
            TokenKind::Semicolon => {
                trim_trailing_space(&mut out);
                out.push_str(";\n\n");
                depth = 0;
                line_start = true;
                i += 1;
                continue;
            }
            TokenKind::OpenParen => {
                space_before(&mut out, line_start, text);
                out.push('(');
                depth += 1;
                line_start = false;
                // A subquery starts on its own line.
                if tokens.get(i + 1).is_some_and(|n| n.is_keyword(src, "SELECT") || n.is_keyword(src, "WITH")) {
                    newline(&mut out, depth);
                    line_start = true;
                }
                i += 1;
                continue;
            }
            TokenKind::CloseParen => {
                depth = depth.saturating_sub(1);
                trim_trailing_space(&mut out);
                out.push(')');
                line_start = false;
                i += 1;
                continue;
            }
            TokenKind::Comment => {
                space_before(&mut out, line_start, text);
                out.push_str(text.trim_end());
                if text.starts_with("--") || text.starts_with('#') {
                    newline(&mut out, depth);
                    line_start = true;
                } else {
                    line_start = false;
                }
                i += 1;
                continue;
            }
            _ => {}
        }

        let rendered = if t.kind == TokenKind::Word && KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(text)) {
            phrase.clone()
        } else {
            text.to_owned()
        };
        space_before(&mut out, line_start, &rendered);
        out.push_str(&rendered);
        line_start = false;
        // Clauses put their first item on the same line; long select lists stay readable.
        i += if t.kind == TokenKind::Word && KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(text)) {
            consumed
        } else {
            1
        };
    }
    let mut out = out.trim_end().to_owned();
    if !out.is_empty() && !out.ends_with(';') && src.trim_end().ends_with(';') {
        out.push(';');
    }
    out.push('\n');
    out
}

/// Uppercased keyword, joined with the next keyword for two-word clauses
/// (`GROUP BY`, `LEFT JOIN`, …). Returns the phrase and tokens consumed.
fn keyword_phrase(src: &str, tokens: &[Token], i: usize) -> (String, usize) {
    let word = |j: usize| tokens.get(j).filter(|t| t.kind == TokenKind::Word).map(|t| t.text(src).to_ascii_uppercase());
    let Some(first) = word(i) else { return (String::new(), 1) };
    for len in (2..=3).rev() {
        let words: Option<Vec<String>> = (0..len).map(|k| word(i + k)).collect();
        if let Some(words) = words {
            let phrase = words.join(" ");
            if CLAUSES.contains(&phrase.as_str())
                || JOINS.contains(&phrase.as_str())
                || matches!(phrase.as_str(), "IS NOT" | "NOT NULL" | "IS NULL")
            {
                return (phrase, len);
            }
        }
    }
    (first, 1)
}

/// `x BETWEEN a AND b` keeps its AND inline.
fn inside_between(src: &str, tokens: &[Token], i: usize) -> bool {
    tokens[..i].iter().rev().take(4).any(|t| t.is_keyword(src, "BETWEEN"))
}

fn newline(out: &mut String, depth: usize) {
    trim_trailing_space(out);
    out.push('\n');
    for _ in 0..depth {
        out.push_str(INDENT);
    }
}

fn space_before(out: &mut String, line_start: bool, next: &str) {
    if line_start
        || out.is_empty()
        || out.ends_with(['(', ' ', '\n'])
        || next.starts_with([',', ')', '.', ':'])
        || out.ends_with(['.', ':'])
    {
        return;
    }
    out.push(' ');
}

fn trim_trailing_space(out: &mut String) {
    while out.ends_with(' ') {
        out.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_clauses_and_conditions() {
        let src = "select o.id, c.email from public.orders o join public.customers c on c.id = o.customer_id where o.status = 'paid' and o.total between 1 and 5 order by o.created_at desc;";
        assert_eq!(
            format_sql(src, SqlFlavor::Postgres),
            "SELECT o.id, c.email\nFROM public.orders o\nJOIN public.customers c ON c.id = o.customer_id\nWHERE o.status = 'paid'\n  AND o.total BETWEEN 1 AND 5\nORDER BY o.created_at DESC;\n"
        );
    }

    #[test]
    fn preserves_literals_and_comments() {
        let src = "-- keep me\nselect 'select from where', \"Mixed Case\", $$ and or $$ from t";
        let out = format_sql(src, SqlFlavor::Postgres);
        assert!(
            out.starts_with("-- keep me\nSELECT 'select from where', \"Mixed Case\", $$ and or $$\nFROM t"),
            "{out}"
        );
    }

    #[test]
    fn indents_subqueries() {
        let out = format_sql("select * from (select id from t where a = 1) x", SqlFlavor::Postgres);
        assert_eq!(out, "SELECT *\nFROM (\n  SELECT id\n  FROM t\n  WHERE a = 1) x\n");
    }

    #[test]
    fn separates_statements() {
        assert_eq!(format_sql("select 1; select 2;", SqlFlavor::Mysql), "SELECT 1;\n\nSELECT 2;\n");
    }
}
