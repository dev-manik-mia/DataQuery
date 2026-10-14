//! SQL builders shared by every SQL adapter. Identifiers are quoted by the
//! dialect and values are always parameters; user-typed filters are validated
//! to be a single expression.

use crate::edit::{BoundStatement, ColumnValue, RowChange};
use crate::error::{DbError, DbResult};
use crate::node::ObjectRef;
use crate::schema::{ColumnInfo, TableStructure};
use crate::table::{RowFilter, SortSpec};

use super::dialect::Dialect;
use super::lexer::{TokenKind, tokenize};

pub const MAX_FILTER_LEN: usize = 8_000;
pub const MAX_CHANGES: usize = 10_000;

/// Validates a user-typed `WHERE` expression. It must not contain statement
/// separators and its parentheses must balance, so it cannot escape the
/// surrounding query. Returns `None` for an empty filter.
pub fn validate_filter(d: &dyn Dialect, filter: Option<&str>) -> DbResult<Option<String>> {
    let Some(filter) = filter.map(str::trim).filter(|f| !f.is_empty()) else {
        return Ok(None);
    };
    if filter.len() > MAX_FILTER_LEN {
        return Err(DbError::invalid("The filter is too long."));
    }
    let mut depth = 0i32;
    for token in tokenize(filter, d.flavor()) {
        match token.kind {
            TokenKind::Semicolon => {
                return Err(DbError::invalid("The filter must be a single expression without `;`."));
            }
            TokenKind::OpenParen => depth += 1,
            TokenKind::CloseParen => {
                depth -= 1;
                if depth < 0 {
                    return Err(DbError::invalid("The filter has an unmatched `)`."));
                }
            }
            TokenKind::Comment if token.text(filter).starts_with("--") || token.text(filter).starts_with('#') => {
                return Err(DbError::invalid("Line comments are not allowed in a filter."));
            }
            TokenKind::String | TokenKind::QuotedIdent | TokenKind::Comment => {
                let text = token.text(filter);
                let last = text.chars().last();
                let first = text.chars().next();
                let closed = text.len() >= 2
                    && match token.kind {
                        TokenKind::Comment => text.ends_with("*/"),
                        _ => first == last || text.starts_with('$') || text.starts_with(['e', 'E']),
                    };
                if !closed {
                    return Err(DbError::invalid("The filter has an unterminated quote or comment."));
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(DbError::invalid("The filter has an unmatched `(`."));
    }
    Ok(Some(filter.to_owned()))
}

/// `SELECT` list for the table view: every column, cast to text when the
/// adapter cannot decode its type natively.
pub fn select_list(d: &dyn Dialect, columns: &[ColumnInfo]) -> String {
    if columns.is_empty() {
        return "*".to_owned();
    }
    columns
        .iter()
        .map(|c| {
            let q = d.quote_ident(&c.name);
            if c.read_as_text { format!("{} AS {q}", d.cast_to_text(&q)) } else { q }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A query with its text parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundQuery {
    pub sql: String,
    pub params: Vec<Option<String>>,
}

/// Escapes `%`, `_` and `\` for a `LIKE` pattern that uses `\` as the escape.
pub fn like_contains(text: &str) -> String {
    format!("%{}%", text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"))
}

/// `WHERE` clause for a [`RowFilter`]: the user's expression plus parameterised
/// matches and searches.
fn where_clause(p: &mut Params<'_>, structure: &TableStructure, rows: &RowFilter) -> DbResult<String> {
    let d = p.dialect;
    let mut parts = Vec::new();
    if let Some(filter) = validate_filter(d, rows.filter.as_deref())? {
        parts.push(format!("({filter})"));
    }
    for m in &rows.matches {
        let col = column(structure, &m.column)?;
        let name = d.quote_ident(&col.name);
        parts.push(match m.value.as_deref() {
            None => format!("{name} IS NULL"),
            Some(v) => format!("{name} = {}", p.push(col, Some(v))),
        });
    }
    for search in rows.searches.iter().filter(|s| !s.contains.is_empty()) {
        let col = column(structure, &search.column)?;
        if search.contains.len() > 1000 {
            return Err(DbError::invalid("A column search is limited to 1000 characters."));
        }
        let text = ColumnInfo { data_type: "text".into(), ..col.clone() };
        let placeholder = p.push(&text, Some(&like_contains(&search.contains)));
        parts.push(d.search_predicate(&d.quote_ident(&col.name), &placeholder));
    }
    Ok(if parts.is_empty() { String::new() } else { format!(" WHERE {}", parts.join(" AND ")) })
}

fn order_clause(d: &dyn Dialect, structure: &TableStructure, sort: Option<&SortSpec>) -> DbResult<String> {
    if let Some(sort) = sort {
        if structure.column(&sort.column).is_none() {
            return Err(DbError::invalid(format!("Unknown sort column `{}`.", sort.column)));
        }
        let dir = if sort.descending { "DESC" } else { "ASC" };
        return Ok(format!(" ORDER BY {} {dir}", d.quote_ident(&sort.column)));
    }
    // Stable paging: default to the row identity.
    if structure.row_identity.is_empty() {
        return Ok(String::new());
    }
    let keys: Vec<_> = structure.row_identity.iter().map(|c| d.quote_ident(c)).collect();
    Ok(format!(" ORDER BY {}", keys.join(", ")))
}

/// Unbounded select used by the table view (with `LIMIT`) and table export (without).
pub fn table_select(
    d: &dyn Dialect,
    structure: &TableStructure,
    rows: &RowFilter,
    sort: Option<&SortSpec>,
) -> DbResult<BoundQuery> {
    let mut p = Params { dialect: d, inline: false, values: Vec::new() };
    let where_ = where_clause(&mut p, structure, rows)?;
    let sql = format!(
        "SELECT {} FROM {}{where_}{}",
        select_list(d, &structure.columns),
        d.quoted_name(&structure.object),
        order_clause(d, structure, sort)?
    );
    Ok(BoundQuery { sql, params: p.values })
}

/// Page query; fetches one extra row to know whether another page exists.
pub fn page_query(
    d: &dyn Dialect,
    structure: &TableStructure,
    rows: &RowFilter,
    sort: Option<&SortSpec>,
    offset: u64,
    limit: u32,
) -> DbResult<BoundQuery> {
    let mut q = table_select(d, structure, rows, sort)?;
    q.sql = format!("{} LIMIT {} OFFSET {offset}", q.sql, u64::from(limit) + 1);
    Ok(q)
}

pub fn count_query(d: &dyn Dialect, structure: &TableStructure, rows: &RowFilter) -> DbResult<BoundQuery> {
    let mut p = Params { dialect: d, inline: false, values: Vec::new() };
    let where_ = where_clause(&mut p, structure, rows)?;
    Ok(BoundQuery {
        sql: format!("SELECT count(*) FROM {}{where_}", d.quoted_name(&structure.object)),
        params: p.values,
    })
}

/// `SELECT … LIMIT 100;` template for "Generate SELECT".
pub fn select_template(d: &dyn Dialect, object: &ObjectRef, columns: &[String]) -> String {
    let list = if columns.is_empty() {
        "*".to_owned()
    } else {
        columns.iter().map(|c| d.display_ident(c)).collect::<Vec<_>>().join(", ")
    };
    format!("SELECT {list}\nFROM {}\nLIMIT 100;", d.display_name(object))
}

/// Collects parameters and produces placeholders, or inlines literals for previews.
struct Params<'d> {
    dialect: &'d dyn Dialect,
    inline: bool,
    values: Vec<Option<String>>,
}

impl Params<'_> {
    fn push(&mut self, column: &ColumnInfo, value: Option<&str>) -> String {
        if self.inline {
            return value.map_or_else(|| "NULL".to_owned(), |v| self.dialect.quote_literal(v));
        }
        self.values.push(value.map(str::to_owned));
        self.dialect.placeholder(self.values.len(), &column.data_type)
    }
}

fn column<'a>(structure: &'a TableStructure, name: &str) -> DbResult<&'a ColumnInfo> {
    structure.column(name).ok_or_else(|| DbError::invalid(format!("Unknown column `{name}`.")))
}

fn check_values(structure: &TableStructure, values: &[ColumnValue]) -> DbResult<()> {
    for (i, v) in values.iter().enumerate() {
        let col = column(structure, &v.column)?;
        if !col.editable {
            return Err(DbError::invalid(format!(
                "Column `{}` ({}) cannot be edited in the grid.",
                col.name, col.data_type
            )));
        }
        if values[..i].iter().any(|o| o.column == v.column) {
            return Err(DbError::invalid(format!("Column `{}` is set twice.", v.column)));
        }
    }
    Ok(())
}

fn check_key(structure: &TableStructure, key: &[ColumnValue]) -> DbResult<()> {
    let same = key.len() == structure.row_identity.len()
        && structure.row_identity.iter().all(|c| key.iter().any(|k| &k.column == c));
    if !same {
        return Err(DbError::invalid("The row key does not match the table's primary key."));
    }
    for k in key.iter().filter(|k| k.value.is_none()) {
        if !column(structure, &k.column)?.nullable {
            return Err(DbError::invalid(format!("Key column `{}` cannot be NULL.", k.column)));
        }
    }
    Ok(())
}

fn key_predicate(p: &mut Params<'_>, structure: &TableStructure, key: &[ColumnValue]) -> DbResult<String> {
    let mut parts = Vec::with_capacity(key.len());
    for k in key {
        let col = column(structure, &k.column)?;
        let name = p.dialect.quote_ident(&col.name);
        // `IS NULL` rather than a null-safe operator, so indexes stay usable.
        parts.push(match k.value.as_deref() {
            None => format!("{name} IS NULL"),
            Some(v) => format!("{name} = {}", p.push(col, Some(v))),
        });
    }
    Ok(parts.join(" AND "))
}

fn build_one(p: &mut Params<'_>, structure: &TableStructure, change: &RowChange) -> DbResult<(String, Option<u64>)> {
    let d = p.dialect;
    let table = d.quoted_name(&structure.object);
    Ok(match change {
        RowChange::Insert { values } => {
            check_values(structure, values)?;
            if values.is_empty() {
                (format!("INSERT INTO {table} DEFAULT VALUES"), Some(1))
            } else {
                let names: Vec<_> = values.iter().map(|v| d.quote_ident(&v.column)).collect();
                let mut placeholders = Vec::with_capacity(values.len());
                for v in values {
                    placeholders.push(p.push(column(structure, &v.column)?, v.value.as_deref()));
                }
                (format!("INSERT INTO {table} ({}) VALUES ({})", names.join(", "), placeholders.join(", ")), Some(1))
            }
        }
        RowChange::Update { key, values } => {
            check_key(structure, key)?;
            check_values(structure, values)?;
            if values.is_empty() {
                return Err(DbError::invalid("An update must change at least one column."));
            }
            let mut sets = Vec::with_capacity(values.len());
            for v in values {
                let value = p.push(column(structure, &v.column)?, v.value.as_deref());
                sets.push(format!("{} = {value}", d.quote_ident(&v.column)));
            }
            let predicate = key_predicate(p, structure, key)?;
            (format!("UPDATE {table} SET {} WHERE {predicate}", sets.join(", ")), Some(1))
        }
        RowChange::Delete { key } => {
            check_key(structure, key)?;
            let predicate = key_predicate(p, structure, key)?;
            (format!("DELETE FROM {table} WHERE {predicate}"), Some(1))
        }
    })
}

fn ensure_editable(structure: &TableStructure, changes: &[RowChange]) -> DbResult<()> {
    if !structure.editable() {
        return Err(DbError::invalid("This object has no primary key or unique key, so rows cannot be edited safely."));
    }
    if changes.is_empty() || changes.len() > MAX_CHANGES {
        return Err(DbError::invalid(format!("Between 1 and {MAX_CHANGES} changes can be saved at once.")));
    }
    Ok(())
}

/// Parameterised statements for a set of grid changes. Each must affect exactly one row.
pub fn change_statements(
    d: &dyn Dialect,
    structure: &TableStructure,
    changes: &[RowChange],
) -> DbResult<Vec<BoundStatement>> {
    ensure_editable(structure, changes)?;
    changes
        .iter()
        .map(|change| {
            let mut p = Params { dialect: d, inline: false, values: Vec::new() };
            let (sql, expect_rows) = build_one(&mut p, structure, change)?;
            Ok(BoundStatement { sql, params: p.values, expect_rows })
        })
        .collect()
}

/// Human-readable SQL for "Preview SQL". Not executed.
pub fn preview_changes(d: &dyn Dialect, structure: &TableStructure, changes: &[RowChange]) -> DbResult<String> {
    ensure_editable(structure, changes)?;
    let mut out = String::new();
    for change in changes {
        let mut p = Params { dialect: d, inline: true, values: Vec::new() };
        out.push_str(&build_one(&mut p, structure, change)?.0);
        out.push_str(";\n");
    }
    Ok(out)
}

/// Multi-row insert for imports.
pub fn insert_batch(
    d: &dyn Dialect,
    structure: &TableStructure,
    columns: &[String],
    rows: &[Vec<Option<String>>],
) -> DbResult<BoundStatement> {
    let cols: Vec<&ColumnInfo> = columns.iter().map(|c| column(structure, c)).collect::<DbResult<_>>()?;
    let mut p = Params { dialect: d, inline: false, values: Vec::with_capacity(cols.len() * rows.len()) };
    let mut tuples = Vec::with_capacity(rows.len());
    for row in rows {
        if row.len() != cols.len() {
            return Err(DbError::internal("import row width does not match the column mapping"));
        }
        let values: Vec<String> = cols.iter().zip(row).map(|(c, v)| p.push(c, v.as_deref())).collect();
        tuples.push(format!("({})", values.join(", ")));
    }
    let names: Vec<_> = cols.iter().map(|c| d.quote_ident(&c.name)).collect();
    Ok(BoundStatement {
        sql: format!(
            "INSERT INTO {} ({}) VALUES {}",
            d.quoted_name(&structure.object),
            names.join(", "),
            tuples.join(", ")
        ),
        params: p.values,
        expect_rows: Some(rows.len() as u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeKind;
    use crate::sql::dialect::{MysqlDialect, PostgresDialect};

    fn col(name: &str, ty: &str, pk: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: ty.into(),
            nullable: !pk,
            default: None,
            primary_key: pk,
            generated: false,
            editable: ty != "bytea",
            numeric: false,
            comment: None,
            read_as_text: ty == "geometry",
        }
    }

    fn orders() -> TableStructure {
        TableStructure {
            object: ObjectRef {
                kind: NodeKind::Table,
                database: "app".into(),
                schema: Some("public".into()),
                name: "orders".into(),
            },
            columns: vec![
                col("id", "bigint", true),
                col("total", "numeric(12,2)", false),
                col("geo", "geometry", false),
                col("raw", "bytea", false),
            ],
            constraints: vec![],
            indexes: vec![],
            row_identity: vec!["id".into()],
            full_row_key: Vec::new(),
            comment: None,
            row_estimate: None,
        }
    }

    fn cv(c: &str, v: Option<&str>) -> ColumnValue {
        ColumnValue { column: c.into(), value: v.map(Into::into) }
    }

    #[test]
    fn filter_validation() {
        let d = &PostgresDialect;
        assert_eq!(validate_filter(d, Some("  ")).unwrap(), None);
        assert!(validate_filter(d, Some("status = 'a;b'")).unwrap().is_some());
        assert!(validate_filter(d, Some("1=1; drop table t")).is_err());
        assert!(validate_filter(d, Some("1=1) OR (1=1")).is_err());
        assert!(validate_filter(d, Some("a = 'x")).is_err());
        assert!(validate_filter(d, Some("a = 1 -- ")).is_err());
        assert!(validate_filter(d, Some("a = 1 /* x")).is_err());
        assert!(validate_filter(d, Some("(a = 1) /* ok */")).unwrap().is_some());
    }

    #[test]
    fn page_query_quotes_and_casts() {
        let s = orders();
        let rows = RowFilter { filter: Some("total > 5".into()), ..RowFilter::default() };
        let q = page_query(
            &PostgresDialect,
            &s,
            &rows,
            Some(&SortSpec { column: "total".into(), descending: true }),
            100,
            50,
        )
        .unwrap();
        assert_eq!(
            q.sql,
            r#"SELECT "id", "total", "geo"::text AS "geo", "raw" FROM "public"."orders" WHERE (total > 5) ORDER BY "total" DESC LIMIT 51 OFFSET 100"#
        );
        assert!(q.params.is_empty());
        assert!(
            page_query(
                &PostgresDialect,
                &s,
                &RowFilter::default(),
                Some(&SortSpec { column: "nope\"".into(), descending: false }),
                0,
                1
            )
            .is_err()
        );
    }

    #[test]
    fn matches_and_searches_are_parameters() {
        let s = orders();
        let rows = RowFilter {
            filter: None,
            matches: vec![cv("id", Some("7")), cv("total", None)],
            searches: vec![crate::table::ColumnSearch { column: "geo".into(), contains: "50%_x".into() }],
        };
        let q = count_query(&PostgresDialect, &s, &rows).unwrap();
        assert_eq!(
            q.sql,
            r#"SELECT count(*) FROM "public"."orders" WHERE "id" = $1::text::bigint AND "total" IS NULL AND "geo"::text ILIKE $2::text::text"#
        );
        assert_eq!(q.params, vec![Some("7".into()), Some(r"%50\%\_x%".into())]);
        let my = table_select(&MysqlDialect, &s, &rows, None).unwrap();
        assert!(my.sql.contains("WHERE `id` = ? AND `total` IS NULL AND CAST(`geo` AS CHAR) LIKE ?"), "{}", my.sql);
        let unknown = RowFilter { matches: vec![cv("nope", Some("1"))], ..RowFilter::default() };
        assert!(count_query(&PostgresDialect, &s, &unknown).is_err());
    }

    #[test]
    fn change_statements_are_parameterised() {
        let s = orders();
        let changes = vec![
            RowChange::Update {
                key: vec![cv("id", Some("9007199254740993"))],
                values: vec![cv("total", Some("84.50"))],
            },
            RowChange::Insert { values: vec![cv("total", None)] },
            RowChange::Delete { key: vec![cv("id", Some("7"))] },
        ];
        let stmts = change_statements(&PostgresDialect, &s, &changes).unwrap();
        assert_eq!(
            stmts[0].sql,
            r#"UPDATE "public"."orders" SET "total" = $1::text::numeric(12,2) WHERE "id" = $2::text::bigint"#
        );
        assert_eq!(stmts[0].params, vec![Some("84.50".into()), Some("9007199254740993".into())]);
        assert_eq!(stmts[1].sql, r#"INSERT INTO "public"."orders" ("total") VALUES ($1::text::numeric(12,2))"#);
        assert_eq!(stmts[1].params, vec![None]);
        assert_eq!(stmts[2].sql, r#"DELETE FROM "public"."orders" WHERE "id" = $1::text::bigint"#);
        assert!(stmts.iter().all(|s| s.expect_rows == Some(1)));
    }

    #[test]
    fn preview_inlines_literals() {
        let s = orders();
        let changes =
            vec![RowChange::Update { key: vec![cv("id", Some("1"))], values: vec![cv("total", Some("it's"))] }];
        assert_eq!(
            preview_changes(&MysqlDialect, &s, &changes).unwrap(),
            "UPDATE `app`.`orders` SET `total` = 'it''s' WHERE `id` = '1';\n"
        );
    }

    #[test]
    fn rejects_bad_changes() {
        let s = orders();
        let bad_key = RowChange::Delete { key: vec![cv("total", Some("1"))] };
        assert!(change_statements(&PostgresDialect, &s, &[bad_key]).is_err());
        let unknown = RowChange::Insert { values: vec![cv("nope", Some("1"))] };
        assert!(change_statements(&PostgresDialect, &s, &[unknown]).is_err());
        let binary = RowChange::Insert { values: vec![cv("raw", Some("1"))] };
        assert!(change_statements(&PostgresDialect, &s, &[binary]).is_err());
        let mut no_pk = orders();
        no_pk.row_identity.clear();
        assert!(change_statements(&PostgresDialect, &no_pk, &[RowChange::Insert { values: vec![] }]).is_err());
    }

    #[test]
    fn full_row_keys_match_nulls_with_is_null() {
        let mut s = orders();
        s.row_identity = vec!["id".into(), "total".into()];
        let delete = RowChange::Delete { key: vec![cv("id", Some("1")), cv("total", None)] };
        let stmts = change_statements(&PostgresDialect, &s, std::slice::from_ref(&delete)).unwrap();
        assert_eq!(stmts[0].sql, r#"DELETE FROM "public"."orders" WHERE "id" = $1::text::bigint AND "total" IS NULL"#);
        assert_eq!(stmts[0].params, vec![Some("1".into())]);
        assert_eq!(stmts[0].expect_rows, Some(1));
        // A NOT NULL key column never matches NULL.
        let null_id = RowChange::Delete { key: vec![cv("id", None), cv("total", Some("1"))] };
        assert!(change_statements(&PostgresDialect, &s, &[null_id]).is_err());
    }

    #[test]
    fn comparable_types() {
        assert!(PostgresDialect.comparable("numeric(12,2)") && PostgresDialect.comparable("text[]"));
        assert!(!PostgresDialect.comparable("json") && !PostgresDialect.comparable("point"));
        assert!(MysqlDialect.comparable("varchar(10)") && !MysqlDialect.comparable("json"));
    }

    #[test]
    fn import_batch() {
        let s = orders();
        let stmt = insert_batch(
            &MysqlDialect,
            &s,
            &["id".into(), "total".into()],
            &[vec![Some("1".into()), None], vec![Some("2".into()), Some("3".into())]],
        )
        .unwrap();
        assert_eq!(stmt.sql, "INSERT INTO `app`.`orders` (`id`, `total`) VALUES (?, ?), (?, ?)");
        assert_eq!(stmt.params.len(), 4);
        assert_eq!(stmt.expect_rows, Some(2));
    }
}
