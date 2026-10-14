//! Identifier and literal quoting per engine.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::node::ObjectRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum SqlFlavor {
    Postgres,
    Mysql,
}

/// Engine-specific SQL spelling. Every identifier that ends up in generated SQL
/// goes through [`Dialect::quote_ident`]; values only ever travel as parameters,
/// except in human-readable previews built with [`Dialect::quote_literal`].
pub trait Dialect: Send + Sync {
    fn flavor(&self) -> SqlFlavor;

    /// Always-quoted identifier, safe for any input.
    fn quote_ident(&self, ident: &str) -> String;

    /// Quotes only when needed. Used for names shown to and copied by the user.
    fn display_ident(&self, ident: &str) -> String {
        if self.needs_quotes(ident) { self.quote_ident(ident) } else { ident.to_owned() }
    }

    fn needs_quotes(&self, ident: &str) -> bool;

    /// String literal for previews. Never used to build executed SQL.
    fn quote_literal(&self, value: &str) -> String;

    /// Placeholder for the `index`-th (1-based) text parameter targeting a
    /// column of `column_type`.
    fn placeholder(&self, index: usize, column_type: &str) -> String;

    /// Expression that reads a column as text for display when its type has no
    /// native decoder.
    fn cast_to_text(&self, quoted_column: &str) -> String;

    /// The container that qualifies an object name: the schema (`PostgreSQL`)
    /// or the database (`MySQL`).
    fn container<'a>(&self, object: &'a ObjectRef) -> Option<&'a str> {
        match self.flavor() {
            SqlFlavor::Postgres => object.schema.as_deref(),
            SqlFlavor::Mysql => Some(object.database.as_str()),
        }
        .filter(|c| !c.is_empty())
    }

    /// Fully quoted `schema.table` (Postgres) or `` `db`.`table` `` (MySQL).
    fn quoted_name(&self, object: &ObjectRef) -> String {
        join(self.container(object), &object.name, |s| self.quote_ident(s))
    }

    /// Minimal-quoted name for copying and generated templates.
    fn display_name(&self, object: &ObjectRef) -> String {
        join(self.container(object), &object.name, |s| self.display_ident(s))
    }
}

pub struct PostgresDialect;
pub struct MysqlDialect;

impl Dialect for PostgresDialect {
    fn flavor(&self) -> SqlFlavor {
        SqlFlavor::Postgres
    }

    fn quote_ident(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }

    fn needs_quotes(&self, ident: &str) -> bool {
        let mut chars = ident.chars();
        let simple = chars.next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$');
        !simple || is_reserved(ident, POSTGRES_RESERVED)
    }

    fn quote_literal(&self, value: &str) -> String {
        format!("'{}'", value.replace('\'', "''"))
    }

    fn placeholder(&self, index: usize, column_type: &str) -> String {
        // Text parameters are converted by the server using the column's own
        // input function, so exact values (int8, numeric, timestamptz) survive.
        format!("${index}::text::{column_type}")
    }

    fn cast_to_text(&self, quoted_column: &str) -> String {
        format!("{quoted_column}::text")
    }
}

impl Dialect for MysqlDialect {
    fn flavor(&self) -> SqlFlavor {
        SqlFlavor::Mysql
    }

    fn quote_ident(&self, ident: &str) -> String {
        format!("`{}`", ident.replace('`', "``"))
    }

    fn needs_quotes(&self, ident: &str) -> bool {
        let simple = !ident.is_empty()
            && ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
            && !ident.chars().all(|c| c.is_ascii_digit());
        !simple || is_reserved(ident, MYSQL_RESERVED)
    }

    fn quote_literal(&self, value: &str) -> String {
        format!("'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
    }

    fn placeholder(&self, _index: usize, _column_type: &str) -> String {
        "?".to_owned()
    }

    fn cast_to_text(&self, quoted_column: &str) -> String {
        format!("CAST({quoted_column} AS CHAR)")
    }
}

pub fn dialect_for(flavor: SqlFlavor) -> &'static dyn Dialect {
    match flavor {
        SqlFlavor::Postgres => &PostgresDialect,
        SqlFlavor::Mysql => &MysqlDialect,
    }
}

fn join(container: Option<&str>, name: &str, quote: impl Fn(&str) -> String) -> String {
    match container {
        Some(c) => format!("{}.{}", quote(c), quote(name)),
        None => quote(name),
    }
}

fn is_reserved(ident: &str, list: &[&str]) -> bool {
    list.iter().any(|k| k.eq_ignore_ascii_case(ident))
}

const POSTGRES_RESERVED: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "authorization",
    "binary",
    "both",
    "case",
    "cast",
    "check",
    "collate",
    "collation",
    "column",
    "concurrently",
    "constraint",
    "create",
    "cross",
    "current_catalog",
    "current_date",
    "current_role",
    "current_schema",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "false",
    "fetch",
    "for",
    "foreign",
    "freeze",
    "from",
    "full",
    "grant",
    "group",
    "having",
    "ilike",
    "in",
    "initially",
    "inner",
    "intersect",
    "into",
    "is",
    "isnull",
    "join",
    "lateral",
    "leading",
    "left",
    "like",
    "limit",
    "localtime",
    "localtimestamp",
    "natural",
    "not",
    "notnull",
    "null",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "outer",
    "overlaps",
    "placing",
    "primary",
    "references",
    "returning",
    "right",
    "select",
    "session_user",
    "similar",
    "some",
    "symmetric",
    "system_user",
    "table",
    "tablesample",
    "then",
    "to",
    "trailing",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "variadic",
    "verbose",
    "when",
    "where",
    "window",
    "with",
];

const MYSQL_RESERVED: &[&str] = &[
    "add",
    "all",
    "alter",
    "analyze",
    "and",
    "as",
    "asc",
    "before",
    "between",
    "bigint",
    "binary",
    "blob",
    "both",
    "by",
    "call",
    "cascade",
    "case",
    "change",
    "char",
    "character",
    "check",
    "collate",
    "column",
    "condition",
    "constraint",
    "continue",
    "convert",
    "create",
    "cross",
    "cube",
    "current_date",
    "current_time",
    "current_timestamp",
    "current_user",
    "cursor",
    "database",
    "databases",
    "dec",
    "decimal",
    "declare",
    "default",
    "delete",
    "desc",
    "describe",
    "distinct",
    "div",
    "double",
    "drop",
    "dual",
    "each",
    "else",
    "elseif",
    "enclosed",
    "escaped",
    "except",
    "exists",
    "exit",
    "explain",
    "false",
    "fetch",
    "float",
    "for",
    "force",
    "foreign",
    "from",
    "fulltext",
    "function",
    "generated",
    "grant",
    "group",
    "groups",
    "having",
    "if",
    "ignore",
    "in",
    "index",
    "inner",
    "insert",
    "int",
    "integer",
    "interval",
    "into",
    "is",
    "join",
    "key",
    "keys",
    "kill",
    "lateral",
    "leading",
    "leave",
    "left",
    "like",
    "limit",
    "lines",
    "load",
    "lock",
    "long",
    "loop",
    "match",
    "mod",
    "natural",
    "not",
    "null",
    "numeric",
    "of",
    "on",
    "option",
    "or",
    "order",
    "out",
    "outer",
    "over",
    "partition",
    "precision",
    "primary",
    "procedure",
    "range",
    "read",
    "real",
    "recursive",
    "references",
    "regexp",
    "rename",
    "repeat",
    "replace",
    "require",
    "restrict",
    "return",
    "revoke",
    "right",
    "rlike",
    "row",
    "rows",
    "schema",
    "schemas",
    "select",
    "set",
    "show",
    "smallint",
    "spatial",
    "sql",
    "table",
    "terminated",
    "then",
    "to",
    "trailing",
    "trigger",
    "true",
    "union",
    "unique",
    "unlock",
    "unsigned",
    "update",
    "usage",
    "use",
    "using",
    "values",
    "varchar",
    "when",
    "where",
    "while",
    "window",
    "with",
    "write",
    "xor",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{NodeKind, ObjectRef};

    fn obj(db: &str, schema: Option<&str>, name: &str) -> ObjectRef {
        ObjectRef { kind: NodeKind::Table, database: db.into(), schema: schema.map(Into::into), name: name.into() }
    }

    #[test]
    fn postgres_quoting() {
        let d = PostgresDialect;
        assert_eq!(d.quote_ident(r#"we"ird"#), r#""we""ird""#);
        assert_eq!(d.display_ident("orders"), "orders");
        assert_eq!(d.display_ident("Orders"), "\"Orders\"");
        assert_eq!(d.display_ident("order"), "\"order\"");
        assert_eq!(d.display_ident("line items"), "\"line items\"");
        assert_eq!(d.display_name(&obj("app", Some("public"), "orders")), "public.orders");
        assert_eq!(d.quoted_name(&obj("app", Some("public"), "orders")), "\"public\".\"orders\"");
        assert_eq!(d.quote_literal("it's"), "'it''s'");
        assert_eq!(d.placeholder(2, "numeric(12,2)"), "$2::text::numeric(12,2)");
    }

    #[test]
    fn mysql_quoting() {
        let d = MysqlDialect;
        assert_eq!(d.quote_ident("a`b"), "`a``b`");
        assert_eq!(d.display_ident("orders"), "orders");
        assert_eq!(d.display_ident("order"), "`order`");
        assert_eq!(d.display_ident("123"), "`123`");
        assert_eq!(d.display_name(&obj("shop", None, "orders")), "shop.orders");
        assert_eq!(d.quote_literal(r"a\'b"), r"'a\\''b'");
    }
}
