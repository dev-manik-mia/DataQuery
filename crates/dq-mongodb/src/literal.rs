//! `MongoDB` shell literals: a parser for the values users type (objects,
//! arrays, strings, numbers, `ObjectId('…')`, `ISODate('…')`, `Long('…')`,
//! `Decimal128('…')`, regexes, Extended JSON wrappers) and the printer that
//! writes documents back in the same syntax, so an edited document keeps
//! every type and digit. Nothing is evaluated: anything else is an error.

use std::fmt::Write;

use base64::Engine as _;
use dq_core::{DbError, DbResult, ErrorKind};
use mongodb::bson::spec::BinarySubtype;
use mongodb::bson::{
    Binary, Bson, DateTime, Decimal128, Document, JavaScriptCodeWithScope, Regex, Timestamp, oid::ObjectId,
};

/// Deepest nesting of objects and arrays accepted.
const MAX_DEPTH: usize = 100;

/// A cursor over statement text. Positions are byte offsets; errors report
/// 1-based character offsets.
pub struct Scanner<'a> {
    pub src: &'a str,
    pub pos: usize,
}

impl<'a> Scanner<'a> {
    pub fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }

    pub fn error(&self, message: impl AsRef<str>) -> DbError {
        self.error_at(self.pos, message)
    }

    pub fn error_at(&self, pos: usize, message: impl AsRef<str>) -> DbError {
        let column = self.src[..pos.min(self.src.len())].chars().count() as u32 + 1;
        DbError::new(ErrorKind::Syntax, message).with_position(Some(column))
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    pub fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    /// Skips whitespace and `//` / `/* */` comments.
    pub fn skip_ws(&mut self) {
        loop {
            let rest = self.rest();
            let trimmed = rest.trim_start();
            self.pos += rest.len() - trimmed.len();
            if trimmed.starts_with("//") {
                self.pos += trimmed.find('\n').unwrap_or(trimmed.len());
            } else if trimmed.starts_with("/*") {
                self.pos += trimmed.find("*/").map_or(trimmed.len(), |i| i + 2);
            } else {
                return;
            }
        }
    }

    pub fn at_end(&mut self) -> bool {
        self.skip_ws();
        self.pos >= self.src.len()
    }

    /// Consumes `c` after whitespace when it is next.
    pub fn eat(&mut self, c: char) -> bool {
        self.skip_ws();
        if self.peek() == Some(c) {
            self.pos += c.len_utf8();
            true
        } else {
            false
        }
    }

    pub fn expect(&mut self, c: char) -> DbResult<()> {
        if self.eat(c) { Ok(()) } else { Err(self.error(format!("Expected `{c}` here."))) }
    }

    /// A JavaScript identifier (`$` allowed), or `None`.
    pub fn ident(&mut self) -> Option<&'a str> {
        self.skip_ws();
        let rest = self.rest();
        let len = rest
            .char_indices()
            .take_while(|(i, c)| c.is_alphanumeric() || *c == '_' || *c == '$' || (*i > 0 && c.is_ascii_digit()))
            .last()
            .map_or(0, |(i, c)| i + c.len_utf8());
        if len == 0 || rest.starts_with(|c: char| c.is_ascii_digit()) {
            return None;
        }
        self.pos += len;
        Some(&rest[..len])
    }

    /// The next identifier without consuming it.
    pub fn peek_ident(&mut self) -> Option<&'a str> {
        let start = self.pos;
        let id = self.ident();
        self.pos = start;
        id
    }

    pub fn string(&mut self) -> DbResult<String> {
        self.skip_ws();
        let start = self.pos;
        let quote =
            self.bump().filter(|c| matches!(c, '\'' | '"' | '`')).ok_or_else(|| self.error("Expected a string."))?;
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(self.error_at(start, "This string is not closed.")),
                Some(c) if c == quote => return Ok(out),
                Some('\\') => match self.bump() {
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some('t') => out.push('\t'),
                    Some('b') => out.push('\u{8}'),
                    Some('f') => out.push('\u{c}'),
                    Some('v') => out.push('\u{b}'),
                    Some('0') => out.push('\0'),
                    Some('\n') => {}
                    Some('u') => out.push(self.unicode_escape()?),
                    Some('x') => {
                        let hex = self.take_hex(2)?;
                        out.push(char::from_u32(hex).ok_or_else(|| self.error("Invalid escape."))?);
                    }
                    Some(c) => out.push(c),
                    None => return Err(self.error_at(start, "This string is not closed.")),
                },
                Some(c) => out.push(c),
            }
        }
    }

    fn take_hex(&mut self, n: usize) -> DbResult<u32> {
        let digits = self.rest().get(..n).filter(|d| d.chars().all(|c| c.is_ascii_hexdigit()));
        let digits = digits.ok_or_else(|| self.error("Invalid escape."))?;
        self.pos += n;
        u32::from_str_radix(digits, 16).map_err(|_| self.error("Invalid escape."))
    }

    fn unicode_escape(&mut self) -> DbResult<char> {
        if self.peek() == Some('{') {
            self.pos += 1;
            let len = self.rest().find('}').ok_or_else(|| self.error("Invalid escape."))?;
            let code = u32::from_str_radix(&self.rest()[..len], 16).map_err(|_| self.error("Invalid escape."))?;
            self.pos += len + 1;
            return char::from_u32(code).ok_or_else(|| self.error("Invalid escape."));
        }
        let high = self.take_hex(4)?;
        if (0xD800..0xDC00).contains(&high) && self.rest().starts_with("\\u") {
            self.pos += 2;
            let low = self.take_hex(4)?;
            let code = 0x10000 + ((high - 0xD800) << 10) + (low.wrapping_sub(0xDC00) & 0x3FF);
            return char::from_u32(code).ok_or_else(|| self.error("Invalid escape."));
        }
        char::from_u32(high).ok_or_else(|| self.error("Invalid escape."))
    }

    /// Parses one value.
    pub fn value(&mut self) -> DbResult<Bson> {
        self.value_at(0)
    }

    fn value_at(&mut self, depth: usize) -> DbResult<Bson> {
        if depth > MAX_DEPTH {
            return Err(self.error("The value is nested too deeply."));
        }
        self.skip_ws();
        let start = self.pos;
        match self.peek() {
            None => Err(self.error("Expected a value.")),
            Some('{') => self.object(depth).and_then(|d| self.ejson(d, start)),
            Some('[') => {
                self.pos += 1;
                let mut items = Vec::new();
                loop {
                    if self.eat(']') {
                        return Ok(Bson::Array(items));
                    }
                    items.push(self.value_at(depth + 1)?);
                    if !self.eat(',') {
                        self.expect(']')?;
                        return Ok(Bson::Array(items));
                    }
                }
            }
            Some('\'' | '"' | '`') => self.string().map(Bson::String),
            Some('/') => self.regex(),
            Some(c) if c.is_ascii_digit() || c == '-' || c == '+' || c == '.' => self.number(),
            Some(_) => self.word(depth),
        }
    }

    fn object(&mut self, depth: usize) -> DbResult<Document> {
        self.expect('{')?;
        let mut doc = Document::new();
        loop {
            if self.eat('}') {
                return Ok(doc);
            }
            self.skip_ws();
            let key = match self.peek() {
                Some('\'' | '"' | '`') => self.string()?,
                Some(c) if c.is_ascii_digit() => {
                    let len = self.rest().find(|c: char| !c.is_ascii_digit()).unwrap_or(self.rest().len());
                    let key = self.rest()[..len].to_owned();
                    self.pos += len;
                    key
                }
                _ => self.ident().ok_or_else(|| self.error("Expected a field name."))?.to_owned(),
            };
            self.expect(':')?;
            let value = self.value_at(depth + 1)?;
            doc.insert(key, value);
            if !self.eat(',') {
                self.expect('}')?;
                return Ok(doc);
            }
        }
    }

    /// `{ $oid: … }`, `{ $date: … }` and the other Extended JSON wrappers.
    fn ejson(&self, doc: Document, start: usize) -> DbResult<Bson> {
        const WRAPPERS: &[&str] = &[
            "$oid",
            "$date",
            "$numberLong",
            "$numberInt",
            "$numberDouble",
            "$numberDecimal",
            "$binary",
            "$uuid",
            "$regularExpression",
            "$timestamp",
            "$minKey",
            "$maxKey",
            "$undefined",
            "$symbol",
            "$code",
            "$dbPointer",
        ];
        let wrapper = doc.len() <= 2 && doc.keys().next().is_some_and(|k| WRAPPERS.contains(&k.as_str()));
        if !wrapper {
            return Ok(Bson::Document(doc));
        }
        let json = Bson::Document(doc).into_canonical_extjson();
        Bson::try_from(json).map_err(|e| self.error_at(start, format!("Invalid Extended JSON value: {e}")))
    }

    fn number(&mut self) -> DbResult<Bson> {
        let start = self.pos;
        let rest = self.rest();
        let sign = usize::from(rest.starts_with(['-', '+']));
        let body = &rest[sign..];
        if body.starts_with("Infinity") {
            self.pos += sign + "Infinity".len();
            return Ok(Bson::Double(if rest.starts_with('-') { f64::NEG_INFINITY } else { f64::INFINITY }));
        }
        if body.starts_with("0x") || body.starts_with("0X") {
            let len = body[2..].find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(body.len() - 2);
            let value = i64::from_str_radix(&body[2..2 + len], 16).map_err(|_| self.error("Invalid number."))?;
            self.pos += sign + 2 + len;
            return Ok(int(if rest.starts_with('-') { -value } else { value }));
        }
        let len = body
            .char_indices()
            .take_while(|(i, c)| {
                c.is_ascii_digit()
                    || *c == '.'
                    || *c == 'e'
                    || *c == 'E'
                    || ((*c == '-' || *c == '+') && *i > 0 && matches!(body.as_bytes()[i - 1], b'e' | b'E'))
            })
            .count();
        let text = &rest[..sign + len];
        self.pos += sign + len;
        if len == 0 {
            return Err(self.error_at(start, "Expected a value."));
        }
        if body[..len].contains(['.', 'e', 'E']) {
            return text.parse::<f64>().map(Bson::Double).map_err(|_| self.error_at(start, "Invalid number."));
        }
        match text.trim_start_matches('+').parse::<i64>() {
            Ok(v) => Ok(int(v)),
            // Too large for Int64: keep the value as a double, like the shell does.
            Err(_) => text.parse::<f64>().map(Bson::Double).map_err(|_| self.error_at(start, "Invalid number.")),
        }
    }

    fn regex(&mut self) -> DbResult<Bson> {
        let start = self.pos;
        self.pos += 1;
        let mut pattern = String::new();
        let mut class = false;
        loop {
            match self.bump() {
                None | Some('\n') => return Err(self.error_at(start, "This regular expression is not closed.")),
                // `\/` is only needed to write `/` between the slashes.
                Some('\\') => match self.bump() {
                    Some('/') => pattern.push('/'),
                    Some(c) => {
                        pattern.push('\\');
                        pattern.push(c);
                    }
                    None => pattern.push('\\'),
                },
                Some('[') => {
                    class = true;
                    pattern.push('[');
                }
                Some(']') => {
                    class = false;
                    pattern.push(']');
                }
                Some('/') if !class => break,
                Some(c) => pattern.push(c),
            }
        }
        let flags_len = self.rest().find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(self.rest().len());
        let mut flags: Vec<char> = self.rest()[..flags_len].chars().collect();
        self.pos += flags_len;
        if let Some(bad) = flags.iter().find(|c| !"imsxlu".contains(**c)) {
            return Err(self.error_at(start, format!("Unknown regular expression flag `{bad}`.")));
        }
        flags.sort_unstable();
        Ok(Bson::RegularExpression(Regex { pattern, options: flags.into_iter().collect() }))
    }

    /// Keywords and constructor calls.
    fn word(&mut self, depth: usize) -> DbResult<Bson> {
        let start = self.pos;
        let mut name = self.ident().ok_or_else(|| self.error("Expected a value."))?.to_owned();
        match name.as_str() {
            "true" => return Ok(Bson::Boolean(true)),
            "false" => return Ok(Bson::Boolean(false)),
            "null" => return Ok(Bson::Null),
            "undefined" => return Ok(Bson::Undefined),
            "NaN" => return Ok(Bson::Double(f64::NAN)),
            "Infinity" => return Ok(Bson::Double(f64::INFINITY)),
            "new" => self.ident().ok_or_else(|| self.error("Expected a type after `new`."))?.clone_into(&mut name),
            _ => {}
        }
        // `Binary.createFromBase64(…)`, `Binary.createFromHexString(…)`.
        if self.peek() == Some('.') {
            self.pos += 1;
            let member = self.ident().ok_or_else(|| self.error("Expected a name."))?;
            name = format!("{name}.{member}");
        }
        if !self.eat('(') {
            return Err(self.error_at(start, format!("`{name}` is not a value. Quote strings, e.g. '{name}'.")));
        }
        let mut args = Vec::new();
        loop {
            if self.eat(')') {
                break;
            }
            args.push(self.value_at(depth + 1)?);
            if !self.eat(',') {
                self.expect(')')?;
                break;
            }
        }
        construct(&name, args).map_err(|m| self.error_at(start, m))
    }
}

fn int(v: i64) -> Bson {
    i32::try_from(v).map_or(Bson::Int64(v), Bson::Int32)
}

/// Text of a numeric or string argument.
fn arg_text(arg: Option<&Bson>) -> Option<String> {
    match arg? {
        Bson::String(s) => Some(s.clone()),
        Bson::Int32(v) => Some(v.to_string()),
        Bson::Int64(v) => Some(v.to_string()),
        Bson::Double(v) => Some(format_double(*v)),
        Bson::Decimal128(v) => Some(v.to_string()),
        _ => None,
    }
}

/// Builds a value from a shell constructor such as `ObjectId('…')`.
fn construct(name: &str, args: Vec<Bson>) -> Result<Bson, String> {
    let text = || arg_text(args.first()).ok_or_else(|| format!("{name}() needs a value."));
    Ok(match name {
        "ObjectId" | "ObjectID" => match args.first() {
            None => Bson::ObjectId(ObjectId::new()),
            Some(_) => Bson::ObjectId(ObjectId::parse_str(text()?).map_err(|_| "ObjectId needs 24 hex digits.")?),
        },
        "ISODate" | "Date" => match args.first() {
            None => Bson::DateTime(DateTime::now()),
            Some(Bson::String(s)) => Bson::DateTime(parse_date(s).ok_or("The date is not ISO 8601.")?),
            Some(v) => Bson::DateTime(DateTime::from_millis(
                arg_text(Some(v)).and_then(|t| t.parse::<f64>().ok()).ok_or("The date is not valid.")? as i64,
            )),
        },
        "NumberLong" | "Long" => Bson::Int64(text()?.trim().parse().map_err(|_| "Long needs a whole number.")?),
        "NumberInt" | "Int32" => Bson::Int32(text()?.trim().parse().map_err(|_| "Int32 needs a 32-bit whole number.")?),
        "NumberDecimal" | "Decimal128" => {
            Bson::Decimal128(text()?.trim().parse::<Decimal128>().map_err(|_| "Decimal128 needs a decimal number.")?)
        }
        "Double" | "Number" => Bson::Double(text()?.trim().parse().map_err(|_| "Double needs a number.")?),
        "UUID" => {
            let uuid = match args.first() {
                None => uuid::Uuid::new_v4(),
                Some(_) => uuid::Uuid::parse_str(&text()?).map_err(|_| "UUID needs a valid UUID.")?,
            };
            Bson::Binary(Binary { subtype: BinarySubtype::Uuid, bytes: uuid.as_bytes().to_vec() })
        }
        "BinData" => {
            let subtype =
                arg_text(args.first()).and_then(|s| s.parse::<u8>().ok()).ok_or("BinData(subtype, base64).")?;
            let data = arg_text(args.get(1)).ok_or("BinData(subtype, base64).")?;
            binary(subtype, &data)?
        }
        "Binary.createFromBase64" => {
            let data = text()?;
            let subtype = arg_text(args.get(1)).and_then(|s| s.parse::<u8>().ok()).unwrap_or(0);
            binary(subtype, &data)?
        }
        "Binary.createFromHexString" => {
            let hex = text()?;
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|i| hex.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
                .collect::<Option<Vec<u8>>>()
                .ok_or("The hex string is not valid.")?;
            let subtype = arg_text(args.get(1)).and_then(|s| s.parse::<u8>().ok()).unwrap_or(0);
            Bson::Binary(Binary { subtype: BinarySubtype::from(subtype), bytes })
        }
        "Timestamp" => match args.as_slice() {
            [Bson::Document(d)] => Bson::Timestamp(Timestamp {
                time: u32_field(d, "t").ok_or("Timestamp({ t, i }) needs whole numbers.")?,
                increment: u32_field(d, "i").ok_or("Timestamp({ t, i }) needs whole numbers.")?,
            }),
            [t, i] => Bson::Timestamp(Timestamp {
                time: arg_text(Some(t)).and_then(|v| v.parse().ok()).ok_or("Timestamp(t, i) needs whole numbers.")?,
                increment: arg_text(Some(i))
                    .and_then(|v| v.parse().ok())
                    .ok_or("Timestamp(t, i) needs whole numbers.")?,
            }),
            _ => return Err("Timestamp needs { t, i }.".into()),
        },
        "MinKey" => Bson::MinKey,
        "MaxKey" => Bson::MaxKey,
        "Code" => match args.as_slice() {
            [Bson::String(code)] => Bson::JavaScriptCode(code.clone()),
            [Bson::String(code), Bson::Document(scope)] => {
                Bson::JavaScriptCodeWithScope(JavaScriptCodeWithScope { code: code.clone(), scope: scope.clone() })
            }
            _ => return Err("Code needs a string.".into()),
        },
        "Symbol" => Bson::Symbol(text()?),
        "RegExp" => {
            let pattern = text()?;
            let mut flags: Vec<char> = arg_text(args.get(1)).unwrap_or_default().chars().collect();
            flags.sort_unstable();
            Bson::RegularExpression(Regex { pattern, options: flags.into_iter().collect() })
        }
        other => return Err(format!("`{other}()` is not supported. Only literal values can be used.")),
    })
}

fn u32_field(d: &Document, key: &str) -> Option<u32> {
    arg_text(d.get(key)).and_then(|v| v.parse().ok())
}

fn binary(subtype: u8, base64: &str) -> Result<Bson, String> {
    let bytes =
        base64::engine::general_purpose::STANDARD.decode(base64.trim()).map_err(|_| "The base64 data is not valid.")?;
    Ok(Bson::Binary(Binary { subtype: BinarySubtype::from(subtype), bytes }))
}

/// Parses ISO 8601 dates: `2026-10-01`, `2026-10-01T10:00:00Z`, with
/// fractions and offsets. A date without an offset is UTC, as in the shell.
pub fn parse_date(s: &str) -> Option<DateTime> {
    let s = s.trim();
    if s.len() == 10 {
        return DateTime::parse_rfc3339_str(format!("{s}T00:00:00Z")).ok();
    }
    let has_offset = s.ends_with(['Z', 'z']) || s.get(19..).is_some_and(|tail| tail.contains(['+', '-']));
    let s = s.replacen(' ', "T", 1);
    if has_offset { DateTime::parse_rfc3339_str(&s).ok() } else { DateTime::parse_rfc3339_str(format!("{s}Z")).ok() }
}

/// Parses a single value; the whole text must be used.
pub fn parse(text: &str) -> DbResult<Bson> {
    let mut s = Scanner::new(text);
    let value = s.value()?;
    if !s.at_end() {
        return Err(s.error("Unexpected text after the value."));
    }
    Ok(value)
}

/// Parses a document; empty text is an empty document.
pub fn parse_document(text: &str, what: &str) -> DbResult<Document> {
    if text.trim().is_empty() {
        return Ok(Document::new());
    }
    match parse(text)? {
        Bson::Document(d) => Ok(d),
        _ => {
            Err(DbError::new(ErrorKind::Syntax, format!("The {what} must be an object such as {{ status: 'paid' }}.")))
        }
    }
}

/// Shortest text that reads back as the same double.
pub fn format_double(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v.is_infinite() {
        if v > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else {
        format!("{v:?}")
    }
}

/// `2026-10-01T10:00:00.000Z`, or `None` outside years 0–9999.
pub fn iso_date(d: DateTime) -> Option<String> {
    let ms = d.timestamp_millis();
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    if !(0..=9999).contains(&year) {
        return None;
    }
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    ))
}

/// A string literal in single quotes.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// A field name, quoted unless it is a plain identifier.
pub fn key(k: &str) -> String {
    let mut chars = k.chars();
    let plain = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if plain { k.to_owned() } else { quote(k) }
}

/// Shell text of one value on one line, e.g. `{ _id: ObjectId('…'), n: Long('5') }`.
pub fn compact(value: &Bson) -> String {
    let mut out = String::new();
    write_value(&mut out, value, None);
    out
}

/// Shell text indented by two spaces per level.
pub fn pretty(value: &Bson) -> String {
    let mut out = String::new();
    write_value(&mut out, value, Some(0));
    out
}

pub fn pretty_document(doc: &Document) -> String {
    let mut out = String::new();
    write_document(&mut out, doc, Some(0));
    out
}

fn write_value(out: &mut String, value: &Bson, indent: Option<usize>) {
    match value {
        Bson::Document(d) => write_document(out, d, indent),
        Bson::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            let scalars = items.iter().all(|v| !matches!(v, Bson::Document(_) | Bson::Array(_)));
            match indent {
                // Short arrays of plain values stay on one line.
                Some(level) if !(scalars && items.len() <= 8) => {
                    out.push_str("[\n");
                    for (i, item) in items.iter().enumerate() {
                        push_indent(out, level + 1);
                        write_value(out, item, Some(level + 1));
                        out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
                    }
                    push_indent(out, level);
                    out.push(']');
                }
                _ => {
                    out.push_str("[ ");
                    for (i, item) in items.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        write_value(out, item, None);
                    }
                    out.push_str(" ]");
                }
            }
        }
        Bson::String(s) => out.push_str(&quote(s)),
        Bson::Double(v) => {
            let text = format_double(*v);
            out.push_str(&text);
        }
        Bson::Int32(v) => {
            let _ = write!(out, "{v}");
        }
        Bson::Int64(v) => {
            let _ = write!(out, "Long('{v}')");
        }
        Bson::Decimal128(v) => {
            let _ = write!(out, "Decimal128('{v}')");
        }
        Bson::Boolean(v) => {
            let _ = write!(out, "{v}");
        }
        Bson::Null => out.push_str("null"),
        Bson::Undefined => out.push_str("undefined"),
        Bson::ObjectId(id) => {
            let _ = write!(out, "ObjectId('{}')", id.to_hex());
        }
        Bson::DateTime(d) => match iso_date(*d) {
            Some(iso) => {
                let _ = write!(out, "ISODate('{iso}')");
            }
            None => {
                let _ = write!(out, "new Date({})", d.timestamp_millis());
            }
        },
        Bson::Binary(b) => {
            if b.subtype == BinarySubtype::Uuid
                && let Ok(uuid) = uuid::Uuid::from_slice(&b.bytes)
            {
                let _ = write!(out, "UUID('{uuid}')");
            } else {
                let data = base64::engine::general_purpose::STANDARD.encode(&b.bytes);
                let _ = write!(out, "Binary.createFromBase64('{data}', {})", u8::from(b.subtype));
            }
        }
        Bson::RegularExpression(r) => {
            let _ = write!(out, "/{}/{}", regex_body(&r.pattern), r.options);
        }
        Bson::Timestamp(t) => {
            let _ = write!(out, "Timestamp({{ t: {}, i: {} }})", t.time, t.increment);
        }
        Bson::JavaScriptCode(code) => {
            let _ = write!(out, "Code({})", quote(code));
        }
        Bson::JavaScriptCodeWithScope(c) => {
            let _ = write!(out, "Code({}, ", quote(&c.code));
            write_document(out, &c.scope, None);
            out.push(')');
        }
        Bson::Symbol(s) => {
            let _ = write!(out, "Symbol({})", quote(s));
        }
        Bson::MinKey => out.push_str("MinKey()"),
        Bson::MaxKey => out.push_str("MaxKey()"),
        // Only Extended JSON can spell a DBPointer.
        other @ Bson::DbPointer(_) => {
            let json = other.clone().into_canonical_extjson();
            match Bson::try_from(json.clone()) {
                Ok(Bson::Document(d)) => write_document(out, &d, None),
                _ => out.push_str(&json.to_string()),
            }
        }
    }
}

/// A regex pattern between slashes: a bare `/` (not escaped, not in a class) gets a backslash.
fn regex_body(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let (mut escaped, mut class) = (false, false);
    for c in pattern.chars() {
        match c {
            '/' if !escaped && !class => out.push_str("\\/"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
        if !escaped {
            match c {
                '[' => class = true,
                ']' => class = false,
                _ => {}
            }
        }
        escaped = c == '\\' && !escaped;
    }
    out
}

fn write_document(out: &mut String, doc: &Document, indent: Option<usize>) {
    if doc.is_empty() {
        out.push_str("{}");
        return;
    }
    let Some(level) = indent else {
        out.push_str("{ ");
        for (i, (k, v)) in doc.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&key(k));
            out.push_str(": ");
            write_value(out, v, None);
        }
        out.push_str(" }");
        return;
    };
    out.push_str("{\n");
    for (i, (k, v)) in doc.iter().enumerate() {
        push_indent(out, level + 1);
        out.push_str(&key(k));
        out.push_str(": ");
        write_value(out, v, Some(level + 1));
        out.push_str(if i + 1 < doc.len() { ",\n" } else { "\n" });
    }
    push_indent(out, level);
    out.push('}');
}

fn push_indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::doc;

    #[test]
    fn exact_numbers_round_trip() {
        let v = parse("{ big: Long('9007199254740993'), total: Decimal128('120.50'), n: 5, d: 5.0, x: 3000000000 }")
            .unwrap();
        let d = v.as_document().unwrap();
        assert_eq!(d.get("big"), Some(&Bson::Int64(9_007_199_254_740_993)));
        assert_eq!(d.get("total").unwrap().to_string(), Bson::Decimal128("120.50".parse().unwrap()).to_string());
        assert_eq!(d.get("n"), Some(&Bson::Int32(5)));
        assert_eq!(d.get("d"), Some(&Bson::Double(5.0)));
        assert_eq!(d.get("x"), Some(&Bson::Int64(3_000_000_000)));
        let printed = pretty(&v);
        assert!(printed.contains("Long('9007199254740993')"));
        assert!(printed.contains("Decimal128('120.50')"));
        assert!(printed.contains("d: 5.0"));
        assert_eq!(parse(&printed).unwrap(), v);
    }

    #[test]
    fn shell_constructors_and_ejson() {
        let v = parse(
            "{ _id: ObjectId(\"66f000000000000000000001\"), at: ISODate('2026-10-01T10:00:00Z'), \
             day: new Date('2026-10-02'), u: UUID('0e2f3b0a-6b2a-4c9b-9f1e-7c1d2a3b4c5d'), \
             b: BinData(0, 'AQI='), t: Timestamp({ t: 1, i: 2 }), k: MinKey(), \
             e: { $oid: '66f000000000000000000002' }, n: { $numberLong: '42' }, \
             r: /a\\/b[/]c/i, s: `tick`, 'quoted key': -1.5e3, }",
        )
        .unwrap();
        let d = v.as_document().unwrap();
        assert!(matches!(d.get("_id"), Some(Bson::ObjectId(_))));
        assert_eq!(iso_date(*d.get_datetime("at").unwrap()).unwrap(), "2026-10-01T10:00:00.000Z");
        assert_eq!(iso_date(*d.get_datetime("day").unwrap()).unwrap(), "2026-10-02T00:00:00.000Z");
        assert!(matches!(d.get("e"), Some(Bson::ObjectId(_))));
        assert_eq!(d.get("n"), Some(&Bson::Int64(42)));
        assert_eq!(d.get("quoted key"), Some(&Bson::Double(-1500.0)));
        match d.get("r") {
            Some(Bson::RegularExpression(r)) => assert_eq!((r.pattern.as_str(), r.options.as_str()), ("a/b[/]c", "i")),
            other => panic!("{other:?}"),
        }
        assert_eq!(parse(&pretty(&v)).unwrap(), v);
        assert_eq!(parse(&compact(&v)).unwrap(), v);
    }

    #[test]
    fn comments_nesting_and_strings() {
        let v = parse("/* c */ { a: [1, 'x\\'y', \"\\u00e9\\n\"], // line\n b: { c: null } }").unwrap();
        assert_eq!(v, Bson::Document(doc! { "a": [1, "x'y", "é\n"], "b": { "c": Bson::Null } }));
    }

    #[test]
    fn errors_have_positions() {
        let e = parse("{ a: 1, b: foo }").unwrap_err();
        assert_eq!(e.position, Some(12));
        assert!(parse("{ a: 'open }").is_err());
        assert!(parse("{ a: eval('x') }").unwrap_err().message.contains("not supported"));
        assert!(parse(&"[".repeat(200)).unwrap_err().message.contains("nested"));
        assert!(parse("{ a: 1 } extra").is_err());
    }

    #[test]
    fn dates_outside_iso_range_print_as_millis() {
        let far = Bson::DateTime(DateTime::from_millis(-80_000_000_000_000));
        assert!(compact(&far).starts_with("new Date(-"));
        assert_eq!(parse(&compact(&far)).unwrap(), far);
        assert_eq!(iso_date(DateTime::from_millis(-1)).unwrap(), "1969-12-31T23:59:59.999Z");
    }
}
