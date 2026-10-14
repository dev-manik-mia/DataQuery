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
        let pg = self.flavor == SqlFlavor::Postgres;
        match c {
            b' ' | b'\t' | b'\n' | b'\r' | 0x0c => {
                while matches!(self.peek(0), Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0c)) {
                    self.pos += 1;
                }
                TokenKind::Whitespace
            }
            b'-' if self.peek(1) == Some(b'-') && (pg || self.peek(2).is_none_or(|c| c.is_ascii_whitespace())) => {
                self.line_comment()
            }
            b'#' if !pg => self.line_comment(),
            b'/' if self.peek(1) == Some(b'*') => self.block_comment(pg),
            b'\'' => self.quoted(b'\'', !pg),
            b'"' if pg => self.quoted(b'"', false).ident(),
            b'"' => self.quoted(b'"', true),
            b'`' if !pg => self.quoted(b'`', false).ident(),
            b'e' | b'E' if pg && self.peek(1) == Some(b'\'') => {
                self.pos += 1;
                self.quoted(b'\'', true)
            }
            b'$' if pg => self.dollar(),
            b'?' if !pg => {
                self.pos += 1;
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
            c if is_word_start(c) => {
                while self.peek(0).is_some_and(is_word_continue) {
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
    }

    #[test]
    fn unterminated_input_does_not_panic() {
        for src in ["'abc", "\"abc", "$x$ abc", "/* abc", "E'\\", "`abc"] {
            let _ = tokenize(src, SqlFlavor::Postgres);
            let _ = tokenize(src, SqlFlavor::Mysql);
        }
    }

    #[test]
    fn unicode_words_and_punct() {
        let toks = kinds("SELECT 'Zoë ✓', naïve → x", SqlFlavor::Postgres);
        assert!(toks.contains(&(TokenKind::Word, "naïve")));
    }
}
