//! Catalog queries over `information_schema` for the explorer, structure tab
//! and generated DDL. Every name travels as a parameter; nothing is interpolated
//! except identifiers quoted by [`MysqlDialect`].

use std::fmt::Write;

use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::schema::{ColumnInfo, ConstraintInfo, ConstraintKind, IndexInfo, TableStructure};
use dq_core::sql::{Dialect, MysqlDialect};
use dq_core::{DbError, DbResult};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Row};

use crate::error::map_error;
use crate::values::text;

/// Databases that belong to the server, hidden unless they are the current one.
const SYSTEM_DATABASES: &str = "('information_schema','mysql','performance_schema','sys')";

fn my(e: mysql_async::Error) -> DbError {
    map_error(&e, None)
}

fn number(row: &Row, i: usize) -> Option<i64> {
    text(row, i).and_then(|v| v.parse().ok())
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

fn kind_of(table_type: &str) -> NodeKind {
    if table_type.contains("VIEW") { NodeKind::View } else { NodeKind::Table }
}

pub async fn databases(conn: &mut Conn, current: &str) -> DbResult<Vec<Node>> {
    let sql = format!(
        "SELECT s.SCHEMA_NAME, \
                (SELECT COUNT(*) FROM information_schema.TABLES t WHERE t.TABLE_SCHEMA = s.SCHEMA_NAME) \
         FROM information_schema.SCHEMATA s \
         WHERE s.SCHEMA_NAME NOT IN {SYSTEM_DATABASES} OR s.SCHEMA_NAME = ? \
         ORDER BY s.SCHEMA_NAME"
    );
    let rows: Vec<Row> = conn.exec(sql, (current,)).await.map_err(my)?;
    Ok(rows
        .iter()
        .map(|r| {
            let name = text(r, 0).unwrap_or_default();
            let mut node = Node::container(ObjectRef {
                kind: NodeKind::Database,
                database: name.clone(),
                schema: None,
                name: name.clone(),
            });
            node.row_estimate = number(r, 1);
            node.current = name == current;
            node
        })
        .collect())
}

const RELATION_COLUMNS: &str = "TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE, TABLE_ROWS, TABLE_COMMENT";

fn relation_node(row: &Row) -> Node {
    let table_type = text(row, 2).unwrap_or_default();
    let kind = kind_of(&table_type);
    let mut node = Node::leaf(ObjectRef {
        kind,
        database: text(row, 0).unwrap_or_default(),
        schema: None,
        name: text(row, 1).unwrap_or_default(),
    });
    // Views report no rows, and "VIEW" as their comment.
    if kind == NodeKind::Table {
        node.row_estimate = number(row, 3);
        node.comment = non_empty(text(row, 4));
    }
    node
}

pub async fn relations(conn: &mut Conn, database: &str) -> DbResult<Vec<Node>> {
    let sql = format!(
        "SELECT {RELATION_COLUMNS} FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = ? ORDER BY TABLE_TYPE LIKE '%VIEW', TABLE_NAME"
    );
    let rows: Vec<Row> = conn.exec(sql, (database,)).await.map_err(my)?;
    Ok(rows.iter().map(relation_node).collect())
}

pub async fn search(conn: &mut Conn, query: &str, limit: u32) -> DbResult<Vec<Node>> {
    let pattern = format!("%{}%", query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let sql = format!(
        "SELECT {RELATION_COLUMNS} FROM information_schema.TABLES \
         WHERE LOWER(TABLE_NAME) LIKE LOWER(?) AND TABLE_SCHEMA NOT IN {SYSTEM_DATABASES} \
         ORDER BY CHAR_LENGTH(TABLE_NAME), TABLE_NAME LIMIT ?"
    );
    let rows: Vec<Row> = conn.exec(sql, (pattern, limit)).await.map_err(my)?;
    Ok(rows.iter().map(relation_node).collect())
}

struct Relation {
    kind: NodeKind,
    estimate: Option<i64>,
    comment: Option<String>,
}

async fn relation(conn: &mut Conn, object: &ObjectRef) -> DbResult<Relation> {
    let row: Row = conn
        .exec_first(
            "SELECT TABLE_TYPE, TABLE_ROWS, TABLE_COMMENT FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
            (&object.database, &object.name),
        )
        .await
        .map_err(my)?
        .ok_or_else(|| DbError::not_found(format!("{} no longer exists.", MysqlDialect.display_name(object))))?;
    let kind = kind_of(&text(&row, 0).unwrap_or_default());
    Ok(if kind == NodeKind::Table {
        Relation { kind, estimate: number(&row, 1), comment: non_empty(text(&row, 2)) }
    } else {
        Relation { kind, estimate: None, comment: None }
    })
}

pub async fn row_estimate(conn: &mut Conn, object: &ObjectRef) -> DbResult<Option<i64>> {
    Ok(relation(conn, object).await?.estimate)
}

pub async fn structure(conn: &mut Conn, object: &ObjectRef) -> DbResult<TableStructure> {
    let rel = relation(conn, object).await?;
    let columns = columns(conn, object).await?;
    let mut constraints = key_constraints(conn, object).await?;
    constraints.extend(check_constraints(conn, object).await);
    let index_columns = index_columns(conn, object).await?;
    let row_identity = if rel.kind == NodeKind::Table { row_identity(&index_columns, &columns) } else { Vec::new() };
    Ok(TableStructure {
        object: ObjectRef { kind: rel.kind, ..object.clone() },
        columns,
        constraints,
        indexes: indexes(object, &index_columns),
        row_identity,
        comment: rel.comment,
        row_estimate: rel.estimate,
    })
}

const NUMERIC_TYPES: &[&str] = &[
    "tinyint",
    "smallint",
    "mediumint",
    "int",
    "integer",
    "bigint",
    "decimal",
    "numeric",
    "float",
    "double",
    "real",
    "year",
];

/// Types the grid cannot round-trip through text.
const BINARY_TYPES: &[&str] = &[
    "binary",
    "varbinary",
    "tinyblob",
    "blob",
    "mediumblob",
    "longblob",
    "bit",
    "geometry",
    "point",
    "linestring",
    "polygon",
    "multipoint",
    "multilinestring",
    "multipolygon",
    "geometrycollection",
    "geomcollection",
    "vector",
];

async fn columns(conn: &mut Conn, object: &ObjectRef) -> DbResult<Vec<ColumnInfo>> {
    let rows: Vec<Row> = conn
        .exec(
            "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, COLUMN_KEY, EXTRA, DATA_TYPE, COLUMN_COMMENT \
             FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
            (&object.database, &object.name),
        )
        .await
        .map_err(my)?;
    Ok(rows
        .iter()
        .map(|r| {
            let extra = text(r, 5).unwrap_or_default().to_ascii_lowercase();
            let base_type = text(r, 6).unwrap_or_default().to_ascii_lowercase();
            // `DEFAULT_GENERATED` marks expression defaults, which are not generated columns.
            let always_generated =
                ["virtual generated", "stored generated", "persistent generated"].iter().any(|g| extra.contains(g));
            ColumnInfo {
                name: text(r, 0).unwrap_or_default(),
                data_type: text(r, 1).unwrap_or_default(),
                nullable: text(r, 2).as_deref() == Some("YES"),
                default: text(r, 3),
                primary_key: text(r, 4).as_deref() == Some("PRI"),
                generated: always_generated || extra.contains("auto_increment"),
                editable: !always_generated && !BINARY_TYPES.contains(&base_type.as_str()),
                numeric: NUMERIC_TYPES.contains(&base_type.as_str()),
                comment: non_empty(text(r, 7)),
                read_as_text: false,
            }
        })
        .collect())
}

/// PRIMARY KEY, UNIQUE and FOREIGN KEY constraints with their columns.
async fn key_constraints(conn: &mut Conn, object: &ObjectRef) -> DbResult<Vec<ConstraintInfo>> {
    let rows: Vec<Row> = conn
        .exec(
            "SELECT tc.CONSTRAINT_NAME, tc.CONSTRAINT_TYPE, k.COLUMN_NAME, k.REFERENCED_TABLE_SCHEMA, \
                    k.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME, rc.UPDATE_RULE, rc.DELETE_RULE \
             FROM information_schema.TABLE_CONSTRAINTS tc \
             JOIN information_schema.KEY_COLUMN_USAGE k \
               ON k.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA AND k.CONSTRAINT_NAME = tc.CONSTRAINT_NAME \
              AND k.TABLE_NAME = tc.TABLE_NAME \
             LEFT JOIN information_schema.REFERENTIAL_CONSTRAINTS rc \
               ON rc.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA AND rc.CONSTRAINT_NAME = tc.CONSTRAINT_NAME \
              AND rc.TABLE_NAME = tc.TABLE_NAME \
             WHERE tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = ? \
               AND tc.CONSTRAINT_TYPE IN ('PRIMARY KEY', 'UNIQUE', 'FOREIGN KEY') \
             ORDER BY FIELD(tc.CONSTRAINT_TYPE, 'PRIMARY KEY', 'UNIQUE', 'FOREIGN KEY'), tc.CONSTRAINT_NAME, \
                      k.ORDINAL_POSITION",
            (&object.database, &object.name),
        )
        .await
        .map_err(my)?;
    let d = MysqlDialect;
    let mut out: Vec<ConstraintInfo> = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let name = text(&rows[i], 0).unwrap_or_default();
        let group: Vec<&Row> = rows[i..].iter().take_while(|r| text(r, 0).as_deref() == Some(&name)).collect();
        i += group.len();
        let list = |col: usize| {
            group.iter().filter_map(|r| text(r, col)).map(|c| d.display_ident(&c)).collect::<Vec<_>>().join(", ")
        };
        let first = group[0];
        let (kind, definition) = match text(first, 1).as_deref() {
            Some("PRIMARY KEY") => (ConstraintKind::PrimaryKey, format!("PRIMARY KEY ({})", list(2))),
            Some("UNIQUE") => (ConstraintKind::Unique, format!("UNIQUE ({})", list(2))),
            _ => {
                let target = ObjectRef {
                    kind: NodeKind::Table,
                    database: text(first, 3).unwrap_or_default(),
                    schema: None,
                    name: text(first, 4).unwrap_or_default(),
                };
                let target = if target.database == object.database {
                    d.display_ident(&target.name)
                } else {
                    d.display_name(&target)
                };
                let mut def = format!("FOREIGN KEY ({}) REFERENCES {target} ({})", list(2), list(5));
                for (clause, col) in [("ON UPDATE", 6), ("ON DELETE", 7)] {
                    if let Some(rule) = text(first, col).filter(|r| r != "RESTRICT" && r != "NO ACTION") {
                        let _ = write!(def, " {clause} {rule}");
                    }
                }
                (ConstraintKind::ForeignKey, def)
            }
        };
        out.push(ConstraintInfo { kind, name, definition });
    }
    Ok(out)
}

/// CHECK constraints (MySQL 8.0.16+, MariaDB 10.2+). Older servers have none.
async fn check_constraints(conn: &mut Conn, object: &ObjectRef) -> Vec<ConstraintInfo> {
    let rows: Vec<Row> = conn
        .exec(
            "SELECT cc.CONSTRAINT_NAME, cc.CHECK_CLAUSE FROM information_schema.CHECK_CONSTRAINTS cc \
             JOIN information_schema.TABLE_CONSTRAINTS tc \
               ON tc.CONSTRAINT_SCHEMA = cc.CONSTRAINT_SCHEMA AND tc.CONSTRAINT_NAME = cc.CONSTRAINT_NAME \
             WHERE tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = ? AND tc.CONSTRAINT_TYPE = 'CHECK' \
             ORDER BY cc.CONSTRAINT_NAME",
            (&object.database, &object.name),
        )
        .await
        .unwrap_or_default();
    rows.iter()
        .map(|r| ConstraintInfo {
            kind: ConstraintKind::Check,
            name: text(r, 0).unwrap_or_default(),
            definition: format!("CHECK ({})", text(r, 1).unwrap_or_default()),
        })
        .collect()
}

/// One column of one index, in index order.
struct IndexColumn {
    index: String,
    unique: bool,
    /// `None` for functional key parts.
    column: Option<String>,
    prefix: Option<i64>,
    descending: bool,
    method: String,
}

async fn index_columns(conn: &mut Conn, object: &ObjectRef) -> DbResult<Vec<IndexColumn>> {
    let rows: Vec<Row> = conn
        .exec(
            "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME, SUB_PART, COLLATION, INDEX_TYPE \
             FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
             ORDER BY INDEX_NAME <> 'PRIMARY', INDEX_NAME, SEQ_IN_INDEX",
            (&object.database, &object.name),
        )
        .await
        .map_err(my)?;
    Ok(rows
        .iter()
        .map(|r| IndexColumn {
            index: text(r, 0).unwrap_or_default(),
            unique: text(r, 1).as_deref() == Some("0"),
            column: text(r, 2),
            prefix: number(r, 3),
            descending: text(r, 4).as_deref() == Some("D"),
            method: text(r, 5).unwrap_or_default(),
        })
        .collect())
}

fn group_by_index(columns: &[IndexColumn]) -> Vec<&[IndexColumn]> {
    columns.chunk_by(|a, b| a.index == b.index).collect()
}

fn indexes(object: &ObjectRef, columns: &[IndexColumn]) -> Vec<IndexInfo> {
    let d = MysqlDialect;
    group_by_index(columns)
        .into_iter()
        .map(|parts| {
            let first = &parts[0];
            let keys = parts
                .iter()
                .map(|p| {
                    let mut key = p.column.as_deref().map_or_else(|| "(expression)".to_owned(), |c| d.display_ident(c));
                    if let Some(len) = p.prefix {
                        let _ = write!(key, "({len})");
                    }
                    if p.descending {
                        key.push_str(" DESC");
                    }
                    key
                })
                .collect::<Vec<_>>()
                .join(", ");
            let primary = first.index == "PRIMARY";
            let definition = if primary {
                format!("PRIMARY KEY ({keys})")
            } else {
                let kind = match first.method.as_str() {
                    "FULLTEXT" => "FULLTEXT ",
                    "SPATIAL" => "SPATIAL ",
                    _ if first.unique => "UNIQUE ",
                    _ => "",
                };
                // BTREE is the default; FULLTEXT and SPATIAL are named in `kind`.
                let using = if first.method == "HASH" { " USING HASH" } else { "" };
                format!(
                    "CREATE {kind}INDEX {} ON {} ({keys}){using}",
                    d.display_ident(&first.index),
                    d.display_name(object)
                )
            };
            IndexInfo { name: first.index.clone(), definition, unique: first.unique, primary }
        })
        .collect()
}

/// Primary key columns, else the columns of the first unique index whose
/// columns are all NOT NULL and indexed in full.
fn row_identity(index_columns: &[IndexColumn], columns: &[ColumnInfo]) -> Vec<String> {
    let not_null = |name: &str| columns.iter().any(|c| c.name == name && !c.nullable);
    group_by_index(index_columns)
        .into_iter()
        .filter(|parts| parts[0].unique)
        .find(|parts| parts.iter().all(|p| p.prefix.is_none() && p.column.as_deref().is_some_and(not_null)))
        .map(|parts| parts.iter().filter_map(|p| p.column.clone()).collect())
        .unwrap_or_default()
}

/// DDL as the server reports it (`SHOW CREATE TABLE` also covers views).
pub async fn ddl(conn: &mut Conn, object: &ObjectRef) -> DbResult<String> {
    relation(conn, object).await?;
    let row: Row = conn
        .query_first(format!("SHOW CREATE TABLE {}", MysqlDialect.quoted_name(object)))
        .await
        .map_err(my)?
        .ok_or_else(|| DbError::not_found(format!("{} no longer exists.", MysqlDialect.display_name(object))))?;
    Ok(format!("{};\n", text(&row, 1).unwrap_or_default().trim_end()))
}
