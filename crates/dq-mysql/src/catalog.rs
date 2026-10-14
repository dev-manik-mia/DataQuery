//! Catalog queries over `information_schema` for the explorer, structure tab
//! and generated DDL. Every name travels as a parameter; nothing is interpolated
//! except identifiers quoted by [`MysqlDialect`].

use std::fmt::Write;

use dq_core::node::{GROUP_FUNCTIONS, GROUP_PROCEDURES, GROUP_SEQUENCES, Node, NodeKind, ObjectRef};
use dq_core::schema::{
    ColumnInfo, ConstraintInfo, ConstraintKind, Dependencies, Dependency, ForeignKey, IndexInfo, ModelColumn,
    ModelTable, SchemaModel, TableSize, TableStructure,
};
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
    if table_type.contains("VIEW") {
        NodeKind::View
    } else if table_type == "SEQUENCE" {
        NodeKind::Sequence
    } else {
        NodeKind::Table
    }
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
            let mut node = Node::container(ObjectRef::new(NodeKind::Database, name.clone(), None, name.clone()));
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
    let object = ObjectRef::new(kind, text(row, 0).unwrap_or_default(), None, text(row, 1).unwrap_or_default());
    // Tables expand to their indexes and triggers.
    let mut node = if kind == NodeKind::Table { Node::container(object) } else { Node::leaf(object) };
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
         WHERE TABLE_SCHEMA = ? AND TABLE_TYPE <> 'SEQUENCE' ORDER BY TABLE_TYPE LIKE '%VIEW', TABLE_NAME"
    );
    let rows: Vec<Row> = conn.exec(sql, (database,)).await.map_err(my)?;
    let mut nodes: Vec<Node> = rows.iter().map(relation_node).collect();
    let counts: Option<Row> = conn
        .exec_first(
            "SELECT (SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'SEQUENCE'), \
                    (SELECT COUNT(*) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = ? AND ROUTINE_TYPE = 'FUNCTION'), \
                    (SELECT COUNT(*) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = ? AND ROUTINE_TYPE = 'PROCEDURE')",
            (database, database, database),
        )
        .await
        .map_err(my)?;
    for (i, group) in [GROUP_SEQUENCES, GROUP_FUNCTIONS, GROUP_PROCEDURES].into_iter().enumerate() {
        let count = counts.as_ref().and_then(|r| number(r, i)).unwrap_or(0);
        if count > 0 {
            let mut node = Node::container(ObjectRef::new(NodeKind::Group, database, None, group));
            node.row_estimate = Some(count);
            nodes.push(node);
        }
    }
    Ok(nodes)
}

/// Indexes and triggers of a table.
pub async fn table_children(conn: &mut Conn, table: &ObjectRef) -> DbResult<Vec<Node>> {
    let child = |kind, name: String| {
        Node::leaf(ObjectRef::new(kind, table.database.clone(), None, name).with_parent(&table.name))
    };
    let indexes: Vec<Row> = conn
        .exec(
            "SELECT DISTINCT INDEX_NAME FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
             ORDER BY INDEX_NAME <> 'PRIMARY', INDEX_NAME",
            (&table.database, &table.name),
        )
        .await
        .map_err(my)?;
    let triggers: Vec<Row> = conn
        .exec(
            "SELECT TRIGGER_NAME FROM information_schema.TRIGGERS WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ? \
             ORDER BY ACTION_ORDER, TRIGGER_NAME",
            (&table.database, &table.name),
        )
        .await
        .map_err(my)?;
    Ok(indexes
        .iter()
        .map(|r| child(NodeKind::Index, text(r, 0).unwrap_or_default()))
        .chain(triggers.iter().map(|r| child(NodeKind::Trigger, text(r, 0).unwrap_or_default())))
        .collect())
}

/// Sequences (MariaDB), functions or procedures of a database.
pub async fn group_children(conn: &mut Conn, group: &ObjectRef) -> DbResult<Vec<Node>> {
    let (sql, kind) = match group.name.as_str() {
        GROUP_SEQUENCES => (
            "SELECT TABLE_NAME, TABLE_COMMENT FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'SEQUENCE' ORDER BY TABLE_NAME",
            NodeKind::Sequence,
        ),
        GROUP_FUNCTIONS => (
            "SELECT ROUTINE_NAME, ROUTINE_COMMENT FROM information_schema.ROUTINES \
             WHERE ROUTINE_SCHEMA = ? AND ROUTINE_TYPE = 'FUNCTION' ORDER BY ROUTINE_NAME",
            NodeKind::Function,
        ),
        GROUP_PROCEDURES => (
            "SELECT ROUTINE_NAME, ROUTINE_COMMENT FROM information_schema.ROUTINES \
             WHERE ROUTINE_SCHEMA = ? AND ROUTINE_TYPE = 'PROCEDURE' ORDER BY ROUTINE_NAME",
            NodeKind::Procedure,
        ),
        other => return Err(DbError::invalid(format!("Unknown group `{other}`."))),
    };
    let rows: Vec<Row> = conn.exec(sql, (&group.database,)).await.map_err(my)?;
    Ok(rows
        .iter()
        .map(|r| {
            let mut node =
                Node::leaf(ObjectRef::new(kind, group.database.clone(), None, text(r, 0).unwrap_or_default()));
            node.comment = non_empty(text(r, 1));
            node
        })
        .collect())
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
    size: Option<TableSize>,
}

async fn relation(conn: &mut Conn, object: &ObjectRef) -> DbResult<Relation> {
    let row: Row = conn
        .exec_first(
            "SELECT TABLE_TYPE, TABLE_ROWS, TABLE_COMMENT, DATA_LENGTH, INDEX_LENGTH FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
            (&object.database, &object.name),
        )
        .await
        .map_err(my)?
        .ok_or_else(|| DbError::not_found(format!("{} no longer exists.", MysqlDialect.display_name(object))))?;
    let kind = kind_of(&text(&row, 0).unwrap_or_default());
    Ok(if kind == NodeKind::Table {
        let (data, index) = (number(&row, 3).unwrap_or(0), number(&row, 4).unwrap_or(0));
        Relation {
            kind,
            estimate: number(&row, 1),
            comment: non_empty(text(&row, 2)),
            size: Some(TableSize { total_bytes: data + index, table_bytes: data, index_bytes: index }),
        }
    } else {
        Relation { kind, estimate: None, comment: None, size: None }
    })
}

pub async fn row_estimate(conn: &mut Conn, object: &ObjectRef) -> DbResult<Option<i64>> {
    Ok(relation(conn, object).await?.estimate)
}

pub async fn structure(conn: &mut Conn, object: &ObjectRef, mariadb: bool) -> DbResult<TableStructure> {
    let rel = relation(conn, object).await?;
    let columns = columns(conn, object, mariadb).await?;
    let mut constraints = key_constraints(conn, object).await?;
    constraints.extend(check_constraints(conn, object).await);
    let index_columns = index_columns(conn, object).await?;
    let row_identity = if rel.kind == NodeKind::Table { row_identity(&index_columns, &columns) } else { Vec::new() };
    let mut indexes = indexes(object, &index_columns);
    let sizes = index_sizes(conn, object).await;
    for index in &mut indexes {
        index.size_bytes = sizes.iter().find(|(n, _)| *n == index.name).map(|(_, b)| *b);
    }
    let foreign_keys =
        foreign_keys(conn, "k.TABLE_SCHEMA = ? AND k.TABLE_NAME = ?", (&object.database, &object.name)).await?;
    Ok(TableStructure {
        object: ObjectRef { kind: rel.kind, ..object.clone() },
        columns,
        constraints,
        indexes,
        row_identity,
        full_row_key: Vec::new(),
        comment: rel.comment,
        row_estimate: rel.estimate,
        foreign_keys,
        size: rel.size,
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

/// `COLUMN_DEFAULT` as a SQL expression. MySQL reports literals unquoted and
/// marks expressions `DEFAULT_GENERATED`; MariaDB already quotes literals and
/// reports a NULL default as the text `NULL`.
fn default_sql(raw: Option<String>, extra: &str, mariadb: bool) -> Option<String> {
    let raw = raw?;
    if mariadb {
        return (raw != "NULL").then_some(raw);
    }
    if !extra.contains("default_generated") {
        return Some(MysqlDialect.quote_literal(&raw));
    }
    let upper = raw.to_ascii_uppercase();
    let bare =
        ["CURRENT_TIMESTAMP", "NOW(", "LOCALTIME", "CURRENT_DATE", "CURRENT_TIME"].iter().any(|k| upper.starts_with(k));
    Some(if bare { raw } else { format!("({raw})") })
}

async fn columns(conn: &mut Conn, object: &ObjectRef, mariadb: bool) -> DbResult<Vec<ColumnInfo>> {
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
                default: default_sql(text(r, 3), &extra, mariadb),
                primary_key: text(r, 4).as_deref() == Some("PRI"),
                generated: always_generated || extra.contains("auto_increment"),
                identity: extra.contains("auto_increment"),
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
                let target = ObjectRef::new(
                    NodeKind::Table,
                    text(first, 3).unwrap_or_default(),
                    None,
                    text(first, 4).unwrap_or_default(),
                );
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
            IndexInfo {
                name: first.index.clone(),
                definition,
                unique: first.unique,
                primary,
                columns: parts.iter().filter_map(|p| p.column.clone()).collect(),
                size_bytes: None,
            }
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

/// InnoDB index sizes in bytes. Needs read access to `mysql.innodb_index_stats`;
/// without it the sizes are simply not shown.
async fn index_sizes(conn: &mut Conn, object: &ObjectRef) -> Vec<(String, i64)> {
    let rows: Vec<Row> = conn
        .exec(
            "SELECT index_name, stat_value * @@innodb_page_size FROM mysql.innodb_index_stats \
             WHERE database_name = ? AND table_name = ? AND stat_name = 'size'",
            (&object.database, &object.name),
        )
        .await
        .unwrap_or_default();
    rows.iter().filter_map(|r| Some((text(r, 0)?, number(r, 1)?))).collect()
}

const FK_SELECT: &str = "SELECT k.CONSTRAINT_NAME, k.TABLE_SCHEMA, k.TABLE_NAME, k.COLUMN_NAME, \
       k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME, rc.UPDATE_RULE, rc.DELETE_RULE \
FROM information_schema.KEY_COLUMN_USAGE k \
JOIN information_schema.REFERENTIAL_CONSTRAINTS rc \
  ON rc.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND rc.CONSTRAINT_NAME = k.CONSTRAINT_NAME AND rc.TABLE_NAME = k.TABLE_NAME \
WHERE k.REFERENCED_TABLE_NAME IS NOT NULL AND ";

fn fk_rule(rule: Option<String>) -> Option<String> {
    rule.filter(|r| r != "RESTRICT" && r != "NO ACTION")
}

/// Foreign keys matching `predicate` over `k` (KEY_COLUMN_USAGE).
async fn foreign_keys(
    conn: &mut Conn,
    predicate: &str,
    params: impl Into<mysql_async::Params> + Send,
) -> DbResult<Vec<ForeignKey>> {
    let rows: Vec<Row> = conn
        .exec(
            format!(
                "{FK_SELECT}{predicate} ORDER BY k.TABLE_SCHEMA, k.TABLE_NAME, k.CONSTRAINT_NAME, k.ORDINAL_POSITION"
            ),
            params,
        )
        .await
        .map_err(my)?;
    let table = |db: Option<String>, name: Option<String>| {
        ObjectRef::new(NodeKind::Table, db.unwrap_or_default(), None, name.unwrap_or_default())
    };
    let mut out: Vec<ForeignKey> = Vec::new();
    for r in &rows {
        let name = text(r, 0).unwrap_or_default();
        let source = table(text(r, 1), text(r, 2));
        match out.last_mut().filter(|fk| fk.name == name && fk.source == source) {
            Some(fk) => {
                fk.columns.extend(text(r, 3));
                fk.target_columns.extend(text(r, 6));
            }
            None => out.push(ForeignKey {
                name,
                source,
                columns: text(r, 3).into_iter().collect(),
                target: table(text(r, 4), text(r, 5)),
                target_columns: text(r, 6).into_iter().collect(),
                on_update: fk_rule(text(r, 7)),
                on_delete: fk_rule(text(r, 8)),
            }),
        }
    }
    Ok(out)
}

/// Foreign keys, view sources and triggers around a table or view.
pub async fn dependencies(conn: &mut Conn, object: &ObjectRef) -> DbResult<Dependencies> {
    if !object.kind.is_relation() {
        return Ok(Dependencies::default());
    }
    let key = (&object.database, &object.name);
    let mut deps = Dependencies::default();
    for fk in foreign_keys(conn, "k.TABLE_SCHEMA = ? AND k.TABLE_NAME = ?", key).await? {
        deps.uses.push(Dependency { object: fk.target, relation: format!("foreign key {}", fk.name) });
    }
    for fk in foreign_keys(conn, "k.REFERENCED_TABLE_SCHEMA = ? AND k.REFERENCED_TABLE_NAME = ?", key).await? {
        deps.used_by.push(Dependency { object: fk.source, relation: format!("foreign key {}", fk.name) });
    }
    // VIEW_TABLE_USAGE exists on MySQL 8.0.13+; other servers simply show no view edges.
    let sources: Vec<Row> = conn
        .exec(
            "SELECT u.TABLE_SCHEMA, u.TABLE_NAME, t.TABLE_TYPE FROM information_schema.VIEW_TABLE_USAGE u \
             LEFT JOIN information_schema.TABLES t ON t.TABLE_SCHEMA = u.TABLE_SCHEMA AND t.TABLE_NAME = u.TABLE_NAME \
             WHERE u.VIEW_SCHEMA = ? AND u.VIEW_NAME = ? ORDER BY 1, 2",
            key,
        )
        .await
        .unwrap_or_default();
    for r in &sources {
        let kind = kind_of(&text(r, 2).unwrap_or_default());
        let o = ObjectRef::new(kind, text(r, 0).unwrap_or_default(), None, text(r, 1).unwrap_or_default());
        deps.uses.push(Dependency { object: o, relation: "reads".into() });
    }
    let views: Vec<Row> = conn
        .exec(
            "SELECT VIEW_SCHEMA, VIEW_NAME FROM information_schema.VIEW_TABLE_USAGE \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY 1, 2",
            key,
        )
        .await
        .unwrap_or_default();
    for r in &views {
        let o = ObjectRef::new(NodeKind::View, text(r, 0).unwrap_or_default(), None, text(r, 1).unwrap_or_default());
        deps.used_by.push(Dependency { object: o, relation: "view".into() });
    }
    let triggers: Vec<Row> = conn
        .exec(
            "SELECT TRIGGER_NAME FROM information_schema.TRIGGERS WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ? \
             ORDER BY TRIGGER_NAME",
            key,
        )
        .await
        .map_err(my)?;
    for r in &triggers {
        let o = ObjectRef::new(NodeKind::Trigger, object.database.clone(), None, text(r, 0).unwrap_or_default())
            .with_parent(&object.name);
        deps.used_by.push(Dependency { object: o, relation: "trigger".into() });
    }
    Ok(deps)
}

/// Tables, columns and foreign keys of a database.
pub async fn schema_model(conn: &mut Conn, database: &str) -> DbResult<SchemaModel> {
    let rows: Vec<Row> = conn
        .exec(
            "SELECT c.TABLE_NAME, t.TABLE_TYPE, c.COLUMN_NAME, c.COLUMN_TYPE, c.IS_NULLABLE, c.COLUMN_KEY \
             FROM information_schema.COLUMNS c \
             JOIN information_schema.TABLES t ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME \
             WHERE c.TABLE_SCHEMA = ? AND t.TABLE_TYPE <> 'SEQUENCE' \
             ORDER BY c.TABLE_NAME, c.ORDINAL_POSITION LIMIT ?",
            (database, SchemaModel::MAX_COLUMNS as u64),
        )
        .await
        .map_err(my)?;
    let mut model = SchemaModel::default();
    for r in &rows {
        let name = text(r, 0).unwrap_or_default();
        if model.tables.last().is_none_or(|t: &ModelTable| t.object.name != name) {
            let kind = kind_of(&text(r, 1).unwrap_or_default());
            model.tables.push(ModelTable { object: ObjectRef::new(kind, database, None, name), columns: Vec::new() });
        }
        if let Some(table) = model.tables.last_mut() {
            table.columns.push(ModelColumn {
                name: text(r, 2).unwrap_or_default(),
                data_type: text(r, 3).unwrap_or_default(),
                nullable: text(r, 4).as_deref() == Some("YES"),
                primary_key: text(r, 5).as_deref() == Some("PRI"),
            });
        }
    }
    model.truncated = model.tables.len() > SchemaModel::MAX_TABLES || rows.len() >= SchemaModel::MAX_COLUMNS;
    model.tables.truncate(SchemaModel::MAX_TABLES);
    model.foreign_keys = foreign_keys(conn, "k.TABLE_SCHEMA = ?", (database,)).await?;
    Ok(model)
}

/// Definition of an index, trigger, routine or sequence.
async fn object_ddl(conn: &mut Conn, object: &ObjectRef) -> DbResult<String> {
    let d = MysqlDialect;
    let missing = || DbError::not_found(format!("{} no longer exists.", object.name));
    let show = |what: &str| format!("SHOW CREATE {what} {}", d.quoted_name(object));
    let (sql, column) = match object.kind {
        NodeKind::Index => {
            let table = object.owner_table().ok_or_else(|| DbError::invalid("An index needs its table."))?;
            let index_columns = index_columns(conn, &table).await?;
            return indexes(&table, &index_columns)
                .into_iter()
                .find(|i| i.name == object.name)
                .map(|i| {
                    if i.primary {
                        format!("ALTER TABLE {} ADD {};\n", d.display_name(&table), i.definition)
                    } else {
                        format!("{};\n", i.definition)
                    }
                })
                .ok_or_else(missing);
        }
        NodeKind::Trigger => (show("TRIGGER"), 2),
        NodeKind::Function => (show("FUNCTION"), 2),
        NodeKind::Procedure => (show("PROCEDURE"), 2),
        NodeKind::Sequence => (show("SEQUENCE"), 1),
        _ => return Err(DbError::invalid("This object has no definition.")),
    };
    let row: Option<Row> = conn.query_first(sql).await.map_err(my)?;
    let text = row.as_ref().and_then(|r| text(r, column)).ok_or_else(|| {
        DbError::new(
            dq_core::ErrorKind::Permission,
            format!("The definition of {} is not visible to this user.", object.name),
        )
    })?;
    Ok(format!("{};\n", text.trim_end()))
}

/// DDL as the server reports it (`SHOW CREATE TABLE` also covers views).
pub async fn ddl(conn: &mut Conn, object: &ObjectRef) -> DbResult<String> {
    if !object.kind.is_relation() {
        return object_ddl(conn, object).await;
    }
    relation(conn, object).await?;
    let row: Row = conn
        .query_first(format!("SHOW CREATE TABLE {}", MysqlDialect.quoted_name(object)))
        .await
        .map_err(my)?
        .ok_or_else(|| DbError::not_found(format!("{} no longer exists.", MysqlDialect.display_name(object))))?;
    Ok(format!("{};\n", text(&row, 1).unwrap_or_default().trim_end()))
}
