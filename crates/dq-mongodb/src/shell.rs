//! Parses one `mongosh`-style statement into its parts. Arguments are
//! literals only (see [`crate::literal`]); there is no JavaScript.

use dq_core::DbResult;
use mongodb::bson::Bson;

use crate::literal::Scanner;

/// One parsed statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    /// `show dbs`, `show collections`.
    Show(String),
    /// `use app`.
    Use(String),
    Begin,
    Commit,
    Rollback,
    Call(Call),
}

/// `db[.getSiblingDB('d')].<collection>.<method>(…).<chain>(…)`, or a
/// database method when `collection` is `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub database: Option<String>,
    pub collection: Option<String>,
    pub method: String,
    pub args: Vec<Bson>,
    /// Cursor methods after the call, e.g. `sort`, `limit`, `explain`.
    pub chain: Vec<(String, Vec<Bson>)>,
}

impl Call {
    /// Arguments of the chained `name(…)`, if present.
    pub fn chained(&self, name: &str) -> Option<&[Bson]> {
        self.chain.iter().find(|(n, _)| n == name).map(|(_, a)| a.as_slice())
    }
}

pub fn parse(text: &str) -> DbResult<Statement> {
    let text = text.trim().trim_end_matches(';').trim_end();
    let mut s = Scanner::new(text);
    let start = s.pos;
    let Some(head) = s.ident() else {
        return Err(s.error("Statements start with `db.`, `show` or `use`, e.g. db.orders.find({ status: 'paid' })."));
    };
    let statement = match head {
        "show" => {
            let what = s.ident().ok_or_else(|| s.error("Use `show dbs` or `show collections`."))?;
            match what {
                "dbs" | "databases" => Statement::Show("dbs".into()),
                "collections" | "tables" => Statement::Show("collections".into()),
                _ => return Err(s.error_at(start, "Use `show dbs` or `show collections`.")),
            }
        }
        "use" => {
            s.skip_ws();
            let rest = s.src[s.pos..].trim();
            if rest.is_empty() || rest.contains(char::is_whitespace) {
                return Err(s.error("Name one database: `use app`."));
            }
            s.pos = s.src.len();
            Statement::Use(rest.trim_matches(['\'', '"']).to_owned())
        }
        w if w.eq_ignore_ascii_case("begin") => Statement::Begin,
        w if w.eq_ignore_ascii_case("commit") => Statement::Commit,
        w if w.eq_ignore_ascii_case("rollback") => Statement::Rollback,
        "db" => Statement::Call(call(&mut s)?),
        other => {
            return Err(s.error_at(start, format!("`{other}` is not supported here. Start with `db.`, `show` or `use`.")));
        }
    };
    if !s.at_end() {
        return Err(s.error("Unexpected text. Run one statement at a time."));
    }
    Ok(statement)
}

/// Arguments up to the closing `)`; the `(` is already consumed.
fn arguments(s: &mut Scanner<'_>) -> DbResult<Vec<Bson>> {
    let mut args = Vec::new();
    loop {
        if s.eat(')') {
            return Ok(args);
        }
        args.push(s.value()?);
        if !s.eat(',') {
            s.expect(')')?;
            return Ok(args);
        }
    }
}

/// A member after `db`: `.name`, `["name"]` or `.name(args)`.
enum Member {
    Property(String),
    Method(String, Vec<Bson>),
}

fn member(s: &mut Scanner<'_>) -> DbResult<Option<Member>> {
    if s.eat('[') {
        let name = s.string()?;
        s.expect(']')?;
        return Ok(Some(Member::Property(name)));
    }
    if !s.eat('.') {
        return Ok(None);
    }
    let name = s.ident().ok_or_else(|| s.error("Expected a name after `.`."))?.to_owned();
    if s.eat('(') { Ok(Some(Member::Method(name, arguments(s)?))) } else { Ok(Some(Member::Property(name))) }
}

fn string_arg(s: &Scanner<'_>, name: &str, args: &[Bson]) -> DbResult<String> {
    match args {
        [Bson::String(v)] => Ok(v.clone()),
        _ => Err(s.error(format!("{name}() takes one name in quotes."))),
    }
}

fn call(s: &mut Scanner<'_>) -> DbResult<Call> {
    let mut database = None;
    let mut collection: Option<String> = None;
    loop {
        match member(s)? {
            None => return Err(s.error("Expected a collection and a method, e.g. db.orders.find().")),
            Some(Member::Property(name)) => {
                // `db.system.profile` names a collection with dots.
                collection = Some(match collection {
                    Some(c) => format!("{c}.{name}"),
                    None => name,
                });
            }
            Some(Member::Method(name, args)) if name == "getSiblingDB" && collection.is_none() => {
                database = Some(string_arg(s, &name, &args)?);
            }
            Some(Member::Method(name, args)) if name == "getCollection" && collection.is_none() => {
                collection = Some(string_arg(s, &name, &args)?);
            }
            Some(Member::Method(method, args)) => {
                let mut chain = Vec::new();
                loop {
                    match member(s)? {
                        None => break,
                        Some(Member::Method(name, args)) => chain.push((name, args)),
                        Some(Member::Property(name)) => {
                            return Err(s.error(format!("`.{name}` must be called, e.g. .{name}().")));
                        }
                    }
                }
                return Ok(Call { database, collection, method, args, chain });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::doc;

    fn call(text: &str) -> Call {
        match parse(text).unwrap() {
            Statement::Call(c) => c,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn collection_methods_and_chains() {
        let c = call("db.orders.find({ status: 'paid' }, { total: 1 })\n  .sort({ total: -1 }).limit(5);");
        assert_eq!((c.collection.as_deref(), c.method.as_str()), (Some("orders"), "find"));
        assert_eq!(c.args, vec![Bson::Document(doc! { "status": "paid" }), Bson::Document(doc! { "total": 1 })]);
        assert_eq!(c.chained("sort"), Some(&[Bson::Document(doc! { "total": -1 })][..]));
        assert_eq!(c.chained("limit"), Some(&[Bson::Int32(5)][..]));
    }

    #[test]
    fn named_collections_and_databases() {
        let c = call("db.getSiblingDB('shop').getCollection('a b').countDocuments()");
        assert_eq!((c.database.as_deref(), c.collection.as_deref()), (Some("shop"), Some("a b")));
        let c = call("db.system.profile.find()");
        assert_eq!(c.collection.as_deref(), Some("system.profile"));
        let c = call("db['orders-2026'].drop()");
        assert_eq!(c.collection.as_deref(), Some("orders-2026"));
        let c = call("db.runCommand({ ping: 1 })");
        assert_eq!((c.collection, c.method.as_str()), (None, "runCommand"));
    }

    #[test]
    fn keywords() {
        assert_eq!(parse("show dbs").unwrap(), Statement::Show("dbs".into()));
        assert_eq!(parse("show collections;").unwrap(), Statement::Show("collections".into()));
        assert_eq!(parse("use shop").unwrap(), Statement::Use("shop".into()));
        assert_eq!(parse("begin").unwrap(), Statement::Begin);
        assert_eq!(parse("COMMIT").unwrap(), Statement::Commit);
    }

    #[test]
    fn errors() {
        assert!(parse("db.orders").is_err());
        assert!(parse("db.orders.find().limit").is_err());
        assert!(parse("db.orders.find() db.x.find()").is_err());
        assert!(parse("print('x')").is_err());
        assert_eq!(parse("db.a.find({ x: y })").unwrap_err().position, Some(16));
    }
}
