//! Schema changes from the structure forms. Each change becomes reviewed SQL:
//! identifiers are quoted by the dialect, and user-typed fragments (types,
//! defaults, check expressions) must be single expressions without `;`.

use std::fmt::Write;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use super::builder::validate_filter;
use super::dialect::{Dialect, SqlFlavor};
use super::lexer::{TokenKind, tokenize};
use crate::error::{DbError, DbResult};
use crate::node::ObjectRef;
use crate::schema::{ColumnInfo, ConstraintKind, TableStructure};

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
            | Self::DropConstraint { table, .. } => table,
        }
    }

    /// The change needs the table's current structure (MySQL `MODIFY COLUMN`,
    /// SQL Server `ALTER COLUMN`, which restate the type).
    pub fn needs_structure(&self, flavor: SqlFlavor) -> bool {
        matches!(flavor, SqlFlavor::Mysql | SqlFlavor::Sqlserver)
            && matches!(self, Self::AlterColumn { data_type, nullable, .. } if data_type.is_some() || nullable.is_some())
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
            TokenKind::Punct => matches!(token.text(t), "," | "." | "[" | "]" | "[]"),
            TokenKind::String => strings_ok,
            _ => false,
        };
        if !ok {
            return Err(DbError::invalid(format!("`{t}` is not a valid column type.")));
        }
    }
    if depth != 0 {
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

fn column_def(d: &dyn Dialect, c: &ColumnDef) -> DbResult<String> {
    name(&c.name, "Column names")?;
    let mut out = format!("{} {}", d.quote_ident(&c.name), validate_type(d, &c.data_type)?);
    if c.identity {
        out.push_str(d.identity_clause());
    }
    // Oracle needs DEFAULT before constraints; every engine accepts that order.
    if let Some(default) = c.default.as_deref().filter(|d| !d.trim().is_empty()) {
        let _ = write!(out, " DEFAULT {}", expression(d, default, "default")?);
    }
    if !c.nullable {
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

/// SQL Server statement that drops the default constraint of `column`, if it has one.
fn sqlserver_drop_default(d: &dyn Dialect, table: &ObjectRef, column: &str) -> String {
    let table_literal = d.quote_literal(&d.quoted_name(table));
    let column_literal = d.quote_literal(column);
    format!(
        "DECLARE @dq_default sysname = (SELECT dc.name FROM sys.default_constraints dc \
         JOIN sys.columns c ON c.object_id = dc.parent_object_id AND c.column_id = dc.parent_column_id \
         WHERE dc.parent_object_id = OBJECT_ID({table_literal}) AND c.name = {column_literal});\n\
         IF @dq_default IS NOT NULL EXEC(N'ALTER TABLE {} DROP CONSTRAINT ' + QUOTENAME(@dq_default))",
        d.quoted_name(table).replace('\'', "''")
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
    let flavor = d.flavor();
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
            if !primary_key.is_empty() {
                lines.push(format!("PRIMARY KEY ({})", column_list(d, primary_key)?));
            }
            vec![format!("CREATE TABLE {table} (\n  {}\n)", lines.join(",\n  "))]
        }
        SchemaChange::DropTable { cascade, .. } => {
            let cascade = match flavor {
                SqlFlavor::Postgres if *cascade => " CASCADE",
                SqlFlavor::Oracle if *cascade => " CASCADE CONSTRAINTS",
                _ => "",
            };
            vec![format!("DROP TABLE {table}{cascade}")]
        }
        SchemaChange::RenameTable { table: t, new_name } => {
            name(new_name, "The new name")?;
            match flavor {
                SqlFlavor::Postgres | SqlFlavor::Oracle => vec![alter(format!("RENAME TO {}", d.quote_ident(new_name)))],
                SqlFlavor::Sqlserver => vec![sp_rename(table.clone(), new_name, "")],
                SqlFlavor::Mysql => {
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
                SqlFlavor::Postgres | SqlFlavor::Mysql => format!("ADD COLUMN {def}"),
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
                SqlFlavor::Postgres | SqlFlavor::Oracle => {
                    let index = ObjectRef { name: n.clone(), ..t.clone() };
                    vec![format!("DROP INDEX {}", d.quoted_name(&index))]
                }
                SqlFlavor::Mysql if n == "PRIMARY" => vec![alter("DROP PRIMARY KEY".into())],
                SqlFlavor::Mysql | SqlFlavor::Sqlserver => vec![format!("DROP INDEX {} ON {table}", d.quote_ident(n))],
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
    })
}

/// SQL for review: statements separated by `;` and blank lines.
pub fn preview(statements: &[String]) -> String {
    statements.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeKind;
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
        let rename = SchemaChange::RenameColumn { table: ms_table.clone(), column: "a".into(), new_name: "it's".into() };
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
        assert_eq!(statements(&OracleDialect, &drop, None).unwrap(), ["DROP TABLE \"APP\".\"ORDERS\" CASCADE CONSTRAINTS"]);
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
}
