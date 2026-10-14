//! Catalog reads from `DuckDB`'s `duckdb_*()` metadata functions. Names travel
//! as parameters; DDL comes from the `sql` column the catalog keeps.

use dq_core::node::{GROUP_FUNCTIONS, GROUP_SEQUENCES, Node, NodeKind, ObjectRef};
use dq_core::result::TextRow;
use dq_core::schema::{
    ColumnInfo, ConstraintInfo, ConstraintKind, Dependencies, Dependency, ForeignKey, IndexInfo, ModelColumn,
    ModelTable, SchemaModel, TableStructure,
};
use dq_core::sql::{Dialect, DuckdbDialect};
use dq_core::{DbError, DbResult};
use duckdb::{Connection, ToSql};

use crate::error::map_error;

const D: DuckdbDialect = DuckdbDialect;
/// Separates list items in text rows (lists are read with `array_to_string`).
const SEP: char = '\u{1f}';

/// Runs a catalog query; every selected value must already be text.
pub fn rows(conn: &Connection, sql: &str, params: &[&dyn ToSql]) -> DbResult<Vec<TextRow>> {
    let mut statement = conn.prepare(sql).map_err(|e| map_error(&e))?;
    let mut found = statement.query(params).map_err(|e| map_error(&e))?;
    let mut out = Vec::new();
    while let Some(row) = found.next().map_err(|e| map_error(&e))? {
        let width = row.as_ref().column_count();
        out.push(TextRow((0..width).map(|i| row.get::<_, Option<String>>(i).ok().flatten()).collect()));
    }
    Ok(out)
}

fn first(conn: &Connection, sql: &str, params: &[&dyn ToSql]) -> DbResult<Option<TextRow>> {
    Ok(rows(conn, sql, params)?.into_iter().next())
}

fn list(value: Option<String>) -> Vec<String> {
    value.filter(|v| !v.is_empty()).map(|v| v.split(SEP).map(str::to_owned).collect()).unwrap_or_default()
}

fn schema_of(object: &ObjectRef) -> DbResult<&str> {
    object.schema.as_deref().ok_or_else(|| DbError::invalid("A schema is required."))
}

fn not_found(object: &ObjectRef) -> DbError {
    DbError::not_found(format!("{} no longer exists.", D.display_name(object)))
}

pub fn current_database(conn: &Connection) -> DbResult<String> {
    Ok(first(conn, "SELECT current_database()", &[])?.map(|r| r.string(0)).unwrap_or_default())
}

pub fn databases(conn: &Connection, current: &str) -> DbResult<Vec<Node>> {
    let found = rows(
        conn,
        "SELECT database_name, CAST((SELECT count(*) FROM duckdb_tables() t WHERE t.database_name = d.database_name) AS VARCHAR) \
         FROM duckdb_databases() d WHERE NOT internal ORDER BY database_name <> current_database(), database_name",
        &[],
    )?;
    Ok(found
        .iter()
        .map(|r| {
            let name = r.string(0);
            let mut node = Node::container(ObjectRef::new(NodeKind::Database, name.clone(), None, name.clone()));
            node.current = name == current;
            node.row_estimate = r.number(1);
            node
        })
        .collect())
}

pub fn schemas(conn: &Connection, database: &str) -> DbResult<Vec<Node>> {
    let found = rows(
        conn,
        "SELECT schema_name, CAST((SELECT count(*) FROM duckdb_tables() t \
                WHERE t.database_name = s.database_name AND t.schema_name = s.schema_name) + \
               (SELECT count(*) FROM duckdb_views() v WHERE NOT v.internal \
                AND v.database_name = s.database_name AND v.schema_name = s.schema_name) AS VARCHAR), comment \
         FROM duckdb_schemas() s WHERE database_name = ? AND schema_name NOT IN ('information_schema', 'pg_catalog') \
         ORDER BY schema_name <> 'main', schema_name",
        &[&database],
    )?;
    Ok(found
        .iter()
        .map(|r| {
            let name = r.string(0);
            let mut node = Node::container(ObjectRef::new(NodeKind::Schema, database, Some(name.clone()), name));
            node.row_estimate = r.number(1);
            node.comment = r.text(2).filter(|c| !c.is_empty());
            node
        })
        .collect())
}

const RELATIONS: &str = "SELECT schema_name, table_name, 'table' AS kind, CAST(estimated_size AS VARCHAR) AS estimate, comment, \
       database_name FROM duckdb_tables() \
UNION ALL SELECT schema_name, view_name, 'view', NULL, comment, database_name FROM duckdb_views() WHERE NOT internal";

fn relation_node(database: &str, r: &TextRow) -> Node {
    let kind = if r.string(2) == "view" { NodeKind::View } else { NodeKind::Table };
    let object = ObjectRef::new(kind, database, Some(r.string(0)), r.string(1));
    let mut node = if kind == NodeKind::Table { Node::container(object) } else { Node::leaf(object) };
    node.row_estimate = r.number(3);
    node.comment = r.text(4).filter(|c| !c.is_empty());
    node
}

pub fn relations(conn: &Connection, database: &str, schema: &str) -> DbResult<Vec<Node>> {
    let found = rows(
        conn,
        &format!("SELECT * FROM ({RELATIONS}) WHERE database_name = ? AND schema_name = ? ORDER BY kind, table_name"),
        &[&database, &schema],
    )?;
    let mut nodes: Vec<Node> = found.iter().map(|r| relation_node(database, r)).collect();
    let counts = first(
        conn,
        "SELECT CAST((SELECT count(*) FROM duckdb_sequences() WHERE database_name = ? AND schema_name = ?) AS VARCHAR), \
                CAST((SELECT count(*) FROM duckdb_functions() WHERE NOT internal AND database_name = ? AND schema_name = ?) AS VARCHAR)",
        &[&database, &schema, &database, &schema],
    )?;
    for (i, group) in [GROUP_SEQUENCES, GROUP_FUNCTIONS].into_iter().enumerate() {
        let count = counts.as_ref().and_then(|r| r.number(i)).unwrap_or(0);
        if count > 0 {
            let mut node = Node::container(ObjectRef::new(NodeKind::Group, database, Some(schema.to_owned()), group));
            node.row_estimate = Some(count);
            nodes.push(node);
        }
    }
    Ok(nodes)
}

pub fn table_children(conn: &Connection, table: &ObjectRef) -> DbResult<Vec<Node>> {
    let schema = schema_of(table)?;
    let found = rows(
        conn,
        "SELECT index_name FROM duckdb_indexes() WHERE database_name = ? AND schema_name = ? AND table_name = ? ORDER BY index_name",
        &[&table.database, &schema, &table.name],
    )?;
    Ok(found
        .iter()
        .map(|r| Node::leaf(ObjectRef::new(NodeKind::Index, table.database.clone(), table.schema.clone(), r.string(0)).with_parent(&table.name)))
        .collect())
}

pub fn group_children(conn: &Connection, group: &ObjectRef) -> DbResult<Vec<Node>> {
    let schema = schema_of(group)?;
    let (sql, kind) = match group.name.as_str() {
        GROUP_SEQUENCES => (
            "SELECT sequence_name, comment FROM duckdb_sequences() WHERE database_name = ? AND schema_name = ? ORDER BY 1",
            NodeKind::Sequence,
        ),
        GROUP_FUNCTIONS => (
            "SELECT DISTINCT function_name, NULL FROM duckdb_functions() WHERE NOT internal AND database_name = ? \
             AND schema_name = ? ORDER BY 1",
            NodeKind::Function,
        ),
        other => return Err(DbError::invalid(format!("Unknown group `{other}`."))),
    };
    Ok(rows(conn, sql, &[&group.database, &schema])?
        .iter()
        .map(|r| {
            let mut node = Node::leaf(ObjectRef::new(kind, group.database.clone(), group.schema.clone(), r.string(0)));
            node.comment = r.text(1).filter(|c| !c.is_empty());
            node
        })
        .collect())
}

pub fn search(conn: &Connection, query: &str, limit: u32) -> DbResult<Vec<Node>> {
    let pattern = dq_core::sql::builder::like_contains(query);
    let found = rows(
        conn,
        &format!(
            "SELECT * FROM ({RELATIONS}) WHERE table_name ILIKE ? ESCAPE '\\' ORDER BY length(table_name), table_name LIMIT {}",
            limit.min(1_000)
        ),
        &[&pattern],
    )?;
    Ok(found.iter().map(|r| relation_node(&r.string(5), r)).collect())
}

fn relation(conn: &Connection, object: &ObjectRef) -> DbResult<TextRow> {
    let schema = schema_of(object)?;
    first(
        conn,
        &format!("SELECT * FROM ({RELATIONS}) WHERE database_name = ? AND schema_name = ? AND table_name = ?"),
        &[&object.database, &schema, &object.name],
    )?
    .ok_or_else(|| not_found(object))
}

pub fn row_estimate(conn: &Connection, object: &ObjectRef) -> DbResult<Option<i64>> {
    Ok(relation(conn, object)?.number(3))
}

const NUMERIC: &[&str] = &[
    "TINYINT", "SMALLINT", "INTEGER", "BIGINT", "HUGEINT", "UTINYINT", "USMALLINT", "UINTEGER", "UBIGINT", "UHUGEINT",
    "FLOAT", "DOUBLE", "DECIMAL", "REAL",
];

/// A constraint of a table: kind, columns, check text, referenced table and columns.
struct Constraint {
    kind: String,
    name: String,
    columns: Vec<String>,
    text: String,
    referenced_table: Option<String>,
    referenced_columns: Vec<String>,
}

fn constraints(conn: &Connection, object: &ObjectRef) -> DbResult<Vec<Constraint>> {
    let schema = schema_of(object)?;
    let found = rows(
        conn,
        &format!(
            "SELECT constraint_type, constraint_name, array_to_string(constraint_column_names, chr(31)), constraint_text, \
                    referenced_table, array_to_string(referenced_column_names, chr(31)) \
             FROM duckdb_constraints() WHERE database_name = ? AND schema_name = ? AND table_name = ? \
               AND constraint_type <> 'NOT NULL' ORDER BY constraint_index"
        ),
        &[&object.database, &schema, &object.name],
    )?;
    Ok(found
        .iter()
        .map(|r| Constraint {
            kind: r.string(0),
            name: r.text(1).unwrap_or_else(|| format!("{}_{}", object.name, r.string(0).to_lowercase().replace(' ', "_"))),
            columns: list(r.text(2)),
            text: r.string(3),
            referenced_table: r.text(4),
            referenced_columns: list(r.text(5)),
        })
        .collect())
}

fn columns(conn: &Connection, object: &ObjectRef, primary: &[String]) -> DbResult<Vec<ColumnInfo>> {
    let schema = schema_of(object)?;
    let found = rows(
        conn,
        "SELECT column_name, data_type, CAST(is_nullable AS VARCHAR), column_default, comment \
         FROM duckdb_columns() WHERE database_name = ? AND schema_name = ? AND table_name = ? ORDER BY column_index",
        &[&object.database, &schema, &object.name],
    )?;
    Ok(found
        .iter()
        .map(|r| {
            let data_type = r.string(1);
            let base = data_type.split('(').next().unwrap_or(&data_type).trim().to_ascii_uppercase();
            let name = r.string(0);
            let generated = r.text(3).is_some_and(|d| d.starts_with("nextval("));
            ColumnInfo {
                primary_key: primary.contains(&name),
                name,
                nullable: r.flag(2),
                default: r.text(3),
                generated,
                identity: generated,
                editable: base != "BLOB" && !data_type.ends_with("[]") && !["STRUCT", "MAP", "UNION"].contains(&base.as_str()),
                numeric: NUMERIC.contains(&base.as_str()),
                comment: r.text(4).filter(|c| !c.is_empty()),
                read_as_text: false,
                data_type,
            }
        })
        .collect())
}

fn indexes(conn: &Connection, object: &ObjectRef) -> DbResult<Vec<IndexInfo>> {
    let schema = schema_of(object)?;
    let found = rows(
        conn,
        "SELECT index_name, CAST(is_unique AS VARCHAR), CAST(is_primary AS VARCHAR), sql, expressions \
         FROM duckdb_indexes() WHERE database_name = ? AND schema_name = ? AND table_name = ? ORDER BY index_name",
        &[&object.database, &schema, &object.name],
    )?;
    Ok(found
        .iter()
        .map(|r| {
            // `expressions` is a list literal of quoted column names: ['"a"', '"b"'].
            let columns = r
                .string(4)
                .trim_matches(['[', ']'])
                .split(',')
                .map(|c| c.trim().trim_matches(['\'', '"']).to_owned())
                .filter(|c| !c.is_empty())
                .collect();
            IndexInfo {
                name: r.string(0),
                definition: r.string(3).trim_end_matches(';').to_owned(),
                unique: r.flag(1),
                primary: r.flag(2),
                columns,
                size_bytes: None,
            }
        })
        .collect())
}

fn table_ref(database: &str, schema: &str, name: &str) -> ObjectRef {
    ObjectRef::new(NodeKind::Table, database, Some(schema.to_owned()), name)
}

/// Foreign keys whose source matches `filter` (over `duckdb_constraints()`).
fn foreign_keys(conn: &Connection, filter: &str, params: &[&dyn ToSql]) -> DbResult<Vec<ForeignKey>> {
    let found = rows(
        conn,
        &format!(
            "SELECT database_name, schema_name, table_name, constraint_name, array_to_string(constraint_column_names, chr(31)), \
                    referenced_table, array_to_string(referenced_column_names, chr(31)) \
             FROM duckdb_constraints() WHERE constraint_type = 'FOREIGN KEY' AND {filter} ORDER BY schema_name, table_name"
        ),
        params,
    )?;
    Ok(found
        .iter()
        .map(|r| ForeignKey {
            name: r.text(3).unwrap_or_else(|| format!("{}_fkey", r.string(2))),
            source: table_ref(&r.string(0), &r.string(1), &r.string(2)),
            columns: list(r.text(4)),
            // DuckDB references tables of the same schema.
            target: table_ref(&r.string(0), &r.string(1), &r.string(5)),
            target_columns: list(r.text(6)),
            on_update: None,
            on_delete: None,
        })
        .collect())
}

pub fn structure(conn: &Connection, object: &ObjectRef) -> DbResult<TableStructure> {
    let rel = relation(conn, object)?;
    let kind = if rel.string(2) == "view" { NodeKind::View } else { NodeKind::Table };
    let all = constraints(conn, object)?;
    let primary: Vec<String> = all.iter().find(|c| c.kind == "PRIMARY KEY").map(|c| c.columns.clone()).unwrap_or_default();
    let columns = columns(conn, object, &primary)?;
    let list_of = |cols: &[String]| cols.iter().map(|c| D.display_ident(c)).collect::<Vec<_>>().join(", ");
    let mut constraint_infos = Vec::new();
    for c in &all {
        let (kind, definition) = match c.kind.as_str() {
            "PRIMARY KEY" => (ConstraintKind::PrimaryKey, format!("PRIMARY KEY ({})", list_of(&c.columns))),
            "UNIQUE" => (ConstraintKind::Unique, format!("UNIQUE ({})", list_of(&c.columns))),
            "FOREIGN KEY" => (
                ConstraintKind::ForeignKey,
                format!(
                    "FOREIGN KEY ({}) REFERENCES {} ({})",
                    list_of(&c.columns),
                    D.display_ident(c.referenced_table.as_deref().unwrap_or_default()),
                    list_of(&c.referenced_columns)
                ),
            ),
            "CHECK" => (ConstraintKind::Check, c.text.clone()),
            _ => continue,
        };
        constraint_infos.push(ConstraintInfo { kind, name: c.name.clone(), definition });
    }
    let unique_key = all
        .iter()
        .filter(|c| c.kind == "UNIQUE")
        .find(|c| c.columns.iter().all(|n| columns.iter().any(|col| &col.name == n && !col.nullable)))
        .map(|c| c.columns.clone());
    let row_identity =
        if kind == NodeKind::Table { if primary.is_empty() { unique_key.unwrap_or_default() } else { primary } } else { Vec::new() };
    let schema = schema_of(object)?;
    let foreign_keys = foreign_keys(conn, "database_name = ? AND schema_name = ? AND table_name = ?", &[&object.database, &schema, &object.name])?;
    Ok(TableStructure {
        object: ObjectRef { kind, ..object.clone() },
        columns,
        constraints: constraint_infos,
        indexes: if kind == NodeKind::Table { indexes(conn, object)? } else { Vec::new() },
        row_identity,
        full_row_key: Vec::new(),
        comment: rel.text(4).filter(|c| !c.is_empty()),
        row_estimate: rel.number(3),
        foreign_keys,
        size: None,
    })
}

pub fn dependencies(conn: &Connection, object: &ObjectRef) -> DbResult<Dependencies> {
    if !object.kind.is_relation() {
        return Ok(Dependencies::default());
    }
    let schema = schema_of(object)?;
    let mut deps = Dependencies::default();
    for fk in foreign_keys(conn, "database_name = ? AND schema_name = ? AND table_name = ?", &[&object.database, &schema, &object.name])? {
        deps.uses.push(Dependency { object: fk.target, relation: format!("foreign key {}", fk.name) });
    }
    for fk in foreign_keys(conn, "database_name = ? AND schema_name = ? AND referenced_table = ?", &[&object.database, &schema, &object.name])? {
        deps.used_by.push(Dependency { object: fk.source, relation: format!("foreign key {}", fk.name) });
    }
    // Views that mention the name in their definition: DuckDB keeps no view dependencies.
    let views = rows(
        conn,
        "SELECT schema_name, view_name FROM duckdb_views() WHERE NOT internal AND database_name = ? \
           AND regexp_matches(sql, '\\b' || ? || '\\b', 'i') AND view_name <> ? ORDER BY 1, 2",
        &[&object.database, &object.name, &object.name],
    )?;
    for r in &views {
        deps.used_by.push(Dependency {
            object: ObjectRef::new(NodeKind::View, object.database.clone(), Some(r.string(0)), r.string(1)),
            relation: "view".into(),
        });
    }
    Ok(deps)
}

pub fn schema_model(conn: &Connection, database: &str, schema: Option<&str>) -> DbResult<SchemaModel> {
    let filter = if schema.is_some() { "AND c.schema_name = ?" } else { "AND c.schema_name NOT IN ('information_schema', 'pg_catalog')" };
    let mut params: Vec<&dyn ToSql> = vec![&database];
    if let Some(s) = &schema {
        params.push(s);
    }
    let found = rows(
        conn,
        &format!(
            "SELECT c.schema_name, c.table_name, c.column_name, c.data_type, CAST(c.is_nullable AS VARCHAR), \
                    CAST(EXISTS (SELECT 1 FROM duckdb_constraints() k WHERE k.constraint_type = 'PRIMARY KEY' \
                      AND k.database_name = c.database_name AND k.schema_name = c.schema_name AND k.table_name = c.table_name \
                      AND list_contains(k.constraint_column_names, c.column_name)) AS VARCHAR), \
                    CASE WHEN EXISTS (SELECT 1 FROM duckdb_views() v WHERE v.database_name = c.database_name \
                      AND v.schema_name = c.schema_name AND v.view_name = c.table_name) THEN 'view' ELSE 'table' END \
             FROM duckdb_columns() c WHERE c.database_name = ? {filter} \
             ORDER BY c.schema_name, c.table_name, c.column_index LIMIT {}",
            SchemaModel::MAX_COLUMNS
        ),
        &params,
    )?;
    let mut model = SchemaModel::default();
    for r in &found {
        let (s, name) = (r.string(0), r.string(1));
        if model.tables.last().is_none_or(|t: &ModelTable| t.object.name != name || t.object.schema.as_deref() != Some(&s)) {
            let kind = if r.string(6) == "view" { NodeKind::View } else { NodeKind::Table };
            model.tables.push(ModelTable { object: ObjectRef::new(kind, database, Some(s), name), columns: Vec::new() });
        }
        if let Some(table) = model.tables.last_mut() {
            table.columns.push(ModelColumn { name: r.string(2), data_type: r.string(3), nullable: r.flag(4), primary_key: r.flag(5) });
        }
    }
    model.truncated = model.tables.len() > SchemaModel::MAX_TABLES || found.len() >= SchemaModel::MAX_COLUMNS;
    model.tables.truncate(SchemaModel::MAX_TABLES);
    model.foreign_keys = match schema {
        Some(s) => foreign_keys(conn, "database_name = ? AND schema_name = ?", &[&database, &s])?,
        None => foreign_keys(conn, "database_name = ?", &[&database])?,
    };
    Ok(model)
}

/// Definition as `DuckDB` keeps it.
pub fn ddl(conn: &Connection, object: &ObjectRef) -> DbResult<String> {
    let schema = schema_of(object)?;
    let sql = match object.kind {
        NodeKind::Table => "SELECT sql FROM duckdb_tables() WHERE database_name = ? AND schema_name = ? AND table_name = ?",
        NodeKind::View => "SELECT sql FROM duckdb_views() WHERE database_name = ? AND schema_name = ? AND view_name = ?",
        NodeKind::Index => "SELECT sql FROM duckdb_indexes() WHERE database_name = ? AND schema_name = ? AND index_name = ?",
        NodeKind::Sequence => "SELECT sql FROM duckdb_sequences() WHERE database_name = ? AND schema_name = ? AND sequence_name = ?",
        NodeKind::Function => {
            "SELECT 'CREATE MACRO ' || function_name || '(' || array_to_string(parameters, ', ') || ') AS ' || macro_definition \
             FROM duckdb_functions() WHERE database_name = ? AND schema_name = ? AND function_name = ? LIMIT 1"
        }
        _ => return Err(DbError::invalid("This object has no definition.")),
    };
    let text = first(conn, sql, &[&object.database, &schema, &object.name])?.and_then(|r| r.text(0)).ok_or_else(|| not_found(object))?;
    let text = text.trim().trim_end_matches(';');
    Ok(format!("{text};\n"))
}
