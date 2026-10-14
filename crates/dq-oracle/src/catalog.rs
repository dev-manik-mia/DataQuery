//! Catalog queries over the `ALL_*` dictionary views (blocking; run through
//! the pool). Names travel as bind variables; DDL comes from `DBMS_METADATA`.

use std::fmt::Write;

use dq_core::node::{GROUP_FUNCTIONS, GROUP_PROCEDURES, GROUP_SEQUENCES, Node, NodeKind, ObjectRef};
use dq_core::schema::{
    ColumnInfo, ConstraintInfo, ConstraintKind, Dependencies, Dependency, ForeignKey, IndexInfo, ModelColumn,
    ModelTable, SchemaModel, TableSize, TableStructure,
};
use dq_core::sql::{Dialect, OracleDialect};
use dq_core::{DbError, DbResult, ErrorKind};
use oracle::Connection;
use oracle::sql_type::ToSql;

use crate::rows::{TextRow, first_row, text_rows};

const D: OracleDialect = OracleDialect;

fn rows(conn: &Connection, sql: &str, params: &[&dyn ToSql]) -> DbResult<Vec<TextRow>> {
    text_rows(conn, sql, params)
}

fn owner_of(object: &ObjectRef) -> DbResult<&str> {
    object.schema.as_deref().ok_or_else(|| DbError::invalid("A schema is required."))
}

fn kind_of(object_type: &str) -> NodeKind {
    match object_type {
        "VIEW" => NodeKind::View,
        "MATERIALIZED VIEW" => NodeKind::MaterializedView,
        "SEQUENCE" => NodeKind::Sequence,
        "FUNCTION" => NodeKind::Function,
        "PROCEDURE" => NodeKind::Procedure,
        "TRIGGER" => NodeKind::Trigger,
        "INDEX" => NodeKind::Index,
        _ => NodeKind::Table,
    }
}

fn not_found(object: &ObjectRef) -> DbError {
    DbError::not_found(format!("{} no longer exists.", D.display_name(object)))
}

/// Schemas Oracle maintains (SYS, SYSTEM, …) are hidden, except the current user.
const USER_SCHEMAS: &str = "(u.oracle_maintained = 'N' OR u.username = USER)";

pub fn schemas(conn: &Connection, database: &str) -> DbResult<Vec<Node>> {
    let found = rows(
        conn,
        &format!(
            "SELECT u.username, (SELECT COUNT(*) FROM all_objects o WHERE o.owner = u.username \
                    AND o.object_type IN ('TABLE', 'VIEW', 'MATERIALIZED VIEW')) \
             FROM all_users u WHERE {USER_SCHEMAS} \
             ORDER BY CASE WHEN u.username = USER THEN 0 ELSE 1 END, u.username"
        ),
        &[],
    )?;
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

/// Tables, views and materialized views. Tables that store a materialized
/// view, and generated helper tables, are left out.
const RELATION_SELECT: &str = "SELECT o.owner, o.object_name, o.object_type, t.num_rows, c.comments \
FROM all_objects o \
LEFT JOIN all_tables t ON t.owner = o.owner AND t.table_name = o.object_name AND o.object_type = 'TABLE' \
LEFT JOIN all_tab_comments c ON c.owner = o.owner AND c.table_name = o.object_name \
WHERE o.object_type IN ('TABLE', 'VIEW', 'MATERIALIZED VIEW') AND o.generated = 'N' \
  AND o.object_name NOT LIKE 'BIN$%' AND o.object_name NOT LIKE 'MLOG$\\_%' ESCAPE '\\' \
  AND NOT (o.object_type = 'TABLE' AND (t.secondary = 'Y' OR t.nested = 'YES' OR EXISTS \
      (SELECT 1 FROM all_mviews m WHERE m.owner = o.owner AND m.mview_name = o.object_name)))";

fn relation_node(database: &str, r: &TextRow) -> Node {
    let kind = kind_of(&r.string(2));
    let object = ObjectRef::new(kind, database, Some(r.string(0)), r.string(1));
    let mut node = if kind == NodeKind::View { Node::leaf(object) } else { Node::container(object) };
    node.row_estimate = r.number(3);
    node.comment = r.text(4).filter(|c| !c.is_empty());
    node
}

pub fn relations(conn: &Connection, database: &str, schema: &str) -> DbResult<Vec<Node>> {
    let found = rows(
        conn,
        &format!(
            "{RELATION_SELECT} AND o.owner = :1 \
             ORDER BY CASE o.object_type WHEN 'TABLE' THEN 0 WHEN 'VIEW' THEN 1 ELSE 2 END, o.object_name"
        ),
        &[&schema],
    )?;
    let mut nodes: Vec<Node> = found.iter().map(|r| relation_node(database, r)).collect();
    let counts = first_row(
        conn,
        "SELECT (SELECT COUNT(*) FROM all_sequences WHERE sequence_owner = :1 AND sequence_name NOT LIKE 'ISEQ$$\\_%' ESCAPE '\\'), \
                (SELECT COUNT(*) FROM all_objects WHERE owner = :1 AND object_type = 'FUNCTION'), \
                (SELECT COUNT(*) FROM all_objects WHERE owner = :1 AND object_type = 'PROCEDURE') FROM DUAL",
        &[&schema],
    )?;
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

/// Indexes and triggers of a table or materialized view.
pub fn table_children(conn: &Connection, table: &ObjectRef) -> DbResult<Vec<Node>> {
    let owner = owner_of(table)?;
    let indexes = rows(
        conn,
        "SELECT index_name FROM all_indexes WHERE table_owner = :1 AND table_name = :2 AND index_type <> 'LOB' \
         ORDER BY index_name",
        &[&owner, &table.name],
    )?;
    let triggers = rows(
        conn,
        "SELECT trigger_name FROM all_triggers WHERE table_owner = :1 AND table_name = :2 ORDER BY trigger_name",
        &[&owner, &table.name],
    )?;
    let child = |kind, name: String| {
        Node::leaf(ObjectRef::new(kind, table.database.clone(), table.schema.clone(), name).with_parent(&table.name))
    };
    Ok(indexes
        .iter()
        .map(|r| child(NodeKind::Index, r.string(0)))
        .chain(triggers.iter().map(|r| child(NodeKind::Trigger, r.string(0))))
        .collect())
}

pub fn group_children(conn: &Connection, group: &ObjectRef) -> DbResult<Vec<Node>> {
    let owner = owner_of(group)?;
    let (sql, kind) = match group.name.as_str() {
        GROUP_SEQUENCES => (
            "SELECT sequence_name FROM all_sequences WHERE sequence_owner = :1 \
             AND sequence_name NOT LIKE 'ISEQ$$\\_%' ESCAPE '\\' ORDER BY sequence_name",
            NodeKind::Sequence,
        ),
        GROUP_FUNCTIONS => (
            "SELECT object_name FROM all_objects WHERE owner = :1 AND object_type = 'FUNCTION' ORDER BY object_name",
            NodeKind::Function,
        ),
        GROUP_PROCEDURES => (
            "SELECT object_name FROM all_objects WHERE owner = :1 AND object_type = 'PROCEDURE' ORDER BY object_name",
            NodeKind::Procedure,
        ),
        other => return Err(DbError::invalid(format!("Unknown group `{other}`."))),
    };
    Ok(rows(conn, sql, &[&owner])?
        .iter()
        .map(|r| Node::leaf(ObjectRef::new(kind, group.database.clone(), group.schema.clone(), r.string(0))))
        .collect())
}

pub fn search(conn: &Connection, database: &str, query: &str, limit: u32) -> DbResult<Vec<Node>> {
    let pattern = dq_core::sql::builder::like_contains(&query.to_lowercase());
    let found = rows(
        conn,
        &format!(
            "{RELATION_SELECT} AND LOWER(o.object_name) LIKE :1 ESCAPE '\\' \
               AND o.owner IN (SELECT u.username FROM all_users u WHERE {USER_SCHEMAS}) \
             ORDER BY LENGTH(o.object_name), o.object_name FETCH FIRST :2 ROWS ONLY"
        ),
        &[&pattern, &i64::from(limit)],
    )?;
    Ok(found.iter().map(|r| relation_node(database, r)).collect())
}

fn relation(conn: &Connection, object: &ObjectRef) -> DbResult<(NodeKind, Option<i64>, Option<String>)> {
    let owner = owner_of(object)?;
    let r = first_row(
        conn,
        &format!("{RELATION_SELECT} AND o.owner = :1 AND o.object_name = :2"),
        &[&owner, &object.name],
    )?
    .ok_or_else(|| not_found(object))?;
    Ok((kind_of(&r.string(2)), r.number(3), r.text(4).filter(|c| !c.is_empty())))
}

pub fn row_estimate(conn: &Connection, object: &ObjectRef) -> DbResult<Option<i64>> {
    Ok(relation(conn, object)?.1)
}

/// The type as written in a column definition.
fn type_spelling(r: &TextRow) -> String {
    let data_type = r.string(1);
    let length = r.number(2).unwrap_or(0);
    let precision = r.number(3);
    let scale = r.number(4);
    let char_length = r.number(10).unwrap_or(0);
    let char_semantics = r.text(11).as_deref() == Some("C");
    match data_type.as_str() {
        "NUMBER" => match (precision, scale) {
            (Some(p), Some(0)) => format!("NUMBER({p})"),
            (Some(p), Some(s)) => format!("NUMBER({p},{s})"),
            (None, Some(0)) => "INTEGER".to_owned(),
            _ => "NUMBER".to_owned(),
        },
        "FLOAT" => precision.map_or_else(|| "FLOAT".to_owned(), |p| format!("FLOAT({p})")),
        "VARCHAR2" | "CHAR" if char_semantics => format!("{data_type}({char_length} CHAR)"),
        "VARCHAR2" | "CHAR" | "RAW" => format!("{data_type}({length})"),
        "NVARCHAR2" | "NCHAR" => format!("{data_type}({char_length})"),
        _ => data_type,
    }
}

const NUMERIC_TYPES: &[&str] = &["NUMBER", "FLOAT", "BINARY_FLOAT", "BINARY_DOUBLE", "INTEGER"];
/// Types the grid cannot edit as text.
const READ_ONLY_TYPES: &[&str] = &["RAW", "LONG RAW", "BLOB", "BFILE", "LONG", "ROWID", "UROWID"];

fn is_text_type(data_type: &str) -> bool {
    matches!(data_type, "XMLTYPE" | "SDO_GEOMETRY" | "JSON")
}

struct Column {
    info: ColumnInfo,
}

fn columns(conn: &Connection, object: &ObjectRef) -> DbResult<Vec<Column>> {
    let owner = owner_of(object)?;
    let found = rows(
        conn,
        "SELECT c.column_name, c.data_type, c.data_length, c.data_precision, c.data_scale, c.nullable, \
                c.data_default, c.identity_column, c.virtual_column, cc.comments, c.char_length, c.char_used, \
                c.data_type_owner, \
                CASE WHEN EXISTS (SELECT 1 FROM all_constraints k JOIN all_cons_columns kc \
                       ON kc.owner = k.owner AND kc.constraint_name = k.constraint_name \
                     WHERE k.owner = c.owner AND k.table_name = c.table_name AND k.constraint_type = 'P' \
                       AND kc.column_name = c.column_name) THEN 'Y' ELSE 'N' END \
         FROM all_tab_cols c \
         LEFT JOIN all_col_comments cc ON cc.owner = c.owner AND cc.table_name = c.table_name AND cc.column_name = c.column_name \
         WHERE c.owner = :1 AND c.table_name = :2 AND c.hidden_column = 'NO' ORDER BY c.column_id",
        &[&owner, &object.name],
    )?;
    Ok(found
        .iter()
        .map(|r| {
            let base = r.string(1);
            let identity = r.flag(7);
            let virtual_column = r.flag(8);
            Column {
                info: ColumnInfo {
                    name: r.string(0),
                    data_type: type_spelling(r),
                    nullable: r.flag(5),
                    default: r.text(6).map(|d| d.trim().to_owned()).filter(|d| !d.is_empty() && d != "NULL"),
                    primary_key: r.flag(13),
                    generated: identity || virtual_column,
                    identity,
                    editable: !identity
                        && !virtual_column
                        && !READ_ONLY_TYPES.contains(&base.as_str())
                        && !is_text_type(&base)
                        && r.text(12).is_none(),
                    numeric: NUMERIC_TYPES.contains(&base.as_str()),
                    comment: r.text(9).filter(|c| !c.is_empty()),
                    read_as_text: is_text_type(&base),
                },
            }
        })
        .collect())
}

/// Columns of each constraint of a table: (name, type, columns, referenced constraint owner/name,
/// delete rule, check condition, generated name).
struct Constraint {
    name: String,
    kind: String,
    columns: Vec<String>,
    r_owner: Option<String>,
    r_name: Option<String>,
    delete_rule: Option<String>,
    condition: Option<String>,
    generated: bool,
}

fn table_constraints(conn: &Connection, owner: &str, table: &str) -> DbResult<Vec<Constraint>> {
    let found = rows(
        conn,
        "SELECT k.constraint_name, k.constraint_type, kc.column_name, k.r_owner, k.r_constraint_name, k.delete_rule, \
                k.search_condition_vc, k.generated \
         FROM all_constraints k \
         LEFT JOIN all_cons_columns kc ON kc.owner = k.owner AND kc.constraint_name = k.constraint_name \
         WHERE k.owner = :1 AND k.table_name = :2 AND k.constraint_type IN ('P', 'U', 'R', 'C') \
         ORDER BY DECODE(k.constraint_type, 'P', 0, 'U', 1, 'R', 2, 3), k.constraint_name, kc.position",
        &[&owner, &table],
    )?;
    let mut out: Vec<Constraint> = Vec::new();
    for r in &found {
        let name = r.string(0);
        match out.last_mut().filter(|c| c.name == name) {
            Some(c) => c.columns.extend(r.text(2)),
            None => out.push(Constraint {
                name,
                kind: r.string(1),
                columns: r.text(2).into_iter().collect(),
                r_owner: r.text(3),
                r_name: r.text(4),
                delete_rule: r.text(5).filter(|d| d != "NO ACTION"),
                condition: r.text(6),
                generated: r.text(7).as_deref() == Some("GENERATED NAME"),
            }),
        }
    }
    Ok(out)
}

/// Columns of a constraint (for the referenced side of a foreign key).
fn constraint_columns(conn: &Connection, owner: &str, name: &str) -> DbResult<(String, Vec<String>)> {
    let found = rows(
        conn,
        "SELECT table_name, column_name FROM all_cons_columns WHERE owner = :1 AND constraint_name = :2 ORDER BY position",
        &[&owner, &name],
    )?;
    let table = found.first().map(|r| r.string(0)).unwrap_or_default();
    Ok((table, found.iter().map(|r| r.string(1)).collect()))
}

/// System-made `"X" IS NOT NULL` checks repeat the column's NOT NULL.
fn is_not_null_check(c: &Constraint) -> bool {
    c.generated && c.condition.as_deref().is_some_and(|s| s.trim_end().ends_with("IS NOT NULL") && !s.contains(" AND "))
}

fn foreign_key(
    conn: &Connection,
    database: &str,
    owner: &str,
    table: &str,
    c: &Constraint,
) -> DbResult<Option<ForeignKey>> {
    let (Some(r_owner), Some(r_name)) = (&c.r_owner, &c.r_name) else { return Ok(None) };
    let (target_table, target_columns) = constraint_columns(conn, r_owner, r_name)?;
    Ok(Some(ForeignKey {
        name: c.name.clone(),
        source: ObjectRef::new(NodeKind::Table, database, Some(owner.to_owned()), table),
        columns: c.columns.clone(),
        target: ObjectRef::new(NodeKind::Table, database, Some(r_owner.clone()), target_table),
        target_columns,
        on_update: None,
        on_delete: c.delete_rule.clone(),
    }))
}

struct Index {
    info: IndexInfo,
    /// All columns NOT NULL, no expressions: usable as a row identity when unique.
    total: bool,
}

fn indexes(
    conn: &Connection,
    object: &ObjectRef,
    columns: &[ColumnInfo],
    primary_index: Option<&str>,
) -> DbResult<Vec<Index>> {
    let owner = owner_of(object)?;
    let found = rows(
        conn,
        "SELECT i.index_name, i.uniqueness, ic.column_name, ic.descend, e.column_expression, i.index_type, i.owner \
         FROM all_indexes i \
         JOIN all_ind_columns ic ON ic.index_owner = i.owner AND ic.index_name = i.index_name \
         LEFT JOIN all_ind_expressions e ON e.index_owner = ic.index_owner AND e.index_name = ic.index_name \
              AND e.column_position = ic.column_position \
         WHERE i.table_owner = :1 AND i.table_name = :2 AND i.index_type <> 'LOB' \
         ORDER BY i.index_name, ic.column_position",
        &[&owner, &object.name],
    )?;
    let not_null = |name: &str| columns.iter().any(|c| c.name == name && !c.nullable);
    let table = D.display_name(object);
    let mut out = Vec::new();
    for group in found.chunk_by(|a, b| a.text(0) == b.text(0)) {
        let first = &group[0];
        let name = first.string(0);
        let unique = first.string(1) == "UNIQUE";
        let mut plain = true;
        let keys: Vec<String> = group
            .iter()
            .map(|r| {
                // DESC columns and function-based keys are stored as expressions.
                let key = match r.text(4) {
                    Some(expr) if r.string(3) == "DESC" => expr.trim().to_owned(),
                    Some(expr) => {
                        plain = false;
                        expr.trim().to_owned()
                    }
                    None => D.display_ident(&r.string(2)),
                };
                if r.string(3) == "DESC" { format!("{key} DESC") } else { key }
            })
            .collect();
        // A DESC key is an expression over one column, e.g. `SYS_EXTRACT_UTC("AT")`.
        let columns_used: Vec<String> = group
            .iter()
            .map(|r| match r.text(4) {
                Some(expr) if r.string(3) == "DESC" => quoted_names(&expr).into_iter().next().unwrap_or(expr),
                _ => r.string(2),
            })
            .collect();
        let primary = primary_index == Some(name.as_str());
        let index_owner = first.string(6);
        let index_ref = ObjectRef::new(NodeKind::Index, object.database.clone(), Some(index_owner), name.clone());
        let bitmap = if first.string(5).starts_with("BITMAP") { "BITMAP " } else { "" };
        let definition = format!(
            "CREATE {}{bitmap}INDEX {} ON {table} ({})",
            if unique { "UNIQUE " } else { "" },
            D.display_name(&index_ref),
            keys.join(", ")
        );
        out.push(Index {
            total: unique && plain && columns_used.iter().all(|c| not_null(c)),
            info: IndexInfo { name, definition, unique, primary, columns: columns_used, size_bytes: None },
        });
    }
    Ok(out)
}

/// The `"NAME"` identifiers inside an index expression.
fn quoted_names(expr: &str) -> Vec<String> {
    expr.split('"').skip(1).step_by(2).map(str::to_owned).collect()
}

/// Segment sizes of the table and its indexes. Other schemas need `DBA_SEGMENTS` access.
fn size(conn: &Connection, owner: &str, table: &str) -> Option<TableSize> {
    let sql = |view: &str, owner_filter: &str| {
        format!(
            "SELECT NVL(SUM(CASE WHEN s.segment_type LIKE 'TABLE%' THEN s.bytes END), 0), \
                    NVL(SUM(CASE WHEN s.segment_type LIKE 'INDEX%' THEN s.bytes END), 0) \
             FROM {view} s WHERE {owner_filter} (s.segment_name = :2 OR s.segment_name IN \
                (SELECT index_name FROM all_indexes WHERE table_owner = :1 AND table_name = :2))"
        )
    };
    let r = first_row(conn, &sql("user_segments", "USER = :1 AND"), &[&owner, &table])
        .ok()
        .flatten()
        .filter(|r| r.number(0).unwrap_or(0) > 0)
        .or_else(|| first_row(conn, &sql("dba_segments", "s.owner = :1 AND"), &[&owner, &table]).ok().flatten())?;
    let (t, i) = (r.number(0)?, r.number(1).unwrap_or(0));
    Some(TableSize { total_bytes: t + i, table_bytes: t, index_bytes: i })
}

pub fn structure(conn: &Connection, object: &ObjectRef) -> DbResult<TableStructure> {
    let owner = owner_of(object)?;
    let (kind, estimate, comment) = relation(conn, object)?;
    let columns: Vec<ColumnInfo> = columns(conn, object)?.into_iter().map(|c| c.info).collect();
    let all = table_constraints(conn, owner, &object.name)?;
    let primary_index = first_row(
        conn,
        "SELECT index_name FROM all_constraints WHERE owner = :1 AND table_name = :2 AND constraint_type = 'P'",
        &[&owner, &object.name],
    )?
    .and_then(|r| r.text(0));
    let indexes = indexes(conn, object, &columns, primary_index.as_deref())?;
    let list = |cols: &[String]| cols.iter().map(|c| D.display_ident(c)).collect::<Vec<_>>().join(", ");
    let mut constraints = Vec::new();
    let mut foreign_keys = Vec::new();
    for c in &all {
        match c.kind.as_str() {
            "P" => constraints.push(ConstraintInfo {
                kind: ConstraintKind::PrimaryKey,
                name: c.name.clone(),
                definition: format!("PRIMARY KEY ({})", list(&c.columns)),
            }),
            "U" => constraints.push(ConstraintInfo {
                kind: ConstraintKind::Unique,
                name: c.name.clone(),
                definition: format!("UNIQUE ({})", list(&c.columns)),
            }),
            "R" => {
                if let Some(fk) = foreign_key(conn, &object.database, owner, &object.name, c)? {
                    let target = if fk.target.schema == object.schema {
                        D.display_ident(&fk.target.name)
                    } else {
                        D.display_name(&fk.target)
                    };
                    let mut definition = format!(
                        "FOREIGN KEY ({}) REFERENCES {target} ({})",
                        list(&fk.columns),
                        list(&fk.target_columns)
                    );
                    if let Some(rule) = &fk.on_delete {
                        let _ = write!(definition, " ON DELETE {rule}");
                    }
                    constraints.push(ConstraintInfo {
                        kind: ConstraintKind::ForeignKey,
                        name: c.name.clone(),
                        definition,
                    });
                    foreign_keys.push(fk);
                }
            }
            _ if !is_not_null_check(c) => constraints.push(ConstraintInfo {
                kind: ConstraintKind::Check,
                name: c.name.clone(),
                definition: format!("CHECK ({})", c.condition.clone().unwrap_or_default()),
            }),
            _ => {}
        }
    }
    let row_identity = if kind == NodeKind::Table {
        all.iter()
            .find(|c| c.kind == "P")
            .map(|c| c.columns.clone())
            .or_else(|| indexes.iter().find(|i| i.total).map(|i| i.info.columns.clone()))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    Ok(TableStructure {
        object: ObjectRef { kind, ..object.clone() },
        columns,
        constraints,
        indexes: indexes.into_iter().map(|i| i.info).collect(),
        row_identity,
        full_row_key: Vec::new(),
        comment,
        row_estimate: if kind == NodeKind::View { None } else { estimate },
        foreign_keys,
        size: if kind == NodeKind::View { None } else { size(conn, owner, &object.name) },
    })
}

fn foreign_keys_where(
    conn: &Connection,
    database: &str,
    predicate: &str,
    params: &[&dyn ToSql],
) -> DbResult<Vec<ForeignKey>> {
    let found = rows(
        conn,
        &format!(
            "SELECT k.owner, k.table_name, k.constraint_name FROM all_constraints k \
             WHERE k.constraint_type = 'R' AND {predicate} ORDER BY k.owner, k.table_name, k.constraint_name"
        ),
        params,
    )?;
    let mut out = Vec::new();
    for r in &found {
        let (owner, table) = (r.string(0), r.string(1));
        for c in table_constraints(conn, &owner, &table)?.iter().filter(|c| c.name == r.string(2)) {
            out.extend(foreign_key(conn, database, &owner, &table, c)?);
        }
    }
    Ok(out)
}

pub fn dependencies(conn: &Connection, object: &ObjectRef) -> DbResult<Dependencies> {
    if !object.kind.is_relation() {
        return Ok(Dependencies::default());
    }
    let owner = owner_of(object)?;
    let db = &object.database;
    let mut deps = Dependencies::default();
    for fk in foreign_keys_where(conn, db, "k.owner = :1 AND k.table_name = :2", &[&owner, &object.name])? {
        deps.uses.push(Dependency { object: fk.target, relation: format!("foreign key {}", fk.name) });
    }
    for fk in foreign_keys_where(
        conn,
        db,
        "(k.r_owner, k.r_constraint_name) IN (SELECT owner, constraint_name FROM all_constraints \
          WHERE owner = :1 AND table_name = :2 AND constraint_type IN ('P', 'U'))",
        &[&owner, &object.name],
    )? {
        deps.used_by.push(Dependency { object: fk.source, relation: format!("foreign key {}", fk.name) });
    }
    let uses = rows(
        conn,
        "SELECT referenced_owner, referenced_name, referenced_type FROM all_dependencies \
         WHERE owner = :1 AND name = :2 AND referenced_type IN ('TABLE', 'VIEW', 'MATERIALIZED VIEW') \
           AND referenced_owner NOT IN ('SYS', 'PUBLIC') ORDER BY 1, 2",
        &[&owner, &object.name],
    )?;
    for r in &uses {
        let o = ObjectRef::new(kind_of(&r.string(2)), db.clone(), Some(r.string(0)), r.string(1));
        deps.uses.push(Dependency { object: o, relation: "reads".into() });
    }
    let used_by = rows(
        conn,
        "SELECT owner, name, type FROM all_dependencies \
         WHERE referenced_owner = :1 AND referenced_name = :2 AND type IN ('VIEW', 'MATERIALIZED VIEW', 'FUNCTION', 'PROCEDURE') \
         ORDER BY 1, 2",
        &[&owner, &object.name],
    )?;
    for r in &used_by {
        let kind = kind_of(&r.string(2));
        let relation = if kind.is_relation() { "view" } else { "reads" };
        deps.used_by.push(Dependency {
            object: ObjectRef::new(kind, db.clone(), Some(r.string(0)), r.string(1)),
            relation: relation.into(),
        });
    }
    let triggers = rows(
        conn,
        "SELECT trigger_name FROM all_triggers WHERE table_owner = :1 AND table_name = :2 ORDER BY 1",
        &[&owner, &object.name],
    )?;
    for r in &triggers {
        let o =
            ObjectRef::new(NodeKind::Trigger, db.clone(), object.schema.clone(), r.string(0)).with_parent(&object.name);
        deps.used_by.push(Dependency { object: o, relation: "trigger".into() });
    }
    Ok(deps)
}

pub fn schema_model(conn: &Connection, database: &str, schema: &str) -> DbResult<SchemaModel> {
    let found = rows(
        conn,
        &format!(
            "SELECT o.object_name, o.object_type, c.column_name, c.data_type, c.data_length, c.data_precision, \
                    c.data_scale, c.nullable, c.char_length, c.char_used, \
                    CASE WHEN EXISTS (SELECT 1 FROM all_constraints k JOIN all_cons_columns kc \
                           ON kc.owner = k.owner AND kc.constraint_name = k.constraint_name \
                         WHERE k.owner = c.owner AND k.table_name = c.table_name AND k.constraint_type = 'P' \
                           AND kc.column_name = c.column_name) THEN 'Y' ELSE 'N' END \
             FROM ({RELATION_SELECT} AND o.owner = :1) o \
             JOIN all_tab_columns c ON c.owner = o.owner AND c.table_name = o.object_name \
             ORDER BY o.object_name, c.column_id FETCH FIRST {} ROWS ONLY",
            SchemaModel::MAX_COLUMNS
        ),
        &[&schema],
    )?;
    let mut model = SchemaModel::default();
    for r in &found {
        let name = r.string(0);
        if model.tables.last().is_none_or(|t: &ModelTable| t.object.name != name) {
            let object = ObjectRef::new(kind_of(&r.string(1)), database, Some(schema.to_owned()), name);
            model.tables.push(ModelTable { object, columns: Vec::new() });
        }
        // Reuse the column type spelling: shift to its column layout.
        let spelled = TextRow(vec![
            r.text(2),
            r.text(3),
            r.text(4),
            r.text(5),
            r.text(6),
            None,
            None,
            None,
            None,
            None,
            r.text(8),
            r.text(9),
        ]);
        if let Some(table) = model.tables.last_mut() {
            table.columns.push(ModelColumn {
                name: r.string(2),
                data_type: type_spelling(&spelled),
                nullable: r.flag(7),
                primary_key: r.flag(10),
            });
        }
    }
    model.truncated = model.tables.len() > SchemaModel::MAX_TABLES || found.len() >= SchemaModel::MAX_COLUMNS;
    model.tables.truncate(SchemaModel::MAX_TABLES);
    model.foreign_keys = foreign_keys_where(conn, database, "k.owner = :1", &[&schema])?;
    Ok(model)
}

/// Definition from `DBMS_METADATA`, in its plain form (no storage clauses).
pub fn ddl(conn: &Connection, object: &ObjectRef) -> DbResult<String> {
    let owner = owner_of(object)?;
    let metadata_type = match object.kind {
        NodeKind::Table => "TABLE",
        NodeKind::View => "VIEW",
        NodeKind::MaterializedView => "MATERIALIZED_VIEW",
        NodeKind::Index => "INDEX",
        NodeKind::Sequence => "SEQUENCE",
        NodeKind::Trigger => "TRIGGER",
        NodeKind::Function => "FUNCTION",
        NodeKind::Procedure => "PROCEDURE",
        _ => return Err(DbError::invalid("This object has no definition.")),
    };
    let r =
        first_row(conn, "SELECT DBMS_METADATA.GET_DDL(:1, :2, :3) FROM DUAL", &[&metadata_type, &object.name, &owner])
            .map_err(|e| {
                if e.code.as_deref() == Some("ORA-31603") {
                    not_found(object)
                } else if e.kind == ErrorKind::Permission {
                    DbError::new(
                        ErrorKind::Permission,
                        format!("The definition of {} is not visible to this user.", object.name),
                    )
                } else {
                    e
                }
            })?;
    let mut text = r.and_then(|r| r.text(0)).ok_or_else(|| not_found(object))?.trim().to_owned();
    // PL/SQL units end with `/` so the script runs as written.
    if matches!(object.kind, NodeKind::Trigger | NodeKind::Function | NodeKind::Procedure) && !text.ends_with('/') {
        text.push_str("\n/");
    }
    text.push('\n');
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cols: &[Option<&str>]) -> TextRow {
        TextRow(cols.iter().map(|c| c.map(str::to_owned)).collect())
    }

    #[test]
    fn spells_types() {
        let t = |ty, len, p, s, cl, cu| {
            type_spelling(&row(&[Some("X"), Some(ty), len, p, s, None, None, None, None, None, cl, cu]))
        };
        assert_eq!(t("NUMBER", Some("22"), Some("12"), Some("2"), None, None), "NUMBER(12,2)");
        assert_eq!(t("NUMBER", Some("22"), Some("19"), Some("0"), None, None), "NUMBER(19)");
        assert_eq!(t("NUMBER", Some("22"), None, None, None, None), "NUMBER");
        assert_eq!(t("VARCHAR2", Some("200"), None, None, Some("50"), Some("C")), "VARCHAR2(50 CHAR)");
        assert_eq!(t("VARCHAR2", Some("20"), None, None, Some("20"), Some("B")), "VARCHAR2(20)");
        assert_eq!(
            t("TIMESTAMP(3) WITH TIME ZONE", Some("13"), None, Some("3"), None, None),
            "TIMESTAMP(3) WITH TIME ZONE"
        );
    }
}
