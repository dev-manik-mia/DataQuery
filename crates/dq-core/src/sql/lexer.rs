//! A small, allocation-free SQL tokenizer.
//!
//! It only needs to be good enough to find statement boundaries and keywords,
//! so it recognises quoting and comment rules exactly and treats everything
//! else as words, numbers or punctuation.

use super::dialect::SqlFlavor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Whitespace,
    Comment,
    Word,
    QuotedIdent,
    String,
    Number,
    Param,
    Semicolon,
    OpenParen,
    CloseParen,
    Punct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
}

impl Token {
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start..self.end]
    }

    pub fn is_trivia(&self) -> bool {
        matches!(self.kind, TokenKind::Whitespace | TokenKind::Comment)
    }

    /// True when this is the unquoted keyword `kw` (case-insensitive).
    pub fn is_keyword(&self, src: &str, kw: &str) -> bool {
        self.kind == TokenKind::Word && self.text(src).eq_ignore_ascii_case(kw)
    }
}

pub fn tokenize(src: &str, flavor: SqlFlavor) -> Vec<Token> {
    let mut lexer = Lexer { src, bytes: src.as_bytes(), pos: 0, flavor };
    let mut out = Vec::new();
    while lexer.pos < lexer.bytes.len() {
        let start = lexer.pos;
        let kind = lexer.next_kind();
        out.push(Token { kind, start, end: lexer.pos });
    }
    out
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    flavor: SqlFlavor,
}

impl Lexer<'_> {
    fn peek(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    fn next_kind(&mut self) -> TokenKind {
        let c = self.bytes[self.pos];
        let flavor = self.flavor;
        let vertica = flavor == SqlFlavor::Vertica;
        // Vertica lexes like PostgreSQL (`E''` strings, dollar quotes) but binds `?`.
        let (pg, my, ms, ora) = (
            flavor.postgres_like() || vertica,
            flavor == SqlFlavor::Mysql,
            flavor == SqlFlavor::Sqlserver,
            flavor == SqlFlavor::Oracle,
        );
        let (ch, sf, duck) =
            (flavor == SqlFlavor::Clickhouse, flavor == SqlFlavor::Snowflake, flavor == SqlFlavor::Duckdb);
        let (mongo, partiql) = (flavor == SqlFlavor::Mongodb, flavor == SqlFlavor::Partiql);
        if flavor == SqlFlavor::Sqlite {
            // `?`, `?NNN`, `:name`, `@name` and `$name` parameters; `` `name` `` and `[name]` identifiers.
            match c {
                b'?' => {
                    self.pos += 1;
                    while self.peek(0).is_some_and(|c| c.is_ascii_digit()) {
                        self.pos += 1;
                    }
                    return TokenKind::Param;
                }
                b':' | b'@' | b'$' if self.peek(1).is_some_and(is_word_continue) => {
                    self.pos += 1;
                    while self.peek(0).is_some_and(is_word_continue) {
                        self.pos += 1;
                    }
                    return TokenKind::Param;
                }
                b'`' => return self.quoted(b'`', false).ident(),
                b'[' => return self.quoted_until(b']'),
                _ => {}
            }
        }
        if flavor == SqlFlavor::Bigquery {
            // `#` comments, `` `name` `` paths, '…' and "…" strings (also triple-quoted and
            // with r/b prefixes), `?` and `@name` parameters; `@@name` is a system variable.
            match c {
                b'#' => return self.line_comment(),
                b'`' => return self.quoted(b'`', true).ident(),
                b'\'' | b'"' => return self.bigquery_string(0),
                b'r' | b'R' | b'b' | b'B' => {
                    let prefixed = |i: usize| matches!(self.peek(i), Some(b'\'' | b'"'));
                    let other = |i: usize| matches!(self.peek(i), Some(b'r' | b'R' | b'b' | b'B'));
                    if prefixed(1) {
                        return self.bigquery_string(1);
                    }
                    if other(1) && !self.peek(1).is_some_and(|n| n.eq_ignore_ascii_case(&c)) && prefixed(2) {
                        return self.bigquery_string(2);
                    }
                }
                b'?' => return self.single(TokenKind::Param),
                b'@' if self.peek(1) == Some(b'@') || self.peek(1).is_some_and(is_word_continue) => {
                    let system = self.peek(1) == Some(b'@');
                    self.pos += if system { 2 } else { 1 };
                    while self.peek(0).is_some_and(is_word_continue) {
                        self.pos += 1;
                    }
                    return if system { TokenKind::Word } else { TokenKind::Param };
                }
                _ => {}
            }
        }
        if flavor == SqlFlavor::QueryDsl {
            // JSON strings; `#` and `//` comment lines.
            let line_start = self.src[..self.pos].trim_end_matches([' ', '\t']).ends_with('\n')
                || self.src[..self.pos].trim().is_empty();
            match c {
                b'"' => return self.quoted(b'"', true),
                b'#' if line_start => return self.line_comment(),
                b'/' if line_start && self.peek(1) == Some(b'/') => return self.line_comment(),
                b'\'' | b'-' | b'$' | b'?' | b'@' | b':' => return self.single(TokenKind::Punct),
                _ => {}
            }
        }
        if flavor == SqlFlavor::Cypher {
            // Backslash-escaped strings, backtick names, `//` comments, `$name` parameters.
            match c {
                b'\'' | b'"' => return self.quoted(c, true),
                b'`' => return self.quoted(b'`', false).ident(),
                b'/' if self.peek(1) == Some(b'/') => return self.line_comment(),
                b'$' if self.peek(1).is_some_and(is_word_continue) => {
                    self.pos += 1;
                    while self.peek(0).is_some_and(is_word_continue) {
                        self.pos += 1;
                    }
                    return TokenKind::Param;
                }
                b'-' => return self.single(TokenKind::Punct),
                _ => {}
            }
        }
        if flavor == SqlFlavor::Cql {
            // `//` comments, `$$…$$` function bodies, `?` markers.
            match c {
                b'/' if self.peek(1) == Some(b'/') => return self.line_comment(),
                b'$' if self.peek(1) == Some(b'$') => {
                    let end = self.src[self.pos + 2..].find("$$").map_or(self.bytes.len(), |i| self.pos + 2 + i + 2);
                    self.pos = end;
                    return TokenKind::String;
                }
                b'?' => return self.single(TokenKind::Param),
                _ => {}
            }
        }
        if flavor == SqlFlavor::Redis {
            // `redis-cli` arguments: quotes with backslash escapes, no comments.
            match c {
                b'\'' | b'"' => return self.quoted(c, true),
                b'-' | b'/' | b'#' | b'$' | b'?' | b'@' | b':' | b'[' | b'{' => return self.single(TokenKind::Punct),
                _ => {}
            }
        }
        if mongo {
            // JavaScript: three string quotes with backslash escapes, `//` comments, `$operators`.
            match c {
                b'\'' | b'"' | b'`' => return self.quoted(c, true),
                b'/' if self.peek(1) == Some(b'/') => return self.line_comment(),
                b'-' => return self.single(TokenKind::Punct),
                b'$' => {
                    self.pos += 1;
                    while self.peek(0).is_some_and(is_word_continue) {
                        self.pos += 1;
                    }
                    return TokenKind::Word;
                }
                _ => {}
            }
        }
        match c {
            b' ' | b'\t' | b'\n' | b'\r' | 0x0c => {
                while matches!(self.peek(0), Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0c)) {
                    self.pos += 1;
                }
                TokenKind::Whitespace
            }
            // MySQL needs whitespace after `--`; the others do not.
            b'-' if self.peek(1) == Some(b'-') && (!my || self.peek(2).is_none_or(|c| c.is_ascii_whitespace())) => {
                self.line_comment()
            }
            b'#' if my || ch => self.line_comment(),
            b'/' if sf && self.peek(1) == Some(b'/') => self.line_comment(),
            b'/' if self.peek(1) == Some(b'*') => self.block_comment(pg || ms),
            // MySQL, ClickHouse and Snowflake read backslash escapes in strings.
            b'\'' => self.quoted(b'\'', my || ch || sf),
            b'"' if my => self.quoted(b'"', true),
            b'"' => self.quoted(b'"', ch).ident(),
            b'`' if my || ch => self.quoted(b'`', ch).ident(),
            b'{' if ch => self.braced_param(),
            b'[' if ms => self.quoted_until(b']'),
            b'e' | b'E' if pg && self.peek(1) == Some(b'\'') => {
                self.pos += 1;
                self.quoted(b'\'', true)
            }
            b'q' | b'Q' if ora && self.peek(1) == Some(b'\'') => self.q_quote(1),
            b'n' | b'N' if ora && matches!(self.peek(1), Some(b'q' | b'Q')) && self.peek(2) == Some(b'\'') => {
                self.q_quote(2)
            }
            b'$' if pg || (sf && self.peek(1) == Some(b'$')) => self.dollar(),
            b'?' if my || duck || sf || partiql || vertica => self.single(TokenKind::Param),
            b'@' if ms => self.at_sign(),
            b':' if (ora || sf) && self.peek(1).is_some_and(|c| c.is_ascii_digit()) => {
                self.pos += 1;
                while self.peek(0).is_some_and(|c| c.is_ascii_digit()) {
                    self.pos += 1;
                }
                TokenKind::Param
            }
            b';' => self.single(TokenKind::Semicolon),
            b'(' => self.single(TokenKind::OpenParen),
            b')' => self.single(TokenKind::CloseParen),
            b'0'..=b'9' => {
                while matches!(
                    self.peek(0),
                    Some(b'0'..=b'9' | b'.' | b'e' | b'E' | b'_' | b'x' | b'X' | b'a'..=b'f' | b'A'..=b'F')
                ) {
                    self.pos += 1;
                }
                TokenKind::Number
            }
            c if is_word_start(c) || (ms && c == b'#') => {
                self.pos += 1;
                while self.peek(0).is_some_and(|c| is_word_continue(c) || (ora && c == b'#')) {
                    self.pos += 1;
                }
                TokenKind::Word
            }
            _ => {
                // Advance one full UTF-8 character.
                let ch = self.src[self.pos..].chars().next().map_or(1, char::len_utf8);
                self.pos += ch;
                TokenKind::Punct
            }
        }
    }

    /// T-SQL `@P1` parameters (as the driver binds them); other `@name` and
    /// `@@name` words are variables and functions.
    fn at_sign(&mut self) -> TokenKind {
        let start = self.pos;
        self.pos += 1;
        while self.peek(0).is_some_and(|c| is_word_continue(c) || c == b'@') {
            self.pos += 1;
        }
        let text = &self.src[start + 1..self.pos];
        let param = text.len() > 1
            && text.as_bytes()[0].eq_ignore_ascii_case(&b'p')
            && text.bytes().skip(1).all(|c| c.is_ascii_digit());
        if param { TokenKind::Param } else { TokenKind::Word }
    }

    /// `ClickHouse` query parameter `{name:Type}`; any other `{` is punctuation.
    fn braced_param(&mut self) -> TokenKind {
        let rest = &self.src[self.pos + 1..];
        let name_len = rest.bytes().take_while(|&c| c.is_ascii_alphanumeric() || c == b'_').count();
        if name_len > 0
            && rest.as_bytes().get(name_len) == Some(&b':')
            && let Some(end) = rest.find('}')
        {
            self.pos += end + 2;
            return TokenKind::Param;
        }
        self.single(TokenKind::Punct)
    }

    /// `[name]` with `]]` as an escaped bracket (T-SQL).
    fn quoted_until(&mut self, close: u8) -> TokenKind {
        self.pos += 1;
        while let Some(c) = self.peek(0) {
            self.pos += 1;
            if c == close {
                if self.peek(0) == Some(close) {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }
        TokenKind::QuotedIdent
    }

    /// Oracle alternative quoting: `q'[it's]'`, `q'{…}'`, `q'!…!'`. `prefix`
    /// is the length of `q` or `nq`.
    fn q_quote(&mut self, prefix: usize) -> TokenKind {
        let open = self.pos + prefix + 1;
        let Some(&delimiter) = self.bytes.get(open) else {
            self.pos = self.bytes.len();
            return TokenKind::String;
        };
        let close = match delimiter {
            b'[' => b']',
            b'{' => b'}',
            b'(' => b')',
            b'<' => b'>',
            other => other,
        };
        let body = open + 1;
        let end = self.bytes[body.min(self.bytes.len())..]
            .windows(2)
            .position(|w| w[0] == close && w[1] == b'\'')
            .map_or(self.bytes.len(), |i| body + i + 2);
        self.pos = end;
        TokenKind::String
    }

    /// A `GoogleSQL` string after `prefix` bytes of `r`/`b`: `'…'`, `"…"`, `'''…'''` or
    /// `"""…"""`. A backslash keeps the next byte inside the literal, raw strings included.
    fn bigquery_string(&mut self, prefix: usize) -> TokenKind {
        self.pos += prefix;
        let quote = self.bytes[self.pos];
        if self.peek(1) != Some(quote) || self.peek(2) != Some(quote) {
            return self.quoted(quote, true);
        }
        self.pos += 3;
        while let Some(c) = self.peek(0) {
            if c == b'\\' {
                self.pos = (self.pos + 2).min(self.bytes.len());
            } else if c == quote && self.peek(1) == Some(quote) && self.peek(2) == Some(quote) {
                self.pos += 3;
                break;
            } else {
                self.pos += 1;
            }
        }
        TokenKind::String
    }

    fn single(&mut self, kind: TokenKind) -> TokenKind {
        self.pos += 1;
        kind
    }

    fn line_comment(&mut self) -> TokenKind {
        while self.peek(0).is_some_and(|c| c != b'\n') {
            self.pos += 1;
        }
        TokenKind::Comment
    }

    fn block_comment(&mut self, nested: bool) -> TokenKind {
        self.pos += 2;
        let mut depth = 1;
        while self.pos < self.bytes.len() {
            if self.peek(0) == Some(b'*') && self.peek(1) == Some(b'/') {
                self.pos += 2;
                depth -= 1;
                if depth == 0 {
                    break;
                }
            } else if nested && self.peek(0) == Some(b'/') && self.peek(1) == Some(b'*') {
                self.pos += 2;
                depth += 1;
            } else {
                self.pos += 1;
            }
        }
        TokenKind::Comment
    }

    /// Consumes a quoted run. A doubled quote is an escaped quote; with
    /// `backslash` a backslash escapes the next byte.
    fn quoted(&mut self, quote: u8, backslash: bool) -> TokenKind {
        self.pos += 1;
        while let Some(c) = self.peek(0) {
            self.pos += 1;
            if backslash && c == b'\\' {
                self.pos = (self.pos + 1).min(self.bytes.len());
            } else if c == quote {
                if self.peek(0) == Some(quote) {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }
        TokenKind::String
    }

    fn dollar(&mut self) -> TokenKind {
        // Positional parameter: $1
        if self.peek(1).is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
            while self.peek(0).is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
            }
            return TokenKind::Param;
        }
        // Dollar quote: $tag$ ... $tag$
        let rest = &self.src[self.pos + 1..];
        let tag_len = rest.bytes().take_while(|&c| is_word_continue(c) && c != b'$').count();
        if rest.as_bytes().get(tag_len) == Some(&b'$') && !rest.as_bytes().first().is_some_and(u8::is_ascii_digit) {
            let delimiter = &self.src[self.pos..self.pos + tag_len + 2];
            let body_start = self.pos + delimiter.len();
            self.pos =
                self.src[body_start..].find(delimiter).map_or(self.bytes.len(), |i| body_start + i + delimiter.len());
            return TokenKind::String;
        }
        self.single(TokenKind::Punct)
    }
}

impl TokenKind {
    fn ident(self) -> Self {
        if self == Self::String { Self::QuotedIdent } else { self }
    }
}

fn is_word_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

fn is_word_continue(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str, flavor: SqlFlavor) -> Vec<(TokenKind, &str)> {
        tokenize(src, flavor).into_iter().filter(|t| !t.is_trivia()).map(|t| (t.kind, t.text(src))).collect()
    }

    #[test]
    fn postgres_quoting() {
        let src = r#"SELECT 'a;b', "x;y", $$ ; $$, $fn$ '; $fn$, E'it\'s;' -- c;
        /* a /* nested ; */ ; */ ;"#;
        let toks = kinds(src, SqlFlavor::Postgres);
        let semis = toks.iter().filter(|(k, _)| *k == TokenKind::Semicolon).count();
        assert_eq!(semis, 1, "{toks:?}");
        assert!(toks.contains(&(TokenKind::QuotedIdent, "\"x;y\"")));
        assert!(toks.contains(&(TokenKind::String, "$fn$ '; $fn$")));
    }

    #[test]
    fn mysql_quoting() {
        let src = "SELECT `a;b`, 'it\\'s;', \"q;\" # c;\n -- c;\n;";
        let toks = kinds(src, SqlFlavor::Mysql);
        assert_eq!(toks.iter().filter(|(k, _)| *k == TokenKind::Semicolon).count(), 1, "{toks:?}");
        assert!(toks.contains(&(TokenKind::QuotedIdent, "`a;b`")));
    }

    #[test]
    fn params() {
        assert!(kinds("select $1", SqlFlavor::Postgres).contains(&(TokenKind::Param, "$1")));
        assert!(kinds("select ?", SqlFlavor::Mysql).contains(&(TokenKind::Param, "?")));
        assert!(kinds("select @P2, @x", SqlFlavor::Sqlserver).contains(&(TokenKind::Param, "@P2")));
        assert!(kinds("select @P2, @x", SqlFlavor::Sqlserver).contains(&(TokenKind::Word, "@x")));
        assert!(kinds("select :1 from dual", SqlFlavor::Oracle).contains(&(TokenKind::Param, ":1")));
        assert!(kinds("select {id:UInt64}, {x}", SqlFlavor::Clickhouse).contains(&(TokenKind::Param, "{id:UInt64}")));
        assert!(kinds("select 'a\\';' # c ;", SqlFlavor::Clickhouse).iter().all(|(k, _)| *k != TokenKind::Semicolon));
        assert!(
            kinds("select $$ ; $$, ? // c ;", SqlFlavor::Snowflake).iter().all(|(k, _)| *k != TokenKind::Semicolon)
        );
        assert!(kinds("select ?, $1", SqlFlavor::Duckdb).contains(&(TokenKind::Param, "$1")));
        assert!(!kinds(":new.id", SqlFlavor::Oracle).iter().any(|(k, _)| *k == TokenKind::Param));
    }

    #[test]
    fn bigquery_strings_comments_and_params() {
        const BQ: SqlFlavor = SqlFlavor::Bigquery;
        let src =
            "SELECT \"a;b\", 'it\\'s;', '''x;\n'y';''', r'\\d;', b\"\"\";\"\"\", rb'\\x00;' # c;\n -- c;\n/* ; */;";
        let toks = kinds(src, BQ);
        assert_eq!(toks.iter().filter(|(k, _)| *k == TokenKind::Semicolon).count(), 1, "{toks:?}");
        assert!(toks.contains(&(TokenKind::String, "\"a;b\"")));
        assert!(toks.contains(&(TokenKind::String, "'''x;\n'y';'''")));
        assert!(toks.contains(&(TokenKind::String, "rb'\\x00;'")));
        let names = kinds("SELECT * FROM `my-project.sales`.`or\\`ders`, real", BQ);
        assert!(names.contains(&(TokenKind::QuotedIdent, "`my-project.sales`")));
        assert!(names.contains(&(TokenKind::QuotedIdent, "`or\\`ders`")));
        assert!(names.contains(&(TokenKind::Word, "real")));
        let params = kinds("SELECT @id, ?, @@row_count", BQ);
        assert!(params.contains(&(TokenKind::Param, "@id")) && params.contains(&(TokenKind::Param, "?")));
        assert!(params.contains(&(TokenKind::Word, "@@row_count")));
    }

    #[test]
    fn unterminated_input_does_not_panic() {
        for src in ["'abc", "\"abc", "$x$ abc", "/* abc", "E'\\", "`abc", "{a:", "$$ x", "'''abc", "r'", "b\"\"\"\\"] {
            let _ = tokenize(src, SqlFlavor::Postgres);
            let _ = tokenize(src, SqlFlavor::Mysql);
            let _ = tokenize(src, SqlFlavor::Sqlserver);
            let _ = tokenize(src, SqlFlavor::Oracle);
            let _ = tokenize(src, SqlFlavor::Clickhouse);
            let _ = tokenize(src, SqlFlavor::Duckdb);
            let _ = tokenize(src, SqlFlavor::Snowflake);
            let _ = tokenize(src, SqlFlavor::Bigquery);
        }
    }

    #[test]
    fn unicode_words_and_punct() {
        let toks = kinds("SELECT 'Zoë ✓', naïve → x", SqlFlavor::Postgres);
        assert!(toks.contains(&(TokenKind::Word, "naïve")));
    }
}
