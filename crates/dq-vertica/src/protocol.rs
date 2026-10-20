//! Vertica's client protocol: the PostgreSQL v3 message framing, with
//! Vertica's startup parameters, password hashes, `Describe` replies (type
//! pools, 64-bit table ids, `CommandDescription`) and typed `Bind` parameters.
//! The messages follow the official drivers (vertica-python, vertica-sql-go).
//! Pure encoding and decoding; the socket lives in [`crate::conn`].

use md5::{Digest as _, Md5};
use sha2::Sha512;

use dq_core::value::hex;

/// The fixed version in the startup packet; the version wanted travels as the
/// `protocol_version` parameter and the server answers with the one in effect.
const STARTUP_VERSION: u32 = 0x0003_0005;
/// Protocol 3.16, as current drivers request.
pub const PROTOCOL_VERSION: u32 = 0x0003_0010;
/// Row descriptions carry parent columns of complex types from 3.12 on, when asked for.
pub const COMPLEX_TYPES_VERSION: u32 = 0x0003_000c;
const SSL_REQUEST: u32 = 80_877_103;
const CANCEL_REQUEST: u32 = 80_877_102;

/// Largest backend message accepted (a row with long `LONG VARCHAR` values fits).
pub const MAX_MESSAGE: usize = 512 * 1024 * 1024;

/// Authentication request codes.
pub mod auth {
    pub const OK: u32 = 0;
    pub const CLEARTEXT: u32 = 3;
    pub const MD5: u32 = 5;
    pub const CHANGE_PASSWORD: u32 = 9;
    pub const PASSWORD_GRACE: u32 = 11;
    pub const HASH: u32 = 65_536;
    pub const HASH_MD5: u32 = 65_536 + 5;
    pub const HASH_SHA512: u32 = 65_536 + 512;
}

/// Type ids of Vertica's built-in types.
pub mod oid {
    pub const UNKNOWN: u32 = 4;
    pub const BOOL: u32 = 5;
    pub const INT8: u32 = 6;
    pub const FLOAT8: u32 = 7;
    pub const CHAR: u32 = 8;
    pub const VARCHAR: u32 = 9;
    pub const DATE: u32 = 10;
    pub const TIME: u32 = 11;
    pub const TIMESTAMP: u32 = 12;
    pub const TIMESTAMPTZ: u32 = 13;
    pub const INTERVAL: u32 = 14;
    pub const TIMETZ: u32 = 15;
    pub const NUMERIC: u32 = 16;
    pub const VARBINARY: u32 = 17;
    pub const UUID: u32 = 20;
    pub const INTERVALYM: u32 = 114;
    pub const LONGVARCHAR: u32 = 115;
    pub const LONGVARBINARY: u32 = 116;
    pub const BINARY: u32 = 117;
    pub const ROW: u32 = 300;
    pub const ARRAY: u32 = 301;
    pub const MAP: u32 = 302;
    /// One-dimensional arrays (`ARRAY[INT]` is 1506) and sets (`SET[INT]` is 2706).
    pub const ARRAY1D: std::ops::Range<u32> = 1500..1600;
    pub const SET: std::ops::Range<u32> = 2700..2800;

    pub fn is_binary(oid: u32) -> bool {
        matches!(oid, BINARY | VARBINARY | LONGVARBINARY)
    }
}

// ---------------------------------------------------------------------------
// Frontend messages
// ---------------------------------------------------------------------------

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

/// A tagged message: tag, length (including itself), body.
fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(tag);
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// An untagged startup-phase packet: length, then body.
fn packet(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(body);
    out
}

pub fn ssl_request() -> Vec<u8> {
    packet(&SSL_REQUEST.to_be_bytes())
}

/// Sent on a new connection: asks the server to stop the statement of the
/// session identified by its backend key.
pub fn cancel_request(pid: u32, key: u32) -> Vec<u8> {
    let mut body = CANCEL_REQUEST.to_be_bytes().to_vec();
    body.extend_from_slice(&pid.to_be_bytes());
    body.extend_from_slice(&key.to_be_bytes());
    packet(&body)
}

/// The startup packet. `protocol_version` is a binary integer; every other
/// parameter is text.
pub fn startup(params: &[(&str, &str)]) -> Vec<u8> {
    let mut body = STARTUP_VERSION.to_be_bytes().to_vec();
    body.extend_from_slice(b"protocol_version\0");
    body.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    body.push(0);
    for (key, value) in params {
        put_str(&mut body, key);
        put_str(&mut body, value);
    }
    body.push(0);
    packet(&body)
}

pub fn password(encoded: &str) -> Vec<u8> {
    let mut body = Vec::with_capacity(encoded.len() + 1);
    put_str(&mut body, encoded);
    frame(b'p', &body)
}

pub fn query(sql: &str) -> Vec<u8> {
    let mut body = Vec::with_capacity(sql.len() + 1);
    put_str(&mut body, sql);
    frame(b'Q', &body)
}

/// The one prepared statement a connection uses; each `Parse` replaces it.
/// Vertica reads `?` markers only in named statements.
const STATEMENT: &str = "s0";

/// Prepares [`STATEMENT`]; the server infers the parameter types.
pub fn parse(sql: &str) -> Vec<u8> {
    let mut body = Vec::with_capacity(sql.len() + 4);
    put_str(&mut body, STATEMENT);
    put_str(&mut body, sql);
    body.extend_from_slice(&0u16.to_be_bytes());
    frame(b'P', &body)
}

/// Describes [`STATEMENT`]: parameters, columns and command.
pub fn describe() -> Vec<u8> {
    let mut body = vec![b'S'];
    put_str(&mut body, STATEMENT);
    frame(b'D', &body)
}

/// Binds text values to [`STATEMENT`] (unnamed portal). Unlike PostgreSQL, Vertica
/// wants each parameter's type id, then the values; results come as text.
pub fn bind(values: &[Option<Vec<u8>>], oids: &[u32]) -> Vec<u8> {
    let mut body = Vec::with_capacity(16 + values.iter().flatten().map(|v| v.len() + 8).sum::<usize>());
    put_str(&mut body, "");
    put_str(&mut body, STATEMENT);
    body.extend_from_slice(&0u16.to_be_bytes());
    body.extend_from_slice(&(oids.len() as u16).to_be_bytes());
    for oid in oids {
        body.extend_from_slice(&oid.to_be_bytes());
    }
    for value in values {
        match value {
            None => body.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(v) => {
                body.extend_from_slice(&(v.len() as u32).to_be_bytes());
                body.extend_from_slice(v);
            }
        }
    }
    body.extend_from_slice(&0u16.to_be_bytes());
    frame(b'B', &body)
}

/// Runs the unnamed portal. Vertica ignores the row limit and sends every row.
pub fn execute() -> Vec<u8> {
    frame(b'E', b"\0\0\0\0\0")
}

pub fn sync() -> Vec<u8> {
    frame(b'S', &[])
}

pub fn flush() -> Vec<u8> {
    frame(b'H', &[])
}

pub fn terminate() -> Vec<u8> {
    frame(b'X', &[])
}

/// Refuses a `COPY … FROM STDIN` the server asked data for.
pub fn copy_fail(reason: &str) -> Vec<u8> {
    let mut body = Vec::new();
    put_str(&mut body, reason);
    frame(b'f', &body)
}

/// A text parameter as Vertica's `Bind` expects it for a parameter of type
/// `oid`: booleans as `1`/`0`; binary values (`\x` hex, as the grid shows
/// them, or raw text) with backslashes escaped as `\134`.
pub fn bind_value(value: &str, oid: u32) -> Vec<u8> {
    if oid == oid::BOOL {
        let truthy = matches!(value.trim().to_ascii_lowercase().as_str(), "t" | "true" | "y" | "yes" | "1" | "on");
        return vec![if truthy { b'1' } else { b'0' }];
    }
    if !oid::is_binary(oid) {
        return value.as_bytes().to_vec();
    }
    let raw = value
        .strip_prefix("\\x")
        .filter(|h| h.len() % 2 == 0)
        .and_then(|h| (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).ok()).collect())
        .unwrap_or_else(|| value.as_bytes().to_vec());
    let mut out = Vec::with_capacity(raw.len());
    for b in raw {
        if b == b'\\' {
            out.extend_from_slice(b"\\134");
        } else {
            out.push(b);
        }
    }
    out
}

/// The answer to an authentication request, or `None` for methods this
/// client does not offer (Kerberos, OAuth, TOTP).
pub fn password_response(request: &AuthRequest, user: &str, password: &str) -> Option<String> {
    let chained = |user_or_salt: &[u8], sha: bool| {
        let mut current = password.as_bytes().to_vec();
        for key in [user_or_salt, &request.salt] {
            current.extend_from_slice(key);
            current = if sha { hex(&Sha512::digest(&current)) } else { hex(&Md5::digest(&current)) }.into_bytes();
        }
        String::from_utf8(current).unwrap_or_default()
    };
    match request.code {
        auth::CLEARTEXT => Some(password.to_owned()),
        auth::MD5 | auth::HASH_MD5 => Some(format!("md5{}", chained(user.as_bytes(), false))),
        auth::HASH | auth::HASH_SHA512 => Some(format!("sha512{}", chained(&request.user_salt, true))),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Backend messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthRequest {
    pub code: u32,
    pub salt: Vec<u8>,
    /// The user's own salt, for `HASH` (SHA-512) requests.
    pub user_salt: Vec<u8>,
}

/// A column of a row description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub table_oid: u64,
    pub type_oid: u32,
    /// The user-defined type's name (`geometry`, `geography`) from the type pool.
    pub type_name: Option<String>,
    pub type_modifier: i32,
    pub nullable: bool,
}

/// A parameter of a described statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Param {
    pub type_oid: u32,
}

/// `ErrorResponse` / `NoticeResponse` fields by their one-byte codes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fields(pub Vec<(u8, String)>);

impl Fields {
    pub fn get(&self, code: u8) -> Option<&str> {
        self.0.iter().find(|(c, _)| *c == code).map(|(_, v)| v.as_str())
    }

    pub fn severity(&self) -> &str {
        self.get(b'S').unwrap_or("ERROR")
    }

    pub fn sqlstate(&self) -> &str {
        self.get(b'C').unwrap_or("")
    }

    pub fn message(&self) -> &str {
        self.get(b'M').unwrap_or("")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    Authentication(AuthRequest),
    ParameterStatus(String, String),
    KeyData {
        pid: u32,
        key: u32,
    },
    ReadyForQuery(u8),
    RowDescription(Vec<Column>),
    DataRow(Vec<Option<Vec<u8>>>),
    CommandComplete(String),
    EmptyQuery,
    Error(Fields),
    Notice(Fields),
    ParseComplete,
    BindComplete,
    CloseComplete,
    NoData,
    PortalSuspended,
    ParameterDescription(Vec<Param>),
    CommandDescription(String),
    /// `COPY … FROM STDIN` waits for data.
    CopyIn,
    /// Messages of `COPY … LOCAL` and load balancing, which this client does not take part in.
    Other(u8),
}

/// Reads fields of a message body.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

type ParseResult<T> = Result<T, String>;

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> ParseResult<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|e| *e <= self.buf.len()).ok_or("message ended early")?;
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn u8(&mut self) -> ParseResult<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> ParseResult<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().map_err(|_| "bad u16")?))
    }

    fn u32(&mut self) -> ParseResult<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().map_err(|_| "bad u32")?))
    }

    fn i32(&mut self) -> ParseResult<i32> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().map_err(|_| "bad i32")?))
    }

    fn u64(&mut self) -> ParseResult<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().map_err(|_| "bad u64")?))
    }

    fn cstr(&mut self) -> ParseResult<String> {
        let rest = &self.buf[self.pos..];
        let len = rest.iter().position(|b| *b == 0).ok_or("string without terminator")?;
        let s = String::from_utf8_lossy(&rest[..len]).into_owned();
        self.pos += len + 1;
        Ok(s)
    }

    fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }
}

/// The type pool of a description: base type id and name of each user type.
fn type_pool(r: &mut Reader<'_>) -> ParseResult<Vec<(u32, String)>> {
    let count = r.u32()? as usize;
    let mut pool = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        pool.push((r.u32()?, r.cstr()?));
    }
    Ok(pool)
}

/// A primitive type id, or a user type looked up in the pool.
fn resolve_type(pool: &[(u32, String)], user_type: bool, id: u32) -> ParseResult<(u32, Option<String>)> {
    if !user_type {
        return Ok((id, None));
    }
    let (base, name) = pool.get(id as usize).ok_or("type pool index out of range")?;
    Ok((*base, Some(name.clone())))
}

fn row_description(body: &[u8], complex_types: bool) -> ParseResult<Vec<Column>> {
    let mut r = Reader::new(body);
    let count = r.u16()? as usize;
    if count == 0 {
        return Ok(Vec::new());
    }
    let pool = type_pool(&mut r)?;
    let mut columns = Vec::with_capacity(count);
    for _ in 0..count {
        let name = r.cstr()?;
        let table_oid = r.u64()?;
        if table_oid != 0 {
            r.cstr()?; // schema
            r.cstr()?; // table
        }
        r.u16()?; // attribute number
        let parent = if complex_types { r.u16()? } else { 0 };
        let user_type = r.u8()? == 1;
        let id = r.u32()?;
        let _size = r.u16()?;
        let nullable = r.u16()? == 1;
        let _identity = r.u16()?;
        let type_modifier = r.i32()?;
        let _format = r.u16()?;
        // Fields of a complex type's elements describe the parent column only.
        if parent != 0 {
            continue;
        }
        let (type_oid, type_name) = resolve_type(&pool, user_type, id)?;
        columns.push(Column { name, table_oid, type_oid, type_name, type_modifier, nullable });
    }
    Ok(columns)
}

fn parameter_description(body: &[u8]) -> ParseResult<Vec<Param>> {
    let mut r = Reader::new(body);
    let count = r.u16()? as usize;
    if count == 0 {
        return Ok(Vec::new());
    }
    let pool = type_pool(&mut r)?;
    let mut params = Vec::with_capacity(count);
    for _ in 0..count {
        let user_type = r.u8()? == 1;
        let id = r.u32()?;
        let _modifier = r.i32()?;
        let _not_null = r.u16()?;
        params.push(Param { type_oid: resolve_type(&pool, user_type, id)?.0 });
    }
    Ok(params)
}

fn data_row(body: &[u8]) -> ParseResult<Vec<Option<Vec<u8>>>> {
    let mut r = Reader::new(body);
    let count = r.u16()? as usize;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        let len = r.i32()?;
        values.push(if len < 0 { None } else { Some(r.take(len as usize)?.to_vec()) });
    }
    Ok(values)
}

fn fields(body: &[u8]) -> ParseResult<Fields> {
    let mut r = Reader::new(body);
    let mut out = Vec::new();
    loop {
        let code = r.u8()?;
        if code == 0 {
            return Ok(Fields(out));
        }
        out.push((code, r.cstr()?));
    }
}

fn authentication(body: &[u8]) -> ParseResult<AuthRequest> {
    let mut r = Reader::new(body);
    let code = r.u32()?;
    let mut request = AuthRequest { code, salt: Vec::new(), user_salt: Vec::new() };
    match code {
        auth::MD5 | auth::HASH_MD5 => request.salt = r.take(4)?.to_vec(),
        auth::HASH | auth::HASH_SHA512 => {
            request.salt = r.take(4)?.to_vec();
            let len = r.u32()? as usize;
            if len != 16 {
                return Err(format!("unexpected user salt size {len}"));
            }
            request.user_salt = r.take(len)?.to_vec();
        }
        _ => {}
    }
    Ok(request)
}

/// Decodes one backend message. `complex_types` says whether row
/// descriptions carry parent column numbers (see [`COMPLEX_TYPES_VERSION`]).
pub fn decode(tag: u8, body: &[u8], complex_types: bool) -> ParseResult<Backend> {
    let tag_body = |r: &mut Reader<'_>| r.cstr();
    Ok(match tag {
        b'R' => Backend::Authentication(authentication(body)?),
        b'S' => {
            let mut r = Reader::new(body);
            Backend::ParameterStatus(r.cstr()?, r.cstr()?)
        }
        b'K' => {
            let mut r = Reader::new(body);
            Backend::KeyData { pid: r.u32()?, key: r.u32()? }
        }
        b'Z' => Backend::ReadyForQuery(Reader::new(body).u8()?),
        b'T' => Backend::RowDescription(row_description(body, complex_types)?),
        b'D' => Backend::DataRow(data_row(body)?),
        b'C' => Backend::CommandComplete(tag_body(&mut Reader::new(body))?),
        b'I' => Backend::EmptyQuery,
        b'E' => Backend::Error(fields(body)?),
        b'N' => Backend::Notice(fields(body)?),
        b'1' => Backend::ParseComplete,
        b'2' => Backend::BindComplete,
        b'3' => Backend::CloseComplete,
        b'n' => Backend::NoData,
        b's' => Backend::PortalSuspended,
        b't' => Backend::ParameterDescription(parameter_description(body)?),
        b'm' => {
            let mut r = Reader::new(body);
            let tag = r.cstr()?;
            let _ = r.rest();
            Backend::CommandDescription(tag)
        }
        b'G' => Backend::CopyIn,
        other => Backend::Other(other),
    })
}

/// Bytes of a binary value in Vertica's text form: `\\` is a backslash and
/// `\ooo` an octal byte; everything else stands for itself.
pub fn unescape_binary(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text[i] == b'\\' {
            if text.get(i + 1) == Some(&b'\\') {
                out.push(b'\\');
                i += 2;
                continue;
            }
            if let Some(octal) = text.get(i + 1..i + 4)
                && let Ok(byte) = u8::from_str_radix(std::str::from_utf8(octal).unwrap_or("x"), 8)
            {
                out.push(byte);
                i += 4;
                continue;
            }
        }
        out.push(text[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vectors computed with vertica-python's `Password.encoded_password()`
    /// (password `secret`, user `dbadmin`, salt `ab12`, user salt `0123456789abcdef`).
    #[test]
    fn password_hashes_match_the_reference_driver() {
        let request = |code| AuthRequest { code, salt: b"ab12".to_vec(), user_salt: b"0123456789abcdef".to_vec() };
        assert_eq!(
            password_response(&request(auth::MD5), "dbadmin", "secret").unwrap(),
            "md58c67046c5fcef4afe2966e8ac75692c5"
        );
        assert_eq!(
            password_response(&request(auth::HASH_MD5), "dbadmin", "secret"),
            password_response(&request(auth::MD5), "dbadmin", "secret")
        );
        assert_eq!(
            password_response(&request(auth::HASH_SHA512), "dbadmin", "secret").unwrap(),
            "sha512622e073e9d071ddff58bbbc0a61429c3523e804bfa827845e1602271698b84f31bb3789652dd427ffb0491abf688a32b8c7033f5a033e2c7ba16148200b7c168"
        );
        assert_eq!(password_response(&request(auth::CLEARTEXT), "dbadmin", "secret").unwrap(), "secret");
        assert_eq!(password_response(&request(7), "dbadmin", "secret"), None);
    }

    #[test]
    fn startup_packet_layout() {
        let bytes = startup(&[("user", "dbadmin"), ("database", "vdb")]);
        assert_eq!(u32::from_be_bytes(bytes[0..4].try_into().unwrap()) as usize, bytes.len());
        assert_eq!(&bytes[4..8], &STARTUP_VERSION.to_be_bytes());
        assert_eq!(&bytes[8..25], b"protocol_version\0");
        assert_eq!(&bytes[25..29], &PROTOCOL_VERSION.to_be_bytes());
        assert_eq!(&bytes[29..], b"\0user\0dbadmin\0database\0vdb\0\0");
        assert_eq!(ssl_request(), [0, 0, 0, 8, 4, 210, 22, 47]);
        assert_eq!(cancel_request(1, 2), [0, 0, 0, 16, 4, 210, 22, 46, 0, 0, 0, 1, 0, 0, 0, 2]);
    }

    #[test]
    fn bind_sends_type_ids_before_values() {
        let bytes = bind(&[Some(b"7".to_vec()), None], &[oid::INT8, oid::VARCHAR]);
        assert_eq!(bytes[0], b'B');
        assert_eq!(
            &bytes[5..],
            &[0, b's', b'0', 0, 0, 0, 0, 2, 0, 0, 0, 6, 0, 0, 0, 9, 0, 0, 0, 1, b'7', 0xff, 0xff, 0xff, 0xff, 0, 0][..]
        );
        assert_eq!(u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize, bytes.len() - 1);
        assert_eq!(bind_value("true", oid::BOOL), b"1");
        assert_eq!(bind_value("f", oid::BOOL), b"0");
        assert_eq!(bind_value("\\x5c41", oid::VARBINARY), b"\\134A");
        assert_eq!(bind_value("a\\b", oid::VARCHAR), b"a\\b");
        assert_eq!(parse("SELECT ?"), b"P\0\0\0\x12s0\0SELECT ?\0\0\0");
        assert_eq!(execute(), b"E\0\0\0\x09\0\0\0\0\0");
    }

    fn cstr(s: &str) -> Vec<u8> {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    }

    #[test]
    fn row_description_with_type_pool() {
        let mut body = 2u16.to_be_bytes().to_vec();
        body.extend(1u32.to_be_bytes());
        body.extend(oid::LONGVARBINARY.to_be_bytes());
        body.extend(cstr("geometry"));
        // A table column of a primitive type.
        body.extend(cstr("id"));
        body.extend(45_035_996_273_704_982u64.to_be_bytes());
        body.extend(cstr("public"));
        body.extend(cstr("t"));
        body.extend(1u16.to_be_bytes());
        body.extend([0]);
        body.extend(oid::INT8.to_be_bytes());
        body.extend(8u16.to_be_bytes());
        body.extend(0u16.to_be_bytes());
        body.extend(1u16.to_be_bytes());
        body.extend((-1i32).to_be_bytes());
        body.extend(0u16.to_be_bytes());
        // An expression of a user type (pool entry 0).
        body.extend(cstr("g"));
        body.extend(0u64.to_be_bytes());
        body.extend(0u16.to_be_bytes());
        body.extend([1]);
        body.extend(0u32.to_be_bytes());
        body.extend(u16::MAX.to_be_bytes());
        body.extend(1u16.to_be_bytes());
        body.extend(0u16.to_be_bytes());
        body.extend(1000i32.to_be_bytes());
        body.extend(0u16.to_be_bytes());
        let Backend::RowDescription(columns) = decode(b'T', &body, false).unwrap() else { panic!() };
        assert_eq!(columns.len(), 2);
        assert_eq!((columns[0].name.as_str(), columns[0].type_oid, columns[0].nullable), ("id", oid::INT8, false));
        assert_eq!(columns[1].type_oid, oid::LONGVARBINARY);
        assert_eq!(columns[1].type_name.as_deref(), Some("geometry"));
        assert!(decode(b'T', &body[..body.len() - 3], false).is_err());
    }

    #[test]
    fn backend_messages() {
        let mut auth = auth::HASH_SHA512.to_be_bytes().to_vec();
        auth.extend(b"salt");
        auth.extend(16u32.to_be_bytes());
        auth.extend(b"0123456789abcdef");
        let Backend::Authentication(request) = decode(b'R', &auth, false).unwrap() else { panic!() };
        assert_eq!((request.salt.as_slice(), request.user_salt.len()), (&b"salt"[..], 16));
        let mut row = 3u16.to_be_bytes().to_vec();
        row.extend(1i32.to_be_bytes());
        row.extend(b"1");
        row.extend((-1i32).to_be_bytes());
        row.extend(0i32.to_be_bytes());
        assert_eq!(decode(b'D', &row, false).unwrap(), Backend::DataRow(vec![Some(b"1".to_vec()), None, Some(vec![])]));
        let error = b"SERROR\0C42V01\0MRelation \"x\" does not exist\0P15\0\0";
        let Backend::Error(f) = decode(b'E', error, false).unwrap() else { panic!() };
        assert_eq!((f.sqlstate(), f.message(), f.get(b'P')), ("42V01", "Relation \"x\" does not exist", Some("15")));
        let mut params = 1u16.to_be_bytes().to_vec();
        params.extend(0u32.to_be_bytes());
        params.extend([0]);
        params.extend(oid::NUMERIC.to_be_bytes());
        params.extend(0i32.to_be_bytes());
        params.extend(0u16.to_be_bytes());
        assert_eq!(decode(b't', &params, false).unwrap(), Backend::ParameterDescription(vec![Param { type_oid: 16 }]));
        assert_eq!(decode(b'm', b"SELECT\0\0\0\0", false).unwrap(), Backend::CommandDescription("SELECT".into()));
        assert_eq!(decode(b'Z', b"T", false).unwrap(), Backend::ReadyForQuery(b'T'));
    }

    #[test]
    fn binary_text_form() {
        assert_eq!(unescape_binary(b"\\001a\\\\b\\377"), vec![1, b'a', b'\\', b'b', 255]);
        assert_eq!(unescape_binary(b"\\9"), b"\\9");
    }
}
