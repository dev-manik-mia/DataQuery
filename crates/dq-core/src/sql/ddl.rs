//! Schema changes from the structure forms. Each change becomes reviewed SQL:
//! identifiers are quoted by the dialect, and user-typed fragments (types,
//! defaults, check expressions) must be single expressions without `;`.

use std::fmt::Write;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use super::builder::validate_filter;
use super::dialect::{Dialect, SqlFlavor};
use super::lexer::{TokenKind, tokenize};
use super::table_actions::{self, TruncateOptions};
use crate::error::{DbError, DbResult};
use crate::node::{NodeKind, ObjectRef};
use crate::schema::{ColumnInfo, ConstraintKind, KeyRole, KeyRoleKind, TableStructure};

/// A column in a create-table or add-column form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ColumnDef {
    pub name: String,
    /// Type as the engine spells it, e.g. `numeric(12,2)` or `varchar(255)`.
    pub data_type: String,
    pub nullable: bool,
    /// SQL expression, e.g. `'pending'` or `now()`.
    pub default: Option<String>,
    /// Identity (PostgreSQL) or `AUTO_INCREMENT` (MySQL).
    pub identity: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "action", rename_all = "snake_case")]
#[ts(export)]
pub enum DefaultChange {
    Set { expression: String },
    Drop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct IndexColumn {
    pub name: String,
    pub descending: bool,
}

/// One schema change. `table` is the table it applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "op", rename_all = "snake_case")]
#[ts(export)]
pub enum SchemaChange {
    CreateTable {
        table: ObjectRef,
        columns: Vec<ColumnDef>,
        primary_key: Vec<String>,
    },
    DropTable {
        table: ObjectRef,
        cascade: bool,
    },
    RenameTable {
        table: ObjectRef,
        new_name: String,
    },
    AddColumn {
        table: ObjectRef,
        column: ColumnDef,
    },
    DropColumn {
        table: ObjectRef,
        column: String,
    },
    RenameColumn {
        table: ObjectRef,
        column: String,
        new_name: String,
    },
    AlterColumn {
        table: ObjectRef,
        column: String,
        data_type: Option<String>,
        nullable: Option<bool>,
        default: Option<DefaultChange>,
    },
    CreateIndex {
        table: ObjectRef,
        name: String,
        columns: Vec<IndexColumn>,
        unique: bool,
    },
    DropIndex {
        table: ObjectRef,
        name: String,
    },
    AddForeignKey {
        table: ObjectRef,
        name: String,
        columns: Vec<String>,
        target: ObjectRef,
        target_columns: Vec<String>,
        on_delete: Option<String>,
        on_update: Option<String>,
    },
    AddUnique {
        table: ObjectRef,
        name: String,
        columns: Vec<String>,
    },
    AddCheck {
        table: ObjectRef,
        name: String,
        expression: String,
    },
    DropConstraint {
        table: ObjectRef,
        name: String,
        kind: ConstraintKind,
    },
    /// Removes every row with `TRUNCATE` (`SQLite`: `DELETE`).
    TruncateTable {
        table: ObjectRef,
        restart_identity: bool,
        cascade: bool,
        disable_foreign_keys: bool,
    },
    /// Deletes every row with `DELETE`: fires triggers, works where `TRUNCATE` is blocked.
    EmptyTable {
        table: ObjectRef,
    },
    /// Copies the table under a new name in the same schema.
    DuplicateTable {
        table: ObjectRef,
        new_name: String,
        with_data: bool,
    },
}

impl SchemaChange {
    pub fn table(&self) -> &ObjectRef {
        match self {
            Self::CreateTable { table, .. }
            | Self::DropTable { table, .. }
            | Self::RenameTable { table, .. }
            | Self::AddColumn { table, .. }
            | Self::DropColumn { table, .. }
            | Self::RenameColumn { table, .. }
            | Self::AlterColumn { table, .. }
            | Self::CreateIndex { table, .. }
            | Self::DropIndex { table, .. }
            | Self::AddForeignKey { table, .. }
            | Self::AddUnique { table, .. }
            | Self::AddCheck { table, .. }
            | Self::DropConstraint { table, .. }
            | Self::TruncateTable { table, .. }
            | Self::EmptyTable { table }
            | Self::DuplicateTable { table, .. } => table,
        }
    }

    /// The change needs the table's current structure (MySQL `MODIFY COLUMN`,
    /// SQL Server `ALTER COLUMN`, which restate the type; copied columns).
    pub fn needs_structure(&self, flavor: SqlFlavor) -> bool {
        match self {
            Self::AlterColumn { data_type, nullable, .. } => {
                matches!(flavor, SqlFlavor::Mysql | SqlFlavor::Sqlserver | SqlFlavor::Clickhouse)
                    && (data_type.is_some() || nullable.is_some())
            }
            Self::DuplicateTable { .. } => !matches!(flavor, SqlFlavor::Clickhouse | SqlFlavor::Snowflake),
            _ => false,
        }
    }

    /// The change needs the table's `CREATE TABLE` text ([`crate::adapter::Session::ddl`]).
    pub fn needs_definition(&self, flavor: SqlFlavor) -> bool {
        match self {
            Self::DuplicateTable { .. } => table_actions::needs_definition(flavor),
            Self::TruncateTable { restart_identity, .. } => *restart_identity && flavor == SqlFlavor::Sqlite,
            _ => false,
        }
    }

    /// Removes rows: confirmed as destructive whatever SQL the engine runs.
    pub fn deletes_rows(&self) -> bool {
        matches!(self, Self::TruncateTable { .. } | Self::EmptyTable { .. })
    }
}

const FK_ACTIONS: &[&str] = &["CASCADE", "RESTRICT", "SET NULL", "SET DEFAULT", "NO ACTION"];

fn name(n: &str, what: &str) -> DbResult<()> {
    if n.trim().is_empty() || n.len() > 128 || n.chars().any(char::is_control) {
        return Err(DbError::invalid(format!("{what} must be 1–128 characters.")));
    }
    Ok(())
}

/// A type name: words, numbers, `(…)`, `[]`, `.` and, for `enum(…)`/`set(…)`, string literals.
pub fn validate_type(d: &dyn Dialect, data_type: &str) -> DbResult<String> {
    let t = data_type.trim();
    if t.is_empty() || t.len() > 200 {
        return Err(DbError::invalid("Enter a column type."));
    }
    let lower = t.to_ascii_lowercase();
    let strings_ok = lower.starts_with("enum") || lower.starts_with("set");
    let mut depth = 0i32;
    // CQL and BigQuery types nest with angle brackets: `map<text, frozen<list<int>>>`, `ARRAY<STRUCT<a INT64>>`.
    let mut angles = 0i32;
    let cql = matches!(d.flavor(), SqlFlavor::Cql | SqlFlavor::Bigquery);
    for token in tokenize(t, d.flavor()) {
        let ok = match token.kind {
            TokenKind::Word | TokenKind::Number | TokenKind::Whitespace | TokenKind::QuotedIdent => true,
            TokenKind::OpenParen => {
                depth += 1;
                true
            }
            TokenKind::CloseParen => {
                depth -= 1;
                depth >= 0
            }
            TokenKind::Punct if cql && token.text(t) == "<" => {
                angles += 1;
                true
            }
            TokenKind::Punct if cql && token.text(t) == ">" => {
                angles -= 1;
                angles >= 0
            }
            TokenKind::Punct => matches!(token.text(t), "," | "." | "[" | "]" | "[]"),
            TokenKind::String => strings_ok,
            _ => false,
        };
        if !ok {
            return Err(DbError::invalid(format!("`{t}` is not a valid column type.")));
        }
    }
    if depth != 0 || angles != 0 {
        return Err(DbError::invalid(format!("`{t}` has unbalanced parentheses.")));
    }
    Ok(t.to_owned())
}

/// A default or check expression: a single expression, like a filter.
fn expression(d: &dyn Dialect, e: &str, what: &str) -> DbResult<String> {
    validate_filter(d, Some(e))?.ok_or_else(|| DbError::invalid(format!("Enter the {what}.")))
}

fn column_list(d: &dyn Dialect, columns: &[String]) -> DbResult<String> {
    if columns.is_empty() {
        return Err(DbError::invalid("Choose at least one column."));
    }
    for c in columns {
        name(c, "Column names")?;
    }
    Ok(columns.iter().map(|c| d.quote_ident(c)).collect::<Vec<_>>().join(", "))
}

/// `ClickHouse` spells nullability in the type: `Nullable(T)`.
fn clickhouse_type(data_type: &str, nullable: bool) -> String {
    if nullable && !data_type.starts_with("Nullable(") {
        format!("Nullable({data_type})")
    } else {
        data_type.to_owned()
    }
}

fn column_def(d: &dyn Dialect, c: &ColumnDef) -> DbResult<String> {
    name(&c.name, "Column names")?;
    let data_type = validate_type(d, &c.data_type)?;
    let clickhouse = d.flavor() == SqlFlavor::Clickhouse;
    let cql = d.flavor() == SqlFlavor::Cql;
    if cql && c.default.as_deref().is_some_and(|v| !v.trim().is_empty()) {
        return Err(DbError::unsupported("Cassandra columns have no defaults."));
    }
    let data_type = if clickhouse { clickhouse_type(&data_type, c.nullable) } else { data_type };
    let mut out = format!("{} {data_type}", d.quote_ident(&c.name));
    if c.identity {
        let identity = d
            .identity_clause()
            .ok_or_else(|| DbError::unsupported("This engine has no identity columns; use a sequence or a default."))?;
        out.push_str(identity);
    }
    // Oracle needs DEFAULT before constraints; every engine accepts that order.
    if let Some(default) = c.default.as_deref().filter(|d| !d.trim().is_empty()) {
        let _ = write!(out, " DEFAULT {}", expression(d, default, "default")?);
    }
    // ClickHouse spells it with `Nullable(…)`; CQL has no NOT NULL (key columns are never null).
    if !c.nullable && !clickhouse && !cql {
        out.push_str(" NOT NULL");
    }
    Ok(out)
}

fn action(clause: &str, value: Option<&str>) -> DbResult<String> {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(String::new()),
        Some(v) => {
            let upper = v.to_ascii_uppercase();
            if !FK_ACTIONS.contains(&upper.as_str()) {
                return Err(DbError::invalid(format!("`{v}` is not a foreign key action.")));
            }
            Ok(format!(" {clause} {upper}"))
        }
    }
}

/// MySQL `MODIFY COLUMN` repeats the whole definition; unchanged parts come from `current`.
fn mysql_modify(
    d: &dyn Dialect,
    current: &ColumnInfo,
    data_type: Option<&str>,
    nullable: Option<bool>,
    default: Option<&DefaultChange>,
) -> DbResult<String> {
    if current.generated && !current.identity {
        return Err(DbError::unsupported("Generated columns are changed with SQL in the editor."));
    }
    let data_type = match data_type {
        Some(t) => validate_type(d, t)?,
        None => current.data_type.clone(),
    };
    let mut out = format!("MODIFY COLUMN {} {data_type}", d.quote_ident(&current.name));
    out.push_str(if nullable.unwrap_or(current.nullable) { " NULL" } else { " NOT NULL" });
    let default = match default {
        Some(DefaultChange::Set { expression: e }) => Some(expression(d, e, "default")?),
        Some(DefaultChange::Drop) => None,
        None => current.default.clone(),
    };
    if let Some(default) = default {
        let _ = write!(out, " DEFAULT {default}");
    }
    if current.identity {
        out.push_str(" AUTO_INCREMENT");
    }
    if let Some(comment) = &current.comment {
        let _ = write!(out, " COMMENT {}", d.quote_literal(comment));
    }
    Ok(out)
}

/// SQL Server statement that drops the default constraint of `column`, if it
/// has one. It has no `;` inside, so it stays one statement when split.
fn sqlserver_drop_default(d: &dyn Dialect, table: &ObjectRef, column: &str) -> String {
    let table_literal = d.quote_literal(&d.quoted_name(table));
    let column_literal = d.quote_literal(column);
    let drop = d.quote_literal(&format!("ALTER TABLE {} DROP CONSTRAINT ", d.quoted_name(table)));
    format!(
        "DECLARE @dq_default sysname = (SELECT dc.name FROM sys.default_constraints dc \
         JOIN sys.columns c ON c.object_id = dc.parent_object_id AND c.column_id = dc.parent_column_id \
         WHERE dc.parent_object_id = OBJECT_ID({table_literal}) AND c.name = {column_literal})\n\
         DECLARE @dq_sql nvarchar(max) = {drop} + QUOTENAME(@dq_default)\n\
         IF @dq_default IS NOT NULL EXEC (@dq_sql)"
    )
}

/// `ALTER TABLE … ALTER COLUMN` changes for one column.
fn alter_column(
    d: &dyn Dialect,
    table: &ObjectRef,
    column: &str,
    data_type: Option<&str>,
    nullable: Option<bool>,
    default: Option<&DefaultChange>,
    structure: Option<&TableStructure>,
) -> DbResult<Vec<String>> {
    let quoted_table = d.quoted_name(table);
    let alter = |body: String| format!("ALTER TABLE {quoted_table} {body}");
    let col = d.quote_ident(column);
    let current = || {
        structure.and_then(|s| s.column(column)).ok_or_else(|| DbError::invalid(format!("Unknown column `{column}`.")))
    };
    let default_sql = |e: &str| expression(d, e, "default");
    Ok(match d.flavor() {
        SqlFlavor::Mongodb | SqlFlavor::Partiql | SqlFlavor::Redis | SqlFlavor::QueryDsl | SqlFlavor::Cypher => {
            return Err(DbError::unsupported("Document collections have no SQL schema to change."));
        }
        SqlFlavor::Cql => {
            return Err(DbError::unsupported("Cassandra cannot change a column's type, nullability or default."));
        }
        SqlFlavor::Bigquery => {
            if nullable == Some(false) {
                return Err(DbError::unsupported(
                    "BigQuery cannot make an existing column REQUIRED; create a new table and copy the rows.",
                ));
            }
            let mut out = Vec::new();
            if let Some(t) = data_type {
                out.push(alter(format!("ALTER COLUMN {col} SET DATA TYPE {}", validate_type(d, t)?)));
            }
            if nullable == Some(true) {
                out.push(alter(format!("ALTER COLUMN {col} DROP NOT NULL")));
            }
            match default {
                Some(DefaultChange::Set { expression: e }) => {
                    out.push(alter(format!("ALTER COLUMN {col} SET DEFAULT {}", default_sql(e)?)));
                }
                Some(DefaultChange::Drop) => out.push(alter(format!("ALTER COLUMN {col} DROP DEFAULT"))),
                None => {}
            }
            out
        }
        SqlFlavor::Sqlite => {
            return Err(DbError::unsupported(
                "SQLite cannot change a column's type, nullability or default. Create a new table and copy the rows.",
            ));
        }
        SqlFlavor::Postgres => {
            let mut parts = Vec::new();
            if let Some(t) = data_type {
                let t = validate_type(d, t)?;
                parts.push(format!("ALTER COLUMN {col} TYPE {t} USING {col}::{t}"));
            }
            if let Some(n) = nullable {
                parts.push(format!("ALTER COLUMN {col} {} NOT NULL", if n { "DROP" } else { "SET" }));
            }
            match default {
                Some(DefaultChange::Set { expression: e }) => {
                    parts.push(format!("ALTER COLUMN {col} SET DEFAULT {}", default_sql(e)?));
                }
                Some(DefaultChange::Drop) => parts.push(format!("ALTER COLUMN {col} DROP DEFAULT")),
                None => {}
            }
            vec![alter(parts.join(",\n  "))]
        }
        SqlFlavor::Mysql if data_type.is_none() && nullable.is_none() => vec![alter(match default {
            Some(DefaultChange::Set { expression: e }) => format!("ALTER COLUMN {col} SET DEFAULT {}", default_sql(e)?),
            _ => format!("ALTER COLUMN {col} DROP DEFAULT"),
        })],
        SqlFlavor::Mysql => vec![alter(mysql_modify(d, current()?, data_type, nullable, default)?)],
        SqlFlavor::Sqlserver => {
            let mut out = Vec::new();
            if data_type.is_some() || nullable.is_some() {
                let current = current()?;
                let t = match data_type {
                    Some(t) => validate_type(d, t)?,
                    None => current.data_type.clone(),
                };
                let null = if nullable.unwrap_or(current.nullable) { "NULL" } else { "NOT NULL" };
                out.push(alter(format!("ALTER COLUMN {col} {t} {null}")));
            }
            if let Some(change) = default {
                out.push(sqlserver_drop_default(d, table, column));
                if let DefaultChange::Set { expression: e } = change {
                    out.push(alter(format!("ADD DEFAULT {} FOR {col}", default_sql(e)?)));
                }
            }
            out
        }
        SqlFlavor::Duckdb => {
            // One change per statement: DuckDB alters a column one property at a time.
            let mut out = Vec::new();
            if let Some(t) = data_type {
                let t = validate_type(d, t)?;
                out.push(alter(format!("ALTER COLUMN {col} TYPE {t}")));
            }
            if let Some(n) = nullable {
                out.push(alter(format!("ALTER COLUMN {col} {} NOT NULL", if n { "DROP" } else { "SET" })));
            }
            match default {
                Some(DefaultChange::Set { expression: e }) => {
                    out.push(alter(format!("ALTER COLUMN {col} SET DEFAULT {}", default_sql(e)?)));
                }
                Some(DefaultChange::Drop) => out.push(alter(format!("ALTER COLUMN {col} DROP DEFAULT"))),
                None => {}
            }
            out
        }
        SqlFlavor::Snowflake => {
            let mut out = Vec::new();
            if let Some(t) = data_type {
                out.push(alter(format!("ALTER COLUMN {col} SET DATA TYPE {}", validate_type(d, t)?)));
            }
            if let Some(n) = nullable {
                out.push(alter(format!("ALTER COLUMN {col} {} NOT NULL", if n { "DROP" } else { "SET" })));
            }
            match default {
                Some(DefaultChange::Set { .. }) => {
                    return Err(DbError::unsupported(
                        "Snowflake can only add a default when the column is created (or set a sequence default).",
                    ));
                }
                Some(DefaultChange::Drop) => out.push(alter(format!("ALTER COLUMN {col} DROP DEFAULT"))),
                None => {}
            }
            out
        }
        SqlFlavor::Clickhouse => {
            let mut out = Vec::new();
            if data_type.is_some() || nullable.is_some() {
                let current = current()?;
                let t = match data_type {
                    Some(t) => validate_type(d, t)?,
                    None => current.data_type.clone(),
                };
                let base = t.strip_prefix("Nullable(").and_then(|r| r.strip_suffix(')')).unwrap_or(&t).to_owned();
                let nullable = nullable.unwrap_or(current.nullable);
                out.push(alter(format!("MODIFY COLUMN {col} {}", clickhouse_type(&base, nullable))));
            }
            match default {
                Some(DefaultChange::Set { expression: e }) => {
                    out.push(alter(format!("MODIFY COLUMN {col} DEFAULT {}", default_sql(e)?)));
                }
                Some(DefaultChange::Drop) => out.push(alter(format!("MODIFY COLUMN {col} REMOVE DEFAULT"))),
                None => {}
            }
            out
        }
        SqlFlavor::Oracle => {
            let mut spec = col.clone();
            if let Some(t) = data_type {
                let _ = write!(spec, " {}", validate_type(d, t)?);
            }
            match default {
                Some(DefaultChange::Set { expression: e }) => {
                    let _ = write!(spec, " DEFAULT {}", default_sql(e)?);
                }
                Some(DefaultChange::Drop) => spec.push_str(" DEFAULT NULL"),
                None => {}
            }
            if let Some(n) = nullable {
                spec.push_str(if n { " NULL" } else { " NOT NULL" });
            }
            vec![alter(format!("MODIFY ({spec})"))]
        }
    })
}

/// Builds the statements for one change. `structure` is required when
/// [`SchemaChange::needs_structure`] says so.
pub fn statements(d: &dyn Dialect, change: &SchemaChange, structure: Option<&TableStructure>) -> DbResult<Vec<String>> {
    statements_with(d, change, structure, None)
}

/// [`statements`] with the table's `CREATE TABLE` text, required when
/// [`SchemaChange::needs_definition`] says so.
pub fn statements_with(
    d: &dyn Dialect,
    change: &SchemaChange,
    structure: Option<&TableStructure>,
    definition: Option<&str>,
) -> DbResult<Vec<String>> {
    let flavor = d.flavor();
    table_actions::table_actions(flavor, None).check(change)?;
    match change {
        SchemaChange::TruncateTable { table, restart_identity, cascade, disable_foreign_keys } => {
            let options = TruncateOptions {
                restart_identity: *restart_identity,
                cascade: *cascade,
                disable_foreign_keys: *disable_foreign_keys,
            };
            return table_actions::truncate(d, table, options, definition);
        }
        SchemaChange::EmptyTable { table } => return table_actions::empty(d, table),
        SchemaChange::DuplicateTable { table, new_name, with_data } => {
            return table_actions::duplicate(d, table, new_name, *with_data, structure, definition);
        }
        _ => {}
    }
    if flavor.is_document() {
        return Err(DbError::unsupported("Document collections have no SQL schema to change."));
    }
    if flavor == SqlFlavor::Cql
        && let Some(statements) = cql_statements(d, change)?
    {
        return Ok(statements);
    }
    if flavor == SqlFlavor::Sqlite
        && let Some(statements) = sqlite_statements(d, change)?
    {
        return Ok(statements);
    }
    if flavor == SqlFlavor::Bigquery
        && let Some(statements) = bigquery_statements(d, change)?
    {
        return Ok(statements);
    }
    let table = d.quoted_name(change.table());
    let alter = |body: String| format!("ALTER TABLE {table} {body}");
    // sp_rename takes the current name as a string and the new name unquoted.
    let sp_rename = |object: String, new_name: &str, kind: &str| {
        format!("EXEC sp_rename {}, {}{kind}", d.quote_literal(&object), d.quote_literal(new_name))
    };
    Ok(match change {
        SchemaChange::CreateTable { table: t, columns, primary_key } => {
            name(&t.name, "The table name")?;
            if columns.is_empty() {
                return Err(DbError::invalid("Add at least one column."));
            }
            let mut lines = columns.iter().map(|c| column_def(d, c)).collect::<DbResult<Vec<_>>>()?;
            if flavor == SqlFlavor::Clickhouse {
                // MergeTree stores rows sorted by the key; without one, in insertion order.
                let order = if primary_key.is_empty() {
                    "tuple()".to_owned()
                } else {
                    format!("({})", column_list(d, primary_key)?)
                };
                vec![format!(
                    "CREATE TABLE {table} (\n  {}\n) ENGINE = MergeTree ORDER BY {order}",
                    lines.join(",\n  ")
                )]
            } else {
                if !primary_key.is_empty() {
                    lines.push(format!("PRIMARY KEY ({})", column_list(d, primary_key)?));
                }
                vec![format!("CREATE TABLE {table} (\n  {}\n)", lines.join(",\n  "))]
            }
        }
        SchemaChange::DropTable { table: t, cascade } => {
            let what = match t.kind {
                NodeKind::View => "VIEW",
                NodeKind::MaterializedView if flavor != SqlFlavor::Clickhouse => "MATERIALIZED VIEW",
                _ => "TABLE",
            };
            let cascade = match flavor {
                SqlFlavor::Postgres | SqlFlavor::Duckdb | SqlFlavor::Snowflake if *cascade => " CASCADE",
                SqlFlavor::Oracle if *cascade && what == "TABLE" => " CASCADE CONSTRAINTS",
                _ => "",
            };
            vec![format!("DROP {what} {table}{cascade}")]
        }
        SchemaChange::RenameTable { table: t, new_name } => {
            name(new_name, "The new name")?;
            match flavor {
                SqlFlavor::Postgres
                | SqlFlavor::Oracle
                | SqlFlavor::Duckdb
                | SqlFlavor::Sqlite
                | SqlFlavor::Bigquery => {
                    vec![alter(format!("RENAME TO {}", d.quote_ident(new_name)))]
                }
                // An unqualified new name would move the table to the current schema.
                SqlFlavor::Snowflake => {
                    let target = ObjectRef { name: new_name.clone(), ..t.clone() };
                    vec![alter(format!("RENAME TO {}", d.quoted_name(&target)))]
                }
                SqlFlavor::Sqlserver => vec![sp_rename(table.clone(), new_name, "")],
                // Refused above.
                SqlFlavor::Mysql
                | SqlFlavor::Clickhouse
                | SqlFlavor::Mongodb
                | SqlFlavor::Partiql
                | SqlFlavor::Redis
                | SqlFlavor::Cql
                | SqlFlavor::QueryDsl
                | SqlFlavor::Cypher => {
                    let target = ObjectRef { name: new_name.clone(), ..t.clone() };
                    vec![format!("RENAME TABLE {table} TO {}", d.quoted_name(&target))]
                }
            }
        }
        SchemaChange::AddColumn { column, .. } => {
            let def = column_def(d, column)?;
            vec![alter(match flavor {
                SqlFlavor::Sqlserver => format!("ADD {def}"),
                SqlFlavor::Oracle => format!("ADD ({def})"),
                SqlFlavor::Postgres
                | SqlFlavor::Mysql
                | SqlFlavor::Clickhouse
                | SqlFlavor::Duckdb
                | SqlFlavor::Sqlite
                | SqlFlavor::Snowflake
                | SqlFlavor::Bigquery
                | SqlFlavor::Mongodb
                | SqlFlavor::Partiql
                | SqlFlavor::Redis
                | SqlFlavor::Cql
                | SqlFlavor::QueryDsl
                | SqlFlavor::Cypher => format!("ADD COLUMN {def}"),
            })]
        }
        SchemaChange::DropColumn { column, .. } => {
            name(column, "The column")?;
            vec![alter(format!("DROP COLUMN {}", d.quote_ident(column)))]
        }
        SchemaChange::RenameColumn { column, new_name, .. } => {
            name(column, "The column")?;
            name(new_name, "The new name")?;
            if flavor == SqlFlavor::Sqlserver {
                vec![sp_rename(format!("{table}.{}", d.quote_ident(column)), new_name, ", 'COLUMN'")]
            } else {
                vec![alter(format!("RENAME COLUMN {} TO {}", d.quote_ident(column), d.quote_ident(new_name)))]
            }
        }
        SchemaChange::AlterColumn { table: t, column, data_type, nullable, default } => {
            name(column, "The column")?;
            if data_type.is_none() && nullable.is_none() && default.is_none() {
                return Err(DbError::invalid("Nothing to change."));
            }
            alter_column(d, t, column, data_type.as_deref(), *nullable, default.as_ref(), structure)?
        }
        SchemaChange::CreateIndex { .. } | SchemaChange::DropIndex { .. }
            if matches!(flavor, SqlFlavor::Clickhouse | SqlFlavor::Snowflake) =>
        {
            return Err(DbError::unsupported(
                "This engine has no ordinary indexes (ClickHouse sorts by its ORDER BY key; Snowflake clusters automatically).",
            ));
        }
        SchemaChange::AddForeignKey { .. } | SchemaChange::AddUnique { .. } if flavor == SqlFlavor::Clickhouse => {
            return Err(DbError::unsupported("ClickHouse has no foreign keys or unique constraints."));
        }
        SchemaChange::CreateIndex { name: n, columns, unique, .. } => {
            name(n, "The index name")?;
            if columns.is_empty() {
                return Err(DbError::invalid("Choose at least one column."));
            }
            let cols = columns
                .iter()
                .map(|c| {
                    name(&c.name, "Column names")?;
                    Ok(format!("{}{}", d.quote_ident(&c.name), if c.descending { " DESC" } else { "" }))
                })
                .collect::<DbResult<Vec<_>>>()?;
            vec![format!(
                "CREATE {}INDEX {} ON {table} ({})",
                if *unique { "UNIQUE " } else { "" },
                d.quote_ident(n),
                cols.join(", ")
            )]
        }
        SchemaChange::DropIndex { table: t, name: n } => {
            name(n, "The index name")?;
            match flavor {
                SqlFlavor::Postgres
                | SqlFlavor::Oracle
                | SqlFlavor::Duckdb
                | SqlFlavor::Sqlite
                | SqlFlavor::Snowflake
                | SqlFlavor::Bigquery => {
                    let index = ObjectRef { name: n.clone(), ..t.clone() };
                    vec![format!("DROP INDEX {}", d.quoted_name(&index))]
                }
                SqlFlavor::Mysql if n == "PRIMARY" => vec![alter("DROP PRIMARY KEY".into())],
                SqlFlavor::Mysql
                | SqlFlavor::Sqlserver
                | SqlFlavor::Clickhouse
                | SqlFlavor::Mongodb
                | SqlFlavor::Partiql
                | SqlFlavor::Redis
                | SqlFlavor::Cql
                | SqlFlavor::QueryDsl
                | SqlFlavor::Cypher => {
                    vec![format!("DROP INDEX {} ON {table}", d.quote_ident(n))]
                }
            }
        }
        SchemaChange::AddForeignKey { name: n, columns, target, target_columns, on_delete, on_update, .. } => {
            name(n, "The constraint name")?;
            if columns.len() != target_columns.len() {
                return Err(DbError::invalid("Pair every column with a referenced column."));
            }
            if flavor == SqlFlavor::Oracle && on_update.as_deref().is_some_and(|u| !u.trim().is_empty()) {
                return Err(DbError::unsupported("Oracle foreign keys have no ON UPDATE action."));
            }
            vec![alter(format!(
                "ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}){}{}",
                d.quote_ident(n),
                column_list(d, columns)?,
                d.quoted_name(target),
                column_list(d, target_columns)?,
                action("ON DELETE", on_delete.as_deref())?,
                action("ON UPDATE", on_update.as_deref())?,
            ))]
        }
        SchemaChange::AddUnique { name: n, columns, .. } => {
            name(n, "The constraint name")?;
            vec![alter(format!("ADD CONSTRAINT {} UNIQUE ({})", d.quote_ident(n), column_list(d, columns)?))]
        }
        SchemaChange::AddCheck { name: n, expression: e, .. } => {
            name(n, "The constraint name")?;
            vec![alter(format!(
                "ADD CONSTRAINT {} CHECK ({})",
                d.quote_ident(n),
                expression(d, e, "check expression")?
            ))]
        }
        SchemaChange::DropConstraint { name: n, kind, .. } => {
            name(n, "The constraint name")?;
            let q = d.quote_ident(n);
            vec![alter(match (flavor, kind) {
                (SqlFlavor::Mysql, ConstraintKind::PrimaryKey) => "DROP PRIMARY KEY".into(),
                (SqlFlavor::Mysql, ConstraintKind::ForeignKey) => format!("DROP FOREIGN KEY {q}"),
                (SqlFlavor::Mysql, ConstraintKind::Unique) => format!("DROP INDEX {q}"),
                _ => format!("DROP CONSTRAINT {q}"),
            })]
        }
        // Built by `table_actions` above.
        SchemaChange::TruncateTable { .. } | SchemaChange::EmptyTable { .. } | SchemaChange::DuplicateTable { .. } => {
            return Err(DbError::internal("table action reached the schema change builder"));
        }
    })
}

/// `SQLite` spellings, or an error for what its `ALTER TABLE` cannot do. `None`
/// falls through to the shared SQL.
fn sqlite_statements(d: &dyn Dialect, change: &SchemaChange) -> DbResult<Option<Vec<String>>> {
    // The index name carries the schema; the table name must not.
    let index = |n: &str, unique: bool, columns: Vec<String>| {
        let target = ObjectRef { name: n.to_owned(), ..change.table().clone() };
        format!(
            "CREATE {}INDEX {} ON {} ({})",
            if unique { "UNIQUE " } else { "" },
            d.quoted_name(&target),
            d.quote_ident(&change.table().name),
            columns.join(", ")
        )
    };
    let only_rowid =
        || DbError::unsupported("In SQLite only a single INTEGER PRIMARY KEY column numbers itself (identity).");
    Ok(match change {
        SchemaChange::CreateTable { columns, primary_key, .. } => {
            for c in columns.iter().filter(|c| c.identity) {
                if !c.data_type.trim().eq_ignore_ascii_case("integer") || primary_key.as_slice() != [c.name.clone()] {
                    return Err(only_rowid());
                }
            }
            None
        }
        SchemaChange::AddColumn { column, .. } if column.identity => return Err(only_rowid()),
        SchemaChange::CreateIndex { name: n, columns, unique, .. } => {
            name(n, "The index name")?;
            if columns.is_empty() {
                return Err(DbError::invalid("Choose at least one column."));
            }
            let cols = columns
                .iter()
                .map(|c| {
                    name(&c.name, "Column names")?;
                    Ok(format!("{}{}", d.quote_ident(&c.name), if c.descending { " DESC" } else { "" }))
                })
                .collect::<DbResult<Vec<_>>>()?;
            Some(vec![index(n, *unique, cols)])
        }
        // A unique index enforces the same rule as a unique constraint.
        SchemaChange::AddUnique { name: n, columns, .. } => {
            name(n, "The constraint name")?;
            column_list(d, columns)?;
            Some(vec![index(n, true, columns.iter().map(|c| d.quote_ident(c)).collect())])
        }
        SchemaChange::AddForeignKey { .. } | SchemaChange::AddCheck { .. } | SchemaChange::DropConstraint { .. } => {
            return Err(DbError::unsupported(
                "SQLite only adds or removes foreign keys and checks when a table is created. Create a new table and copy the rows.",
            ));
        }
        _ => None,
    })
}

/// A CQL table with its partition key, clustering columns and clustering
/// order: `PRIMARY KEY ((a, b), c) WITH CLUSTERING ORDER BY (c DESC)`.
/// `statics` names the static columns.
pub fn cql_create_table(
    d: &dyn Dialect,
    table: &ObjectRef,
    columns: &[ColumnDef],
    statics: &[String],
    keys: &[KeyRole],
) -> DbResult<String> {
    name(&table.name, "The table name")?;
    if columns.is_empty() {
        return Err(DbError::invalid("Add at least one column."));
    }
    let of = |role: KeyRoleKind| keys.iter().filter(move |k| k.role == role);
    let partition: Vec<String> = of(KeyRoleKind::Partition).map(|k| k.column.clone()).collect();
    if partition.is_empty() {
        return Err(DbError::invalid(
            "Cassandra tables need a primary key: its first column is the partition key, the rest are clustering columns.",
        ));
    }
    let mut lines = Vec::with_capacity(columns.len() + 1);
    for c in columns {
        let line = column_def(d, c)?;
        lines.push(if statics.contains(&c.name) { format!("{line} STATIC") } else { line });
    }
    let mut key = column_list(d, &partition)?;
    if partition.len() > 1 {
        key = format!("({key})");
    }
    let clustering: Vec<String> = of(KeyRoleKind::Clustering).map(|k| k.column.clone()).collect();
    if !clustering.is_empty() {
        let _ = write!(key, ", {}", column_list(d, &clustering)?);
    }
    lines.push(format!("PRIMARY KEY ({key})"));
    let mut sql = format!("CREATE TABLE {} (\n  {}\n)", d.quoted_name(table), lines.join(",\n  "));
    if of(KeyRoleKind::Clustering).any(|k| k.descending) {
        let order: Vec<String> = of(KeyRoleKind::Clustering)
            .map(|k| format!("{} {}", d.quote_ident(&k.column), if k.descending { "DESC" } else { "ASC" }))
            .collect();
        let _ = write!(sql, " WITH CLUSTERING ORDER BY ({})", order.join(", "));
    }
    Ok(sql)
}

/// `BigQuery` spellings: keys are declared `NOT ENFORCED`, there are no indexes,
/// unique or check constraints, and a new name keeps the dataset. `None` falls
/// through to the shared SQL.
fn bigquery_statements(d: &dyn Dialect, change: &SchemaChange) -> DbResult<Option<Vec<String>>> {
    let table = d.quoted_name(change.table());
    Ok(match change {
        SchemaChange::CreateTable { table: t, columns, primary_key } => {
            name(&t.name, "The table name")?;
            if columns.is_empty() {
                return Err(DbError::invalid("Add at least one column."));
            }
            let mut lines = columns.iter().map(|c| column_def(d, c)).collect::<DbResult<Vec<_>>>()?;
            if !primary_key.is_empty() {
                lines.push(format!("PRIMARY KEY ({}) NOT ENFORCED", column_list(d, primary_key)?));
            }
            Some(vec![format!("CREATE TABLE {table} (\n  {}\n)", lines.join(",\n  "))])
        }
        SchemaChange::CreateIndex { .. } | SchemaChange::DropIndex { .. } => {
            return Err(DbError::unsupported(
                "BigQuery has no ordinary indexes; partitioning and clustering are set when a table is created.",
            ));
        }
        SchemaChange::AddUnique { .. } | SchemaChange::AddCheck { .. } => {
            return Err(DbError::unsupported("BigQuery has no unique or check constraints."));
        }
        SchemaChange::AddForeignKey { name: n, columns, target, target_columns, on_delete, on_update, .. } => {
            name(n, "The constraint name")?;
            if columns.len() != target_columns.len() {
                return Err(DbError::invalid("Pair every column with a referenced column."));
            }
            if [on_delete, on_update].iter().any(|a| a.as_deref().is_some_and(|a| !a.trim().is_empty())) {
                return Err(DbError::unsupported("BigQuery foreign keys are not enforced, so they have no actions."));
            }
            Some(vec![format!(
                "ALTER TABLE {table} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) NOT ENFORCED",
                d.quote_ident(n),
                column_list(d, columns)?,
                d.quoted_name(target),
                column_list(d, target_columns)?,
            )])
        }
        SchemaChange::DropConstraint { kind: ConstraintKind::PrimaryKey, .. } => {
            Some(vec![format!("ALTER TABLE {table} DROP PRIMARY KEY")])
        }
        _ => None,
    })
}

/// CQL spellings, or an error for what Cassandra cannot do. `None` falls through
/// to the shared SQL (create and drop table).
fn cql_statements(d: &dyn Dialect, change: &SchemaChange) -> DbResult<Option<Vec<String>>> {
    let table = d.quoted_name(change.table());
    let alter = |body: String| Some(vec![format!("ALTER TABLE {table} {body}")]);
    Ok(match change {
        // The first key column is the partition key, the rest are clustering columns.
        SchemaChange::CreateTable { table: t, columns, primary_key } => {
            let keys: Vec<KeyRole> = primary_key
                .iter()
                .enumerate()
                .map(|(i, c)| KeyRole {
                    column: c.clone(),
                    role: if i == 0 { KeyRoleKind::Partition } else { KeyRoleKind::Clustering },
                    descending: false,
                })
                .collect();
            Some(vec![cql_create_table(d, t, columns, &[], &keys)?])
        }
        SchemaChange::DropTable { .. }
        | SchemaChange::TruncateTable { .. }
        | SchemaChange::EmptyTable { .. }
        | SchemaChange::DuplicateTable { .. } => None,
        SchemaChange::AddColumn { column, .. } => alter(format!("ADD {}", column_def(d, column)?)),
        SchemaChange::DropColumn { column, .. } => {
            name(column, "The column")?;
            alter(format!("DROP {}", d.quote_ident(column)))
        }
        // Only primary key columns can be renamed; the server says so for others.
        SchemaChange::RenameColumn { column, new_name, .. } => {
            name(column, "The column")?;
            name(new_name, "The new name")?;
            alter(format!("RENAME {} TO {}", d.quote_ident(column), d.quote_ident(new_name)))
        }
        SchemaChange::CreateIndex { name: n, columns, unique, .. } => {
            name(n, "The index name")?;
            let [column] = columns.as_slice() else {
                return Err(DbError::unsupported("A Cassandra secondary index covers exactly one column."));
            };
            if *unique || column.descending {
                return Err(DbError::unsupported("Cassandra indexes are neither unique nor ordered."));
            }
            name(&column.name, "Column names")?;
            Some(vec![format!("CREATE INDEX {} ON {table} ({})", d.quote_ident(n), d.quote_ident(&column.name))])
        }
        SchemaChange::DropIndex { table: t, name: n } => {
            name(n, "The index name")?;
            let index = ObjectRef { name: n.clone(), ..t.clone() };
            Some(vec![format!("DROP INDEX {}", d.quoted_name(&index))])
        }
        SchemaChange::RenameTable { .. } => {
            return Err(DbError::unsupported("Cassandra cannot rename tables."));
        }
        SchemaChange::AlterColumn { .. } => {
            return Err(DbError::unsupported("Cassandra cannot change a column's type, nullability or default."));
        }
        SchemaChange::AddForeignKey { .. }
        | SchemaChange::AddUnique { .. }
        | SchemaChange::AddCheck { .. }
        | SchemaChange::DropConstraint { .. } => {
            return Err(DbError::unsupported("Cassandra has no foreign keys, unique or check constraints."));
        }
    })
}

/// SQL for review: statements separated by `;` and blank lines.
pub fn preview(statements: &[String]) -> String {
    statements.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::dialect::{MysqlDialect, PostgresDialect};

    fn pg_table() -> ObjectRef {
        ObjectRef::new(NodeKind::Table, "app", Some("public".into()), "orders")
    }

    fn my_table() -> ObjectRef {
        ObjectRef::new(NodeKind::Table, "shop", None, "orders")
    }

    fn col(name: &str, ty: &str) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, default: None, identity: false }
    }

    #[test]
    fn create_table_per_dialect() {
        let change = SchemaChange::CreateTable {
            table: pg_table(),
            columns: vec![
                ColumnDef { identity: true, nullable: false, ..col("id", "bigint") },
                ColumnDef { default: Some("'pending'".into()), nullable: false, ..col("status", "text") },
            ],
            primary_key: vec!["id".into()],
        };
        assert_eq!(
            statements(&PostgresDialect, &change, None).unwrap(),
            [r#"CREATE TABLE "public"."orders" (
  "id" bigint GENERATED BY DEFAULT AS IDENTITY NOT NULL,
  "status" text DEFAULT 'pending' NOT NULL,
  PRIMARY KEY ("id")
)"#]
        );
        let my = SchemaChange::CreateTable {
            table: my_table(),
            columns: vec![ColumnDef { identity: true, ..col("id", "int unsigned") }],
            primary_key: vec!["id".into()],
        };
        assert!(statements(&MysqlDialect, &my, None).unwrap()[0].contains("`id` int unsigned AUTO_INCREMENT"));
    }

    #[test]
    fn sqlite_changes() {
        use crate::sql::dialect::SqliteDialect;
        let t = ObjectRef::new(NodeKind::Table, "main", None, "orders");
        let index = SchemaChange::CreateIndex {
            table: t.clone(),
            name: "by_status".into(),
            columns: vec![IndexColumn { name: "status".into(), descending: true }],
            unique: false,
        };
        assert_eq!(
            statements(&SqliteDialect, &index, None).unwrap(),
            [r#"CREATE INDEX "main"."by_status" ON "orders" ("status" DESC)"#]
        );
        let unique =
            SchemaChange::AddUnique { table: t.clone(), name: "u".into(), columns: vec!["a".into(), "b".into()] };
        assert_eq!(
            statements(&SqliteDialect, &unique, None).unwrap(),
            [r#"CREATE UNIQUE INDEX "main"."u" ON "orders" ("a", "b")"#]
        );
        let drop = SchemaChange::DropIndex { table: t.clone(), name: "u".into() };
        assert_eq!(statements(&SqliteDialect, &drop, None).unwrap(), [r#"DROP INDEX "main"."u""#]);
        let rename = SchemaChange::RenameTable { table: t.clone(), new_name: "sales".into() };
        assert_eq!(
            statements(&SqliteDialect, &rename, None).unwrap(),
            [r#"ALTER TABLE "main"."orders" RENAME TO "sales""#]
        );
        let alter = SchemaChange::AlterColumn {
            table: t.clone(),
            column: "status".into(),
            data_type: Some("text".into()),
            nullable: None,
            default: None,
        };
        assert!(statements(&SqliteDialect, &alter, None).is_err());
        let check = SchemaChange::AddCheck { table: t, name: "c".into(), expression: "total > 0".into() };
        assert!(statements(&SqliteDialect, &check, None).is_err());
    }

    #[test]
    fn sqlserver_and_oracle_changes() {
        use crate::sql::dialect::{OracleDialect, SqlServerDialect};
        let ms_table = ObjectRef::new(NodeKind::Table, "shop", Some("dbo".into()), "orders");
        let add = SchemaChange::AddColumn {
            table: ms_table.clone(),
            column: ColumnDef { identity: true, nullable: false, ..col("id", "int") },
        };
        assert_eq!(
            statements(&SqlServerDialect, &add, None).unwrap(),
            ["ALTER TABLE [dbo].[orders] ADD [id] int IDENTITY(1,1) NOT NULL"]
        );
        let rename =
            SchemaChange::RenameColumn { table: ms_table.clone(), column: "a".into(), new_name: "it's".into() };
        assert_eq!(
            statements(&SqlServerDialect, &rename, None).unwrap(),
            ["EXEC sp_rename N'[dbo].[orders].[a]', N'it''s', 'COLUMN'"]
        );
        let default = SchemaChange::AlterColumn {
            table: ms_table,
            column: "status".into(),
            data_type: None,
            nullable: None,
            default: Some(DefaultChange::Set { expression: "'new'".into() }),
        };
        let sql = statements(&SqlServerDialect, &default, None).unwrap();
        assert_eq!(sql.len(), 2);
        assert!(sql[0].contains("sys.default_constraints") && sql[0].contains("QUOTENAME(@dq_default)"));
        assert_eq!(sql[1], "ALTER TABLE [dbo].[orders] ADD DEFAULT 'new' FOR [status]");

        let ora_table = ObjectRef::new(NodeKind::Table, "FREEPDB1", Some("APP".into()), "ORDERS");
        let add = SchemaChange::AddColumn {
            table: ora_table.clone(),
            column: ColumnDef { default: Some("0".into()), nullable: false, ..col("QTY", "NUMBER(10)") },
        };
        assert_eq!(
            statements(&OracleDialect, &add, None).unwrap(),
            ["ALTER TABLE \"APP\".\"ORDERS\" ADD (\"QTY\" NUMBER(10) DEFAULT 0 NOT NULL)"]
        );
        let modify = SchemaChange::AlterColumn {
            table: ora_table.clone(),
            column: "QTY".into(),
            data_type: Some("NUMBER(12)".into()),
            nullable: Some(true),
            default: Some(DefaultChange::Drop),
        };
        assert_eq!(
            statements(&OracleDialect, &modify, None).unwrap(),
            ["ALTER TABLE \"APP\".\"ORDERS\" MODIFY (\"QTY\" NUMBER(12) DEFAULT NULL NULL)"]
        );
        let drop = SchemaChange::DropTable { table: ora_table, cascade: true };
        assert_eq!(
            statements(&OracleDialect, &drop, None).unwrap(),
            ["DROP TABLE \"APP\".\"ORDERS\" CASCADE CONSTRAINTS"]
        );
    }

    #[test]
    fn clickhouse_duckdb_and_snowflake_changes() {
        use crate::sql::dialect::{ClickhouseDialect, DuckdbDialect, SnowflakeDialect};
        let ch = ObjectRef::new(NodeKind::Table, "logs", None, "events");
        let create = SchemaChange::CreateTable {
            table: ch.clone(),
            columns: vec![ColumnDef { nullable: false, ..col("id", "UInt64") }, col("note", "String")],
            primary_key: vec!["id".into()],
        };
        assert_eq!(
            statements(&ClickhouseDialect, &create, None).unwrap(),
            [
                "CREATE TABLE `logs`.`events` (\n  `id` UInt64,\n  `note` Nullable(String)\n) ENGINE = MergeTree ORDER BY (`id`)"
            ]
        );
        let index = SchemaChange::CreateIndex { table: ch, name: "i".into(), columns: vec![], unique: false };
        assert!(statements(&ClickhouseDialect, &index, None).is_err());
        let duck = ObjectRef::new(NodeKind::Table, "sales", Some("main".into()), "orders");
        let identity = SchemaChange::AddColumn {
            table: duck.clone(),
            column: ColumnDef { identity: true, ..col("id", "BIGINT") },
        };
        assert_eq!(statements(&DuckdbDialect, &identity, None).unwrap_err().kind, crate::ErrorKind::Unsupported);
        let alter = SchemaChange::AlterColumn {
            table: duck,
            column: "total".into(),
            data_type: Some("DECIMAL(14,2)".into()),
            nullable: Some(false),
            default: None,
        };
        assert_eq!(statements(&DuckdbDialect, &alter, None).unwrap().len(), 2);
        let sf = ObjectRef::new(NodeKind::Table, "DB", Some("PUBLIC".into()), "ORDERS");
        let rename = SchemaChange::RenameTable { table: sf, new_name: "BILLS".into() };
        assert_eq!(
            statements(&SnowflakeDialect, &rename, None).unwrap(),
            ["ALTER TABLE \"DB\".\"PUBLIC\".\"ORDERS\" RENAME TO \"DB\".\"PUBLIC\".\"BILLS\""]
        );
    }

    #[test]
    fn bigquery_changes() {
        use crate::sql::dialect::BigQueryDialect;
        let bq = BigQueryDialect;
        let t = ObjectRef::new(NodeKind::Table, "my-project", Some("sales".into()), "orders");
        let create = SchemaChange::CreateTable {
            table: t.clone(),
            columns: vec![ColumnDef { nullable: false, ..col("id", "INT64") }, col("tags", "ARRAY<STRUCT<a INT64>>")],
            primary_key: vec!["id".into()],
        };
        assert_eq!(
            statements(&bq, &create, None).unwrap(),
            [
                "CREATE TABLE `my-project`.`sales`.`orders` (\n  `id` INT64 NOT NULL,\n  `tags` ARRAY<STRUCT<a INT64>>,\n  PRIMARY KEY (`id`) NOT ENFORCED\n)"
            ]
        );
        let rename = SchemaChange::RenameTable { table: t.clone(), new_name: "bills".into() };
        assert_eq!(
            statements(&bq, &rename, None).unwrap(),
            ["ALTER TABLE `my-project`.`sales`.`orders` RENAME TO `bills`"]
        );
        let alter = |nullable, default| SchemaChange::AlterColumn {
            table: t.clone(),
            column: "total".into(),
            data_type: Some("NUMERIC(14, 2)".into()),
            nullable,
            default,
        };
        assert_eq!(
            statements(&bq, &alter(Some(true), Some(DefaultChange::Drop)), None).unwrap(),
            [
                "ALTER TABLE `my-project`.`sales`.`orders` ALTER COLUMN `total` SET DATA TYPE NUMERIC(14, 2)",
                "ALTER TABLE `my-project`.`sales`.`orders` ALTER COLUMN `total` DROP NOT NULL",
                "ALTER TABLE `my-project`.`sales`.`orders` ALTER COLUMN `total` DROP DEFAULT"
            ]
        );
        assert_eq!(statements(&bq, &alter(Some(false), None), None).unwrap_err().kind, crate::ErrorKind::Unsupported);
        let index = SchemaChange::CreateIndex { table: t.clone(), name: "i".into(), columns: vec![], unique: false };
        assert_eq!(statements(&bq, &index, None).unwrap_err().kind, crate::ErrorKind::Unsupported);
        let identity =
            SchemaChange::AddColumn { table: t.clone(), column: ColumnDef { identity: true, ..col("n", "INT64") } };
        assert!(statements(&bq, &identity, None).is_err());
        assert_eq!(
            statements(&bq, &SchemaChange::EmptyTable { table: t.clone() }, None).unwrap(),
            ["DELETE FROM `my-project`.`sales`.`orders` WHERE TRUE"]
        );
        let copy = SchemaChange::DuplicateTable { table: t, new_name: "orders_copy".into(), with_data: true };
        assert_eq!(
            statements(&bq, &copy, None).unwrap(),
            ["CREATE TABLE `my-project`.`sales`.`orders_copy` COPY `my-project`.`sales`.`orders`"]
        );
    }

    #[test]
    fn rejects_injection_in_fragments() {
        let bad_type = SchemaChange::AddColumn { table: pg_table(), column: col("x", "int; DROP TABLE t") };
        assert!(statements(&PostgresDialect, &bad_type, None).is_err());
        let bad_default = SchemaChange::AddColumn {
            table: pg_table(),
            column: ColumnDef { default: Some("1); DROP TABLE t; --".into()), ..col("x", "int") },
        };
        assert!(statements(&PostgresDialect, &bad_default, None).is_err());
        let bad_action = SchemaChange::AddForeignKey {
            table: pg_table(),
            name: "fk".into(),
            columns: vec!["a".into()],
            target: pg_table(),
            target_columns: vec!["id".into()],
            on_delete: Some("CASCADE; DROP".into()),
            on_update: None,
        };
        assert!(statements(&PostgresDialect, &bad_action, None).is_err());
        assert!(validate_type(&MysqlDialect, "enum('a','b')").is_ok());
        assert!(validate_type(&PostgresDialect, "numeric(12,2)[]").is_ok());
        assert!(validate_type(&PostgresDialect, "timestamp with time zone").is_ok());
        assert!(validate_type(&PostgresDialect, "text 'x'").is_err());
    }

    #[test]
    fn alter_column_postgres_and_mysql() {
        let change = SchemaChange::AlterColumn {
            table: pg_table(),
            column: "total".into(),
            data_type: Some("numeric(14,2)".into()),
            nullable: Some(false),
            default: Some(DefaultChange::Drop),
        };
        assert_eq!(
            statements(&PostgresDialect, &change, None).unwrap()[0],
            "ALTER TABLE \"public\".\"orders\" ALTER COLUMN \"total\" TYPE numeric(14,2) USING \"total\"::numeric(14,2),\n  ALTER COLUMN \"total\" SET NOT NULL,\n  ALTER COLUMN \"total\" DROP DEFAULT"
        );
        let structure = TableStructure {
            object: my_table(),
            columns: vec![ColumnInfo {
                name: "status".into(),
                data_type: "varchar(20)".into(),
                nullable: false,
                default: Some("'pending'".into()),
                primary_key: false,
                generated: false,
                identity: false,
                editable: true,
                numeric: false,
                comment: Some("it's".into()),
                read_as_text: false,
            }],
            constraints: vec![],
            indexes: vec![],
            row_identity: vec![],
            full_row_key: vec![],
            comment: None,
            row_estimate: None,
            foreign_keys: vec![],
            size: None,
        };
        let my = SchemaChange::AlterColumn {
            table: my_table(),
            column: "status".into(),
            data_type: Some("varchar(40)".into()),
            nullable: None,
            default: None,
        };
        assert!(my.needs_structure(SqlFlavor::Mysql));
        assert_eq!(
            statements(&MysqlDialect, &my, Some(&structure)).unwrap()[0],
            "ALTER TABLE `shop`.`orders` MODIFY COLUMN `status` varchar(40) NOT NULL DEFAULT 'pending' COMMENT 'it''s'"
        );
        let only_default = SchemaChange::AlterColumn {
            table: my_table(),
            column: "status".into(),
            data_type: None,
            nullable: None,
            default: Some(DefaultChange::Set { expression: "'new'".into() }),
        };
        assert_eq!(
            statements(&MysqlDialect, &only_default, None).unwrap()[0],
            "ALTER TABLE `shop`.`orders` ALTER COLUMN `status` SET DEFAULT 'new'"
        );
    }

    #[test]
    fn indexes_and_constraints() {
        let idx = SchemaChange::CreateIndex {
            table: pg_table(),
            name: "orders_total_idx".into(),
            columns: vec![IndexColumn { name: "total".into(), descending: true }],
            unique: false,
        };
        assert_eq!(
            statements(&PostgresDialect, &idx, None).unwrap()[0],
            r#"CREATE INDEX "orders_total_idx" ON "public"."orders" ("total" DESC)"#
        );
        let drop = SchemaChange::DropIndex { table: pg_table(), name: "orders_total_idx".into() };
        assert_eq!(statements(&PostgresDialect, &drop, None).unwrap()[0], r#"DROP INDEX "public"."orders_total_idx""#);
        let drop_my = SchemaChange::DropIndex { table: my_table(), name: "orders_total_idx".into() };
        assert_eq!(
            statements(&MysqlDialect, &drop_my, None).unwrap()[0],
            "DROP INDEX `orders_total_idx` ON `shop`.`orders`"
        );
        let fk =
            SchemaChange::DropConstraint { table: my_table(), name: "fk".into(), kind: ConstraintKind::ForeignKey };
        assert_eq!(
            statements(&MysqlDialect, &fk, None).unwrap()[0],
            "ALTER TABLE `shop`.`orders` DROP FOREIGN KEY `fk`"
        );
        let rename = SchemaChange::RenameTable { table: my_table(), new_name: "sales".into() };
        assert_eq!(
            statements(&MysqlDialect, &rename, None).unwrap()[0],
            "RENAME TABLE `shop`.`orders` TO `shop`.`sales`"
        );
        assert_eq!(preview(&["SELECT 1".into(), "SELECT 2".into()]), "SELECT 1;\n\nSELECT 2;\n");
    }

    #[test]
    fn drops_views_and_table_actions() {
        let view = ObjectRef { kind: NodeKind::View, ..pg_table() };
        let drop = SchemaChange::DropTable { table: view, cascade: true };
        assert_eq!(statements(&PostgresDialect, &drop, None).unwrap(), [r#"DROP VIEW "public"."orders" CASCADE"#]);
        let mv = SchemaChange::DropTable {
            table: ObjectRef { kind: NodeKind::MaterializedView, ..pg_table() },
            cascade: false,
        };
        assert_eq!(statements(&PostgresDialect, &mv, None).unwrap(), [r#"DROP MATERIALIZED VIEW "public"."orders""#]);
        let empty = SchemaChange::EmptyTable { table: my_table() };
        assert!(empty.deletes_rows() && !empty.needs_structure(SqlFlavor::Mysql));
        assert_eq!(statements(&MysqlDialect, &empty, None).unwrap(), ["DELETE FROM `shop`.`orders`"]);
        let duplicate = SchemaChange::DuplicateTable { table: my_table(), new_name: "copy".into(), with_data: false };
        assert!(duplicate.needs_structure(SqlFlavor::Mysql) && !duplicate.needs_definition(SqlFlavor::Mysql));
        assert!(duplicate.needs_definition(SqlFlavor::Sqlite));
        let truncate = SchemaChange::TruncateTable {
            table: pg_table(),
            restart_identity: true,
            cascade: false,
            disable_foreign_keys: false,
        };
        assert!(truncate.needs_definition(SqlFlavor::Sqlite) && !truncate.needs_definition(SqlFlavor::Postgres));
        assert_eq!(
            statements(&PostgresDialect, &truncate, None).unwrap(),
            [r#"TRUNCATE TABLE "public"."orders" RESTART IDENTITY"#]
        );
    }

    #[test]
    fn cql_changes() {
        use crate::sql::dialect::CqlDialect;
        let d = &CqlDialect;
        let t = ObjectRef::new(NodeKind::Table, "telemetry", None, "readings");
        let add = SchemaChange::AddColumn { table: t.clone(), column: col("tags", "map<text, frozen<list<int>>>") };
        assert_eq!(
            statements(d, &add, None).unwrap(),
            ["ALTER TABLE \"telemetry\".\"readings\" ADD \"tags\" map<text, frozen<list<int>>>"]
        );
        let bad = SchemaChange::AddColumn { table: t.clone(), column: col("x", "map<text, int") };
        assert!(statements(d, &bad, None).is_err());
        let drop = SchemaChange::DropColumn { table: t.clone(), column: "tags".into() };
        assert_eq!(statements(d, &drop, None).unwrap(), ["ALTER TABLE \"telemetry\".\"readings\" DROP \"tags\""]);
        let no_key =
            SchemaChange::CreateTable { table: t.clone(), columns: vec![col("a", "int")], primary_key: vec![] };
        assert!(statements(d, &no_key, None).is_err());
        assert!(statements(d, &SchemaChange::RenameTable { table: t.clone(), new_name: "r2".into() }, None).is_err());
        let simple = SchemaChange::CreateTable {
            table: t.clone(),
            columns: vec![col("sensor", "uuid"), col("at", "timestamp"), col("value", "double")],
            primary_key: vec!["sensor".into(), "at".into()],
        };
        assert_eq!(
            statements(d, &simple, None).unwrap(),
            [
                "CREATE TABLE \"telemetry\".\"readings\" (\n  \"sensor\" uuid,\n  \"at\" timestamp,\n  \"value\" double,\n  PRIMARY KEY (\"sensor\", \"at\")\n)"
            ]
        );
        let key = |column: &str, role, descending| KeyRole { column: column.into(), role, descending };
        let keys = [
            key("sensor", KeyRoleKind::Partition, false),
            key("day", KeyRoleKind::Partition, false),
            key("at", KeyRoleKind::Clustering, true),
        ];
        let columns = [col("sensor", "uuid"), col("day", "date"), col("at", "timestamp"), col("unit", "text")];
        assert_eq!(
            cql_create_table(d, &t, &columns, &["unit".into()], &keys).unwrap(),
            "CREATE TABLE \"telemetry\".\"readings\" (\n  \"sensor\" uuid,\n  \"day\" date,\n  \"at\" timestamp,\n  \"unit\" text STATIC,\n  PRIMARY KEY ((\"sensor\", \"day\"), \"at\")\n) WITH CLUSTERING ORDER BY (\"at\" DESC)"
        );
    }
}
