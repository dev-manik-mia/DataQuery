//! Catalog queries over the `sys` views of one database. The connection is
//! already switched to the object's database; names travel as parameters and
//! `OBJECT_ID` receives them through `QUOTENAME`.

use std::fmt::Write;

use dq_core::node::{GROUP_FUNCTIONS, GROUP_PROCEDURES, GROUP_SEQUENCES, Node, NodeKind, ObjectRef};
use dq_core::schema::{
    ColumnInfo, ConstraintInfo, ConstraintKind, Dependencies, Dependency, ForeignKey, IndexInfo, ModelColumn,
    ModelTable, SchemaModel, TableSize, TableStructure,
};
use dq_core::sql::{Dialect, SqlServerDialect};
use dq_core::{DbError, DbResult, ErrorKind};

use crate::connect::Conn;
use crate::rows::{TextRow, first_row, text_rows};

const D: SqlServerDialect = SqlServerDialect;

/// `OBJECT_ID` of `@P1`.`@P2` (schema, name).
const OBJECT: &str = "OBJECT_ID(QUOTENAME(@P1) + '.' + QUOTENAME(@P2))";

const FUNCTION_TYPES: &str = "('FN','IF','TF','FS','FT')";
const PROCEDURE_TYPES: &str = "('P','PC')";

async fn rows(conn: &mut Conn, sql: &str, params: &[&str]) -> DbResult<Vec<TextRow>> {
    text_rows(conn, sql, params, None).await
}

fn schema_of(object: &ObjectRef) -> &str {
    object.schema.as_deref().unwrap_or("dbo")
}

fn kind_of(object_type: &str) -> NodeKind {
    match object_type.trim() {
        "V" => NodeKind::View,
        "SO" => NodeKind::Sequence,
        "P" | "PC" => NodeKind::Procedure,
        "FN" | "IF" | "TF" | "FS" | "FT" => NodeKind::Function,
        "TR" => NodeKind::Trigger,
        _ => NodeKind::Table,
    }
}

fn not_found(object: &ObjectRef) -> DbError {
    DbError::not_found(format!("{} no longer exists.", D.display_name(object)))
}

pub async fn databases(conn: &mut Conn, current: &str) -> DbResult<Vec<Node>> {
    let found = rows(
        conn,
        "SELECT name FROM sys.databases WHERE HAS_DBACCESS(name) = 1 AND state = 0 \
         AND (database_id > 4 OR name = @P1) ORDER BY name",
        &[current],
    )
    .await?;
    Ok(found
        .iter()
        .map(|r| {
            let name = r.string(0);
            let mut node = Node::container(ObjectRef::new(NodeKind::Database, name.clone(), None, name.clone()));
            node.current = name == current;
            node
        })
        .collect())
}

pub async fn schemas(conn: &mut Conn, database: &str) -> DbResult<Vec<Node>> {
    let found = rows(
        conn,
        "SELECT s.name, (SELECT COUNT(*) FROM sys.objects o WHERE o.schema_id = s.schema_id AND o.type IN ('U','V')) \
         FROM sys.schemas s WHERE s.schema_id < 16384 AND s.name NOT IN ('sys', 'INFORMATION_SCHEMA', 'guest') \
         ORDER BY CASE WHEN s.name = 'dbo' THEN 0 ELSE 1 END, s.name",
        &[],
    )
    .await?;
    Ok(found
        .iter()
        .map(|r| {
            let name = r.string(0);
            let mut node = Node::container(ObjectRef::new(NodeKind::Schema, database, Some(name.clone()), name));
            node.row_estimate = r.number(1);
            node
        })
        .collect())
}

const RELATION_SELECT: &str = "SELECT s.name AS schema_name, o.name, o.type, \
       (SELECT SUM(p.rows) FROM sys.partitions p WHERE p.object_id = o.object_id AND p.index_id IN (0, 1)) AS estimate, \
       CAST(ep.value AS nvarchar(4000)) AS comment \
FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id \
LEFT JOIN sys.extended_properties ep ON ep.class = 1 AND ep.major_id = o.object_id AND ep.minor_id = 0 \
     AND ep.name = 'MS_Description' \
WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0";

fn relation_node(database: &str, r: &TextRow) -> Node {
    let kind = kind_of(&r.string(2));
    let object = ObjectRef::new(kind, database, Some(r.string(0)), r.string(1));
    let mut node = if kind == NodeKind::Table { Node::container(object) } else { Node::leaf(object) };
    if kind == NodeKind::Table {
        node.row_estimate = r.number(3);
    }
    node.comment = r.text(4).filter(|c| !c.is_empty());
    node
}

pub async fn relations(conn: &mut Conn, database: &str, schema: &str) -> DbResult<Vec<Node>> {
    let found = rows(conn, &format!("{RELATION_SELECT} AND s.name = @P1 ORDER BY o.type, o.name"), &[schema]).await?;
    let mut nodes: Vec<Node> = found.iter().map(|r| relation_node(database, r)).collect();
    let counts = first_row(
        conn,
        &format!(
            "SELECT SUM(CASE WHEN o.type = 'SO' THEN 1 ELSE 0 END), \
                    SUM(CASE WHEN o.type IN {FUNCTION_TYPES} THEN 1 ELSE 0 END), \
                    SUM(CASE WHEN o.type IN {PROCEDURE_TYPES} THEN 1 ELSE 0 END) \
             FROM sys.objects o WHERE o.schema_id = SCHEMA_ID(@P1) AND o.is_ms_shipped = 0"
        ),
        &[schema],
        None,
    )
    .await?;
    for (i, group) in [GROUP_SEQUENCES, GROUP_FUNCTIONS, GROUP_PROCEDURES].into_iter().enumerate() {
        let count = counts.as_ref().and_then(|r| r.number(i)).unwrap_or(0);
        if count > 0 {
            let mut node = Node::container(ObjectRef::new(NodeKind::Group, database, Some(schema.to_owned()), group));
            node.row_estimate = Some(count);
            nodes.push(node);
        }
    }
    Ok(nodes)
}

/// Indexes and triggers of a table.
pub async fn table_children(conn: &mut Conn, table: &ObjectRef) -> DbResult<Vec<Node>> {
    let key = [schema_of(table), table.name.as_str()];
    let indexes = rows(
        conn,
        &format!(
            "SELECT name FROM sys.indexes WHERE object_id = {OBJECT} AND index_id > 0 AND is_hypothetical = 0 \
             ORDER BY is_primary_key DESC, name"
        ),
        &key,
    )
    .await?;
    let triggers =
        rows(conn, &format!("SELECT name FROM sys.triggers WHERE parent_id = {OBJECT} ORDER BY name"), &key).await?;
    let child = |kind, name: String| {
        Node::leaf(ObjectRef::new(kind, table.database.clone(), table.schema.clone(), name).with_parent(&table.name))
    };
    Ok(indexes
        .iter()
        .map(|r| child(NodeKind::Index, r.string(0)))
        .chain(triggers.iter().map(|r| child(NodeKind::Trigger, r.string(0))))
        .collect())
}

pub async fn group_children(conn: &mut Conn, group: &ObjectRef) -> DbResult<Vec<Node>> {
    let types = match group.name.as_str() {
        GROUP_SEQUENCES => "('SO')",
        GROUP_FUNCTIONS => FUNCTION_TYPES,
        GROUP_PROCEDURES => PROCEDURE_TYPES,
        other => return Err(DbError::invalid(format!("Unknown group `{other}`."))),
    };
    let found = rows(
        conn,
        &format!(
            "SELECT o.name, o.type FROM sys.objects o WHERE o.schema_id = SCHEMA_ID(@P1) AND o.type IN {types} \
             AND o.is_ms_shipped = 0 ORDER BY o.name"
        ),
        &[schema_of(group)],
    )
    .await?;
    Ok(found
        .iter()
        .map(|r| {
            Node::leaf(ObjectRef::new(kind_of(&r.string(1)), group.database.clone(), group.schema.clone(), r.string(0)))
        })
        .collect())
}

pub async fn search(conn: &mut Conn, database: &str, query: &str, limit: u32) -> DbResult<Vec<Node>> {
    let pattern = dq_core::sql::builder::like_contains(&query.to_lowercase());
    let found = rows(
        conn,
        &format!(
            "SELECT TOP (CAST(@P2 AS int)) * FROM ({RELATION_SELECT} AND LOWER(o.name) LIKE @P1 ESCAPE '\\') r \
             ORDER BY LEN(r.name), r.name"
        ),
        &[&pattern, &limit.to_string()],
    )
    .await?;
    Ok(found.iter().map(|r| relation_node(database, r)).collect())
}

/// Kind, row estimate and comment of a table or view.
async fn relation(conn: &mut Conn, object: &ObjectRef) -> DbResult<(NodeKind, Option<i64>, Option<String>)> {
    let r = first_row(
        conn,
        &format!("{RELATION_SELECT} AND o.object_id = {OBJECT}"),
        &[schema_of(object), &object.name],
        None,
    )
    .await?
    .ok_or_else(|| not_found(object))?;
    Ok((kind_of(&r.string(2)), r.number(3), r.text(4).filter(|c| !c.is_empty())))
}

pub async fn row_estimate(conn: &mut Conn, object: &ObjectRef) -> DbResult<Option<i64>> {
    Ok(relation(conn, object).await?.1)
}

/// Type spelled as in a column definition: `nvarchar(50)`, `decimal(12,2)`, `varbinary(max)`.
fn type_spelling(name: &str, max_length: i64, precision: i64, scale: i64) -> String {
    let length = |unit: i64| if max_length == -1 { "max".to_owned() } else { (max_length / unit).to_string() };
    match name {
        "varchar" | "char" | "varbinary" | "binary" => format!("{name}({})", length(1)),
        "nvarchar" | "nchar" => format!("{name}({})", length(2)),
        "decimal" | "numeric" => format!("{name}({precision},{scale})"),
        "datetime2" | "time" | "datetimeoffset" => format!("{name}({scale})"),
        _ => name.to_owned(),
    }
}

/// `((0))` → `0`, `('pending')` → `'pending'`: the default as it was written.
pub fn unwrap_default(definition: &str) -> String {
    let mut d = definition.trim();
    while d.starts_with('(') && d.ends_with(')') && wrapped_once(d) {
        d = d[1..d.len() - 1].trim();
    }
    d.to_owned()
}

/// The outer parentheses enclose the whole expression.
fn wrapped_once(d: &str) -> bool {
    let mut depth = 0;
    let mut quoted = false;
    for (i, c) in d.char_indices() {
        match c {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => {
                depth -= 1;
                if depth == 0 && i != d.len() - 1 {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

const NUMERIC_TYPES: &[&str] =
    &["tinyint", "smallint", "int", "bigint", "decimal", "numeric", "money", "smallmoney", "float", "real"];
/// Types the grid cannot edit as text.
const READ_ONLY_TYPES: &[&str] =
    &["binary", "varbinary", "image", "timestamp", "rowversion", "geography", "geometry", "hierarchyid", "sql_variant"];
/// CLR types read with `ToString()`.
const CLR_TYPES: &[&str] = &["geography", "geometry", "hierarchyid"];

const COLUMN_SELECT: &str = "SELECT c.name, TYPE_NAME(c.user_type_id), c.max_length, c.precision, c.scale, \
       c.is_nullable, OBJECT_DEFINITION(c.default_object_id), c.is_identity, c.is_computed, \
       CAST(ep.value AS nvarchar(4000)), \
       CASE WHEN EXISTS (SELECT 1 FROM sys.indexes i JOIN sys.index_columns ic \
            ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
            WHERE i.object_id = c.object_id AND i.is_primary_key = 1 AND ic.column_id = c.column_id) THEN 1 ELSE 0 END, \
       cc.definition, cc.is_persisted, CAST(IDENT_SEED(QUOTENAME(OBJECT_SCHEMA_NAME(c.object_id)) + '.' + \
       QUOTENAME(OBJECT_NAME(c.object_id))) AS bigint), CAST(IDENT_INCR(QUOTENAME(OBJECT_SCHEMA_NAME(c.object_id)) \
       + '.' + QUOTENAME(OBJECT_NAME(c.object_id))) AS bigint) \
FROM sys.columns c \
LEFT JOIN sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
LEFT JOIN sys.extended_properties ep ON ep.class = 1 AND ep.major_id = c.object_id AND ep.minor_id = c.column_id \
     AND ep.name = 'MS_Description'";

struct Column {
    info: ColumnInfo,
    /// `AS (expr) [PERSISTED]` of a computed column.
    computed: Option<String>,
    /// `IDENTITY(seed,increment)`.
    identity: Option<String>,
}

async fn columns(conn: &mut Conn, object: &ObjectRef) -> DbResult<Vec<Column>> {
    let found = rows(
        conn,
        &format!("{COLUMN_SELECT} WHERE c.object_id = {OBJECT} ORDER BY c.column_id"),
        &[schema_of(object), &object.name],
    )
    .await?;
    Ok(found
        .iter()
        .map(|r| {
            let base = r.string(1).to_ascii_lowercase();
            let identity = r.flag(7);
            let computed = r.flag(8);
            Column {
                info: ColumnInfo {
                    name: r.string(0),
                    data_type: type_spelling(
                        &base,
                        r.number(2).unwrap_or(0),
                        r.number(3).unwrap_or(0),
                        r.number(4).unwrap_or(0),
                    ),
                    nullable: r.flag(5),
                    default: r.text(6).map(|d| unwrap_default(&d)),
                    primary_key: r.flag(10),
                    generated: identity || computed || matches!(base.as_str(), "timestamp" | "rowversion"),
                    identity,
                    editable: !identity && !computed && !READ_ONLY_TYPES.contains(&base.as_str()),
                    numeric: NUMERIC_TYPES.contains(&base.as_str()),
                    comment: r.text(9).filter(|c| !c.is_empty()),
                    read_as_text: CLR_TYPES.contains(&base.as_str()),
                },
                computed: computed
                    .then(|| format!("AS {}{}", r.string(11), if r.flag(12) { " PERSISTED" } else { "" })),
                identity: identity
                    .then(|| format!("IDENTITY({},{})", r.number(13).unwrap_or(1), r.number(14).unwrap_or(1))),
            }
        })
        .collect())
}

struct Index {
    info: IndexInfo,
    constraint: Option<ConstraintKind>,
    /// All key columns are NOT NULL and the index has no filter.
    total: bool,
}

async fn indexes(conn: &mut Conn, object: &ObjectRef, columns: &[ColumnInfo]) -> DbResult<Vec<Index>> {
    let found = rows(
        conn,
        &format!(
            "SELECT i.name, i.is_unique, i.is_primary_key, i.is_unique_constraint, i.type_desc, i.filter_definition, \
                    c.name, ic.is_descending_key, ic.is_included_column, \
                    (SELECT SUM(s.used_page_count) * 8192 FROM sys.dm_db_partition_stats s \
                     WHERE s.object_id = i.object_id AND s.index_id = i.index_id) \
             FROM sys.indexes i \
             JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
             JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
             WHERE i.object_id = {OBJECT} AND i.index_id > 0 AND i.is_hypothetical = 0 \
             ORDER BY i.is_primary_key DESC, i.name, ic.is_included_column, ic.key_ordinal, ic.index_column_id"
        ),
        &[schema_of(object), &object.name],
    )
    .await
    .or_else(|e| if e.kind == ErrorKind::Permission { Ok(Vec::new()) } else { Err(e) })?;
    let table = D.display_name(object);
    let not_null = |name: &str| columns.iter().any(|c| c.name == name && !c.nullable);
    let mut out = Vec::new();
    for group in found.chunk_by(|a, b| a.text(0) == b.text(0)) {
        let first = &group[0];
        let name = first.string(0);
        let keys: Vec<&TextRow> = group.iter().filter(|r| !r.flag(8)).collect();
        let included: Vec<String> = group.iter().filter(|r| r.flag(8)).map(|r| D.display_ident(&r.string(6))).collect();
        let key_list = keys
            .iter()
            .map(|r| format!("{}{}", D.display_ident(&r.string(6)), if r.flag(7) { " DESC" } else { "" }))
            .collect::<Vec<_>>()
            .join(", ");
        let kind = first.string(4).replace('_', " ");
        let constraint = if first.flag(2) {
            Some(ConstraintKind::PrimaryKey)
        } else if first.flag(3) {
            Some(ConstraintKind::Unique)
        } else {
            None
        };
        let definition = match constraint {
            Some(ConstraintKind::PrimaryKey) => {
                format!("CONSTRAINT {} PRIMARY KEY {kind} ({key_list})", D.display_ident(&name))
            }
            Some(_) => format!("CONSTRAINT {} UNIQUE {kind} ({key_list})", D.display_ident(&name)),
            None => {
                let mut def = format!(
                    "CREATE {}{kind} INDEX {} ON {table} ({key_list})",
                    if first.flag(1) { "UNIQUE " } else { "" },
                    D.display_ident(&name)
                );
                if !included.is_empty() {
                    let _ = write!(def, " INCLUDE ({})", included.join(", "));
                }
                if let Some(filter) = first.text(5) {
                    let _ = write!(def, " WHERE {filter}");
                }
                def
            }
        };
        let key_names: Vec<String> = keys.iter().map(|r| r.string(6)).collect();
        out.push(Index {
            total: first.flag(1) && first.text(5).is_none() && key_names.iter().all(|c| not_null(c)),
            info: IndexInfo {
                name,
                definition,
                unique: first.flag(1),
                primary: first.flag(2),
                columns: key_names,
                size_bytes: first.number(9),
            },
            constraint,
        });
    }
    Ok(out)
}

async fn check_constraints(conn: &mut Conn, object: &ObjectRef) -> DbResult<Vec<ConstraintInfo>> {
    let found = rows(
        conn,
        &format!("SELECT name, definition FROM sys.check_constraints WHERE parent_object_id = {OBJECT} ORDER BY name"),
        &[schema_of(object), &object.name],
    )
    .await?;
    Ok(found
        .iter()
        .map(|r| ConstraintInfo {
            kind: ConstraintKind::Check,
            name: r.string(0),
            definition: format!("CHECK {}", r.string(1)),
        })
        .collect())
}

const FK_SELECT: &str = "SELECT fk.name, SCHEMA_NAME(fk.schema_id), OBJECT_NAME(fk.parent_object_id), pc.name, \
       SCHEMA_NAME(rt.schema_id), rt.name, rc.name, fk.update_referential_action_desc, \
       fk.delete_referential_action_desc \
FROM sys.foreign_keys fk \
JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
JOIN sys.objects rt ON rt.object_id = fk.referenced_object_id \
JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
WHERE ";

fn fk_rule(rule: Option<String>) -> Option<String> {
    rule.filter(|r| r != "NO_ACTION").map(|r| r.replace('_', " "))
}

/// Foreign keys matching `predicate` (over `fk`).
async fn foreign_keys(conn: &mut Conn, database: &str, predicate: &str, params: &[&str]) -> DbResult<Vec<ForeignKey>> {
    let found =
        rows(conn, &format!("{FK_SELECT}{predicate} ORDER BY 2, 3, 1, fkc.constraint_column_id"), params).await?;
    let table = |schema: String, name: String| ObjectRef::new(NodeKind::Table, database, Some(schema), name);
    let mut out: Vec<ForeignKey> = Vec::new();
    for r in &found {
        let name = r.string(0);
        let source = table(r.string(1), r.string(2));
        match out.last_mut().filter(|fk| fk.name == name && fk.source == source) {
            Some(fk) => {
                fk.columns.push(r.string(3));
                fk.target_columns.push(r.string(6));
            }
            None => out.push(ForeignKey {
                name,
                source,
                columns: vec![r.string(3)],
                target: table(r.string(4), r.string(5)),
                target_columns: vec![r.string(6)],
                on_update: fk_rule(r.text(7)),
                on_delete: fk_rule(r.text(8)),
            }),
        }
    }
    Ok(out)
}

fn fk_definition(fk: &ForeignKey, object: &ObjectRef) -> String {
    let list = |cols: &[String]| cols.iter().map(|c| D.display_ident(c)).collect::<Vec<_>>().join(", ");
    let target =
        if fk.target.schema == object.schema { D.display_ident(&fk.target.name) } else { D.display_name(&fk.target) };
    let mut def = format!("FOREIGN KEY ({}) REFERENCES {target} ({})", list(&fk.columns), list(&fk.target_columns));
    if let Some(rule) = &fk.on_update {
        let _ = write!(def, " ON UPDATE {rule}");
    }
    if let Some(rule) = &fk.on_delete {
        let _ = write!(def, " ON DELETE {rule}");
    }
    def
}

/// Bytes used by the table (heap or clustered index) and by its other indexes.
async fn size(conn: &mut Conn, object: &ObjectRef) -> Option<TableSize> {
    let r = first_row(
        conn,
        &format!(
            "SELECT SUM(CASE WHEN index_id IN (0, 1) THEN used_page_count ELSE 0 END) * 8192, \
                    SUM(CASE WHEN index_id > 1 THEN used_page_count ELSE 0 END) * 8192 \
             FROM sys.dm_db_partition_stats WHERE object_id = {OBJECT}"
        ),
        &[schema_of(object), &object.name],
        None,
    )
    .await
    .ok()??;
    let (table, index) = (r.number(0)?, r.number(1).unwrap_or(0));
    Some(TableSize { total_bytes: table + index, table_bytes: table, index_bytes: index })
}

pub async fn structure(conn: &mut Conn, object: &ObjectRef) -> DbResult<TableStructure> {
    Ok(full_structure(conn, object).await?.0)
}

async fn full_structure(conn: &mut Conn, object: &ObjectRef) -> DbResult<(TableStructure, Vec<Column>, Vec<Index>)> {
    let (kind, estimate, comment) = relation(conn, object).await?;
    let columns = columns(conn, object).await?;
    let infos: Vec<ColumnInfo> = columns.iter().map(|c| c.info.clone()).collect();
    let indexes = indexes(conn, object, &infos).await?;
    let foreign_keys = foreign_keys(
        conn,
        &object.database,
        &format!("fk.parent_object_id = {OBJECT}"),
        &[schema_of(object), &object.name],
    )
    .await?;
    let mut constraints: Vec<ConstraintInfo> = indexes
        .iter()
        .filter_map(|i| {
            let kind = i.constraint?;
            let definition = i.info.definition.split_once(' ').map(|(_, rest)| rest)?;
            // Drop the leading `CONSTRAINT [name] `.
            let definition = definition.split_once(' ').map_or(definition, |(_, rest)| rest).to_owned();
            Some(ConstraintInfo { kind, name: i.info.name.clone(), definition })
        })
        .collect();
    constraints.extend(foreign_keys.iter().map(|fk| ConstraintInfo {
        kind: ConstraintKind::ForeignKey,
        name: fk.name.clone(),
        definition: fk_definition(fk, object),
    }));
    constraints.extend(check_constraints(conn, object).await?);
    let row_identity = if kind == NodeKind::Table {
        indexes
            .iter()
            .find(|i| i.info.primary)
            .or_else(|| indexes.iter().find(|i| i.total))
            .map(|i| i.info.columns.clone())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let size = if kind == NodeKind::Table { size(conn, object).await } else { None };
    let structure = TableStructure {
        object: ObjectRef { kind, ..object.clone() },
        columns: infos,
        constraints,
        indexes: indexes.iter().map(|i| i.info.clone()).collect(),
        row_identity,
        full_row_key: Vec::new(),
        comment,
        row_estimate: if kind == NodeKind::Table { estimate } else { None },
        foreign_keys,
        size,
    };
    Ok((structure, columns, indexes))
}

/// Foreign keys, view sources and triggers around a table or view.
pub async fn dependencies(conn: &mut Conn, object: &ObjectRef) -> DbResult<Dependencies> {
    if !object.kind.is_relation() {
        return Ok(Dependencies::default());
    }
    let key = [schema_of(object), object.name.as_str()];
    let db = &object.database;
    let mut deps = Dependencies::default();
    for fk in foreign_keys(conn, db, &format!("fk.parent_object_id = {OBJECT}"), &key).await? {
        deps.uses.push(Dependency { object: fk.target, relation: format!("foreign key {}", fk.name) });
    }
    for fk in foreign_keys(conn, db, &format!("fk.referenced_object_id = {OBJECT}"), &key).await? {
        deps.used_by.push(Dependency { object: fk.source, relation: format!("foreign key {}", fk.name) });
    }
    let edge = "SELECT DISTINCT OBJECT_SCHEMA_NAME(o.object_id), o.name, o.type FROM sys.sql_expression_dependencies d \
                JOIN sys.objects o ON o.object_id = ";
    let uses = rows(conn, &format!("{edge}d.referenced_id WHERE d.referencing_id = {OBJECT}"), &key).await?;
    for r in &uses {
        let o = ObjectRef::new(kind_of(&r.string(2)), db.clone(), Some(r.string(0)), r.string(1));
        deps.uses.push(Dependency { object: o, relation: "reads".into() });
    }
    let used_by =
        rows(conn, &format!("{edge}d.referencing_id WHERE d.referenced_id = {OBJECT} AND o.type <> 'TR'"), &key)
            .await?;
    for r in &used_by {
        let kind = kind_of(&r.string(2));
        let o = ObjectRef::new(kind, db.clone(), Some(r.string(0)), r.string(1));
        let relation = if kind == NodeKind::View { "view" } else { "reads" };
        deps.used_by.push(Dependency { object: o, relation: relation.into() });
    }
    let triggers =
        rows(conn, &format!("SELECT name FROM sys.triggers WHERE parent_id = {OBJECT} ORDER BY name"), &key).await?;
    for r in &triggers {
        let o =
            ObjectRef::new(NodeKind::Trigger, db.clone(), object.schema.clone(), r.string(0)).with_parent(&object.name);
        deps.used_by.push(Dependency { object: o, relation: "trigger".into() });
    }
    Ok(deps)
}

/// Tables, columns and foreign keys of one schema, or of every user schema.
pub async fn schema_model(conn: &mut Conn, database: &str, schema: Option<&str>) -> DbResult<SchemaModel> {
    let filter = if schema.is_some() { "AND s.name = @P1" } else { "AND s.schema_id < 16384 AND s.name <> 'sys'" };
    let params: Vec<&str> = schema.into_iter().collect();
    let found = rows(
        conn,
        &format!(
            "SELECT TOP {} s.name, o.name, o.type, c.name, TYPE_NAME(c.user_type_id), c.max_length, c.precision, \
                    c.scale, c.is_nullable, \
                    CASE WHEN EXISTS (SELECT 1 FROM sys.indexes i JOIN sys.index_columns ic \
                         ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                         WHERE i.object_id = o.object_id AND i.is_primary_key = 1 AND ic.column_id = c.column_id) \
                    THEN 1 ELSE 0 END \
             FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id \
             JOIN sys.columns c ON c.object_id = o.object_id \
             WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0 {filter} \
             ORDER BY s.name, o.name, c.column_id",
            SchemaModel::MAX_COLUMNS
        ),
        &params,
    )
    .await?;
    let mut model = SchemaModel::default();
    for r in &found {
        let (s, name) = (r.string(0), r.string(1));
        if model
            .tables
            .last()
            .is_none_or(|t: &ModelTable| t.object.name != name || t.object.schema.as_deref() != Some(&s))
        {
            let object = ObjectRef::new(kind_of(&r.string(2)), database, Some(s), name);
            model.tables.push(ModelTable { object, columns: Vec::new() });
        }
        if let Some(table) = model.tables.last_mut() {
            let base = r.string(4).to_ascii_lowercase();
            table.columns.push(ModelColumn {
                name: r.string(3),
                data_type: type_spelling(
                    &base,
                    r.number(5).unwrap_or(0),
                    r.number(6).unwrap_or(0),
                    r.number(7).unwrap_or(0),
                ),
                nullable: r.flag(8),
                primary_key: r.flag(9),
            });
        }
    }
    model.truncated = model.tables.len() > SchemaModel::MAX_TABLES || found.len() >= SchemaModel::MAX_COLUMNS;
    model.tables.truncate(SchemaModel::MAX_TABLES);
    let fk_filter = match schema {
        Some(_) => "SCHEMA_NAME(fk.schema_id) = @P1",
        None => "1 = 1",
    };
    model.foreign_keys = foreign_keys(conn, database, fk_filter, &params).await?;
    Ok(model)
}

/// `CREATE TABLE` with its constraints, then its other indexes.
async fn table_ddl(conn: &mut Conn, object: &ObjectRef) -> DbResult<String> {
    let (structure, columns, indexes) = full_structure(conn, object).await?;
    let mut lines: Vec<String> = columns
        .iter()
        .map(|c| {
            let name = D.display_ident(&c.info.name);
            if let Some(computed) = &c.computed {
                return format!("{name} {computed}");
            }
            let mut line = format!("{name} {}", c.info.data_type);
            if let Some(identity) = &c.identity {
                let _ = write!(line, " {identity}");
            }
            line.push_str(if c.info.nullable { " NULL" } else { " NOT NULL" });
            if let Some(default) = &c.info.default {
                let _ = write!(line, " DEFAULT {default}");
            }
            line
        })
        .collect();
    lines.extend(indexes.iter().filter(|i| i.constraint.is_some()).map(|i| i.info.definition.clone()));
    for c in
        structure.constraints.iter().filter(|c| matches!(c.kind, ConstraintKind::ForeignKey | ConstraintKind::Check))
    {
        lines.push(format!("CONSTRAINT {} {}", D.display_ident(&c.name), c.definition));
    }
    let mut out = format!("CREATE TABLE {} (\n  {}\n);\n", D.display_name(object), lines.join(",\n  "));
    for index in indexes.iter().filter(|i| i.constraint.is_none()) {
        let _ = writeln!(out, "{};", index.info.definition);
    }
    Ok(out)
}

/// Definition of any object with a definition.
pub async fn ddl(conn: &mut Conn, object: &ObjectRef) -> DbResult<String> {
    let schema = schema_of(object);
    match object.kind {
        NodeKind::Table => table_ddl(conn, object).await,
        NodeKind::Index => {
            let table = object.owner_table().ok_or_else(|| DbError::invalid("An index needs its table."))?;
            let columns: Vec<ColumnInfo> = columns(conn, &table).await?.into_iter().map(|c| c.info).collect();
            let index = indexes(conn, &table, &columns)
                .await?
                .into_iter()
                .find(|i| i.info.name == object.name)
                .ok_or_else(|| not_found(object))?;
            Ok(if index.constraint.is_some() {
                format!("ALTER TABLE {} ADD {};\n", D.display_name(&table), index.info.definition)
            } else {
                format!("{};\n", index.info.definition)
            })
        }
        NodeKind::Sequence => {
            let r = first_row(
                conn,
                &format!(
                    "SELECT TYPE_NAME(user_type_id), CAST(start_value AS nvarchar(64)), CAST(increment AS nvarchar(64)), \
                            CAST(minimum_value AS nvarchar(64)), CAST(maximum_value AS nvarchar(64)), is_cycling \
                     FROM sys.sequences WHERE object_id = {OBJECT}"
                ),
                &[schema, &object.name],
                None,
            )
            .await?
            .ok_or_else(|| not_found(object))?;
            Ok(format!(
                "CREATE SEQUENCE {} AS {}\n  START WITH {} INCREMENT BY {}\n  MINVALUE {} MAXVALUE {}\n  {};\n",
                D.display_name(object),
                r.string(0),
                r.string(1),
                r.string(2),
                r.string(3),
                r.string(4),
                if r.flag(5) { "CYCLE" } else { "NO CYCLE" }
            ))
        }
        NodeKind::View | NodeKind::Function | NodeKind::Procedure | NodeKind::Trigger => {
            let r =
                first_row(conn, &format!("SELECT OBJECT_DEFINITION({OBJECT})"), &[schema, &object.name], None).await?;
            let text = r.and_then(|r| r.text(0)).ok_or_else(|| {
                DbError::new(
                    ErrorKind::Permission,
                    format!("The definition of {} is not visible to this login.", object.name),
                )
            })?;
            Ok(format!("{}\nGO\n", text.trim_end()))
        }
        _ => Err(DbError::invalid("This object has no definition.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_lose_their_wrapping() {
        assert_eq!(unwrap_default("((0))"), "0");
        assert_eq!(unwrap_default("('pending')"), "'pending'");
        assert_eq!(unwrap_default("(sysutcdatetime())"), "sysutcdatetime()");
        assert_eq!(unwrap_default("((1)+(2))"), "(1)+(2)");
        assert_eq!(unwrap_default("(')(')"), "')('");
    }

    #[test]
    fn spells_types() {
        assert_eq!(type_spelling("nvarchar", 100, 0, 0), "nvarchar(50)");
        assert_eq!(type_spelling("varbinary", -1, 0, 0), "varbinary(max)");
        assert_eq!(type_spelling("decimal", 9, 12, 2), "decimal(12,2)");
        assert_eq!(type_spelling("datetime2", 8, 27, 3), "datetime2(3)");
        assert_eq!(type_spelling("int", 4, 10, 0), "int");
    }
}
