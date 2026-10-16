//! Catalog reads from `sqlite_master` and the `pragma_*` table functions.
//! Object names travel as parameters; a database (schema) name can only be
//! spliced in quoted, as `"name".sqlite_master`.

use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::result::TextRow;
use dq_core::schema::{
    ColumnInfo, ConstraintInfo, ConstraintKind, Dependencies, Dependency, ForeignKey, IndexInfo, ModelColumn,
    ModelTable, SchemaModel, TableStructure,
};
use dq_core::sql::{Dialect, SqliteDialect};
use dq_core::{DbError, DbResult};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, ToSql};

use crate::error::map_error;
use crate::values::numeric_affinity;

const D: SqliteDialect = SqliteDialect;
/// Internal tables (`sqlite_sequence`, `sqlite_stat1`, …) are hidden.
const USER_OBJECTS: &str = "name NOT LIKE 'sqlite\\_%' ESCAPE '\\'";

fn text_of(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(f.to_string()),
        ValueRef::Text(t) | ValueRef::Blob(t) => Some(String::from_utf8_lossy(t).into_owned()),
    }
}

/// Runs a catalog query, reading every value as text.
pub fn rows(conn: &Connection, sql: &str, params: &[&dyn ToSql]) -> DbResult<Vec<TextRow>> {
    let mut statement = conn.prepare(sql).map_err(|e| map_error(&e))?;
    let width = statement.column_count();
    let mut found = statement.query(params).map_err(|e| map_error(&e))?;
    let mut out = Vec::new();
    while let Some(row) = found.next().map_err(|e| map_error(&e))? {
        out.push(TextRow((0..width).map(|i| row.get_ref(i).ok().and_then(text_of)).collect()));
    }
    Ok(out)
}

fn first(conn: &Connection, sql: &str, params: &[&dyn ToSql]) -> DbResult<Option<TextRow>> {
    Ok(rows(conn, sql, params)?.into_iter().next())
}

/// `"db".sqlite_master`.
fn master(database: &str) -> String {
    format!("{}.sqlite_master", D.quote_ident(database))
}

fn not_found(object: &ObjectRef) -> DbError {
    DbError::not_found(format!("{} no longer exists.", D.display_name(object)))
}

/// Attached databases: `main`, `temp` when it holds objects, then the rest.
fn database_names(conn: &Connection) -> DbResult<Vec<String>> {
    let names: Vec<String> =
        rows(conn, "SELECT name FROM pragma_database_list ORDER BY seq", &[])?.iter().map(|r| r.string(0)).collect();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        if name == "temp" && relation_count(conn, &name)? == 0 {
            continue;
        }
        out.push(name);
    }
    Ok(out)
}

fn relation_count(conn: &Connection, database: &str) -> DbResult<i64> {
    let sql = format!("SELECT count(*) FROM {} WHERE type IN ('table', 'view') AND {USER_OBJECTS}", master(database));
    Ok(first(conn, &sql, &[])?.and_then(|r| r.number(0)).unwrap_or(0))
}

pub fn databases(conn: &Connection) -> DbResult<Vec<Node>> {
    database_names(conn)?
        .into_iter()
        .map(|name| {
            let mut node = Node::container(ObjectRef::new(NodeKind::Database, name.clone(), None, name.clone()));
            node.current = name == "main";
            node.row_estimate = Some(relation_count(conn, &name)?);
            Ok(node)
        })
        .collect()
}

/// Row counts from `ANALYZE` statistics, when the file has them.
fn stat_estimates(conn: &Connection, database: &str) -> Vec<(String, i64)> {
    let sql =
        format!("SELECT tbl, max(CAST(stat AS INTEGER)) FROM {}.sqlite_stat1 GROUP BY tbl", D.quote_ident(database));
    rows(conn, &sql, &[])
        .map(|found| found.iter().filter_map(|r| Some((r.string(0), r.number(1)?))).collect())
        .unwrap_or_default()
}

fn relation_node(database: &str, name: String, kind: &str) -> Node {
    let kind = if kind == "view" { NodeKind::View } else { NodeKind::Table };
    let object = ObjectRef::new(kind, database, None, name);
    if kind == NodeKind::Table { Node::container(object) } else { Node::leaf(object) }
}

pub fn relations(conn: &Connection, database: &str) -> DbResult<Vec<Node>> {
    let sql = format!(
        "SELECT name, type FROM {} WHERE type IN ('table', 'view') AND {USER_OBJECTS} ORDER BY type, name",
        master(database)
    );
    let estimates = stat_estimates(conn, database);
    Ok(rows(conn, &sql, &[])?
        .iter()
        .map(|r| {
            let mut node = relation_node(database, r.string(0), &r.string(1));
            node.row_estimate = estimates.iter().find(|(t, _)| *t == node.object.name).map(|(_, n)| *n);
            node
        })
        .collect())
}

/// Indexes and triggers of a table.
pub fn table_children(conn: &Connection, table: &ObjectRef) -> DbResult<Vec<Node>> {
    let sql = format!(
        "SELECT name, type FROM {} WHERE tbl_name = ?1 AND type IN ('index', 'trigger') \
         AND name NOT LIKE 'sqlite\\_autoindex%' ESCAPE '\\' ORDER BY type, name",
        master(&table.database)
    );
    Ok(rows(conn, &sql, &[&table.name])?
        .iter()
        .map(|r| {
            let kind = if r.string(1) == "index" { NodeKind::Index } else { NodeKind::Trigger };
            Node::leaf(ObjectRef::new(kind, table.database.clone(), None, r.string(0)).with_parent(&table.name))
        })
        .collect())
}

pub fn search(conn: &Connection, query: &str, limit: u32) -> DbResult<Vec<Node>> {
    let pattern = dq_core::sql::builder::like_contains(query);
    let mut out = Vec::new();
    for database in database_names(conn)? {
        let sql = format!(
            "SELECT name, type FROM {} WHERE type IN ('table', 'view') AND {USER_OBJECTS} \
             AND name LIKE ?1 ESCAPE '\\' ORDER BY length(name), name LIMIT {}",
            master(&database),
            limit.min(1_000)
        );
        out.extend(rows(conn, &sql, &[&pattern])?.iter().map(|r| relation_node(&database, r.string(0), &r.string(1))));
    }
    out.truncate(limit as usize);
    Ok(out)
}

/// The `type` of a table or view, or not found.
fn relation_kind(conn: &Connection, object: &ObjectRef) -> DbResult<NodeKind> {
    let sql = format!("SELECT type FROM {} WHERE name = ?1 AND type IN ('table', 'view')", master(&object.database));
    match first(conn, &sql, &[&object.name])?.map(|r| r.string(0)) {
        Some(kind) if kind == "view" => Ok(NodeKind::View),
        Some(_) => Ok(NodeKind::Table),
        None => Err(not_found(object)),
    }
}

pub fn row_estimate(conn: &Connection, object: &ObjectRef) -> Option<i64> {
    stat_estimates(conn, &object.database).into_iter().find(|(t, _)| *t == object.name).map(|(_, n)| n)
}

fn columns(conn: &Connection, object: &ObjectRef) -> DbResult<Vec<ColumnInfo>> {
    // `hidden`: 1 = hidden column of a virtual table, 2/3 = generated column.
    let found = rows(
        conn,
        "SELECT name, type, \"notnull\", dflt_value, pk, hidden FROM pragma_table_xinfo(?1, ?2) WHERE hidden <> 1 ORDER BY cid",
        &[&object.name, &object.database],
    )?;
    let pk_count = found.iter().filter(|r| r.number(4).unwrap_or(0) > 0).count();
    Ok(found
        .iter()
        .map(|r| {
            let data_type = r.string(1);
            let primary_key = r.number(4).unwrap_or(0) > 0;
            let generated = r.number(5).unwrap_or(0) >= 2;
            // A lone `INTEGER PRIMARY KEY` is the rowid: it numbers itself.
            let identity = primary_key && pk_count == 1 && data_type.eq_ignore_ascii_case("INTEGER");
            ColumnInfo {
                name: r.string(0),
                nullable: !r.flag(2) && !primary_key,
                default: r.text(3),
                primary_key,
                generated,
                identity,
                editable: !generated,
                numeric: numeric_affinity(&data_type),
                comment: None,
                read_as_text: false,
                data_type,
            }
        })
        .collect())
}

fn primary_key(conn: &Connection, object: &ObjectRef) -> DbResult<Vec<String>> {
    Ok(rows(
        conn,
        "SELECT name FROM pragma_table_info(?1, ?2) WHERE pk > 0 ORDER BY pk",
        &[&object.name, &object.database],
    )?
    .iter()
    .map(|r| r.string(0))
    .collect())
}

/// An entry of `pragma_index_list` with its columns.
struct IndexEntry {
    name: String,
    unique: bool,
    /// `c` (created), `u` (unique constraint) or `pk`.
    origin: String,
    columns: Vec<String>,
}

fn index_list(conn: &Connection, object: &ObjectRef) -> DbResult<Vec<IndexEntry>> {
    let list = rows(
        conn,
        "SELECT name, \"unique\", origin FROM pragma_index_list(?1, ?2) ORDER BY name",
        &[&object.name, &object.database],
    )?;
    list.iter()
        .map(|r| {
            let name = r.string(0);
            let columns = rows(
                conn,
                "SELECT coalesce(name, '(expression)') FROM pragma_index_info(?1, ?2) ORDER BY seqno",
                &[&name, &object.database],
            )?
            .iter()
            .map(|c| c.string(0))
            .collect();
            Ok(IndexEntry { name, unique: r.flag(1), origin: r.string(2), columns })
        })
        .collect()
}

/// Indexes created with `CREATE INDEX`. Those behind PRIMARY KEY and UNIQUE
/// constraints (`sqlite_autoindex_*`) are shown as the constraints instead.
fn indexes(conn: &Connection, object: &ObjectRef, entries: &[IndexEntry]) -> DbResult<Vec<IndexInfo>> {
    let sql_of = format!("SELECT sql FROM {} WHERE type = 'index' AND name = ?1", master(&object.database));
    entries
        .iter()
        .filter(|e| e.origin == "c")
        .map(|e| {
            let definition = first(conn, &sql_of, &[&e.name])?.and_then(|r| r.text(0)).unwrap_or_default();
            Ok(IndexInfo {
                name: e.name.clone(),
                definition,
                unique: e.unique,
                primary: false,
                columns: e.columns.clone(),
                size_bytes: None,
            })
        })
        .collect()
}

/// Foreign keys declared on `table` in `database`.
fn foreign_keys_of(conn: &Connection, database: &str, table: &str) -> DbResult<Vec<ForeignKey>> {
    let found = rows(
        conn,
        "SELECT id, \"table\", \"from\", \"to\", on_update, on_delete FROM pragma_foreign_key_list(?1, ?2) ORDER BY id, seq",
        &[&table, &database],
    )?;
    let mut out: Vec<(String, ForeignKey)> = Vec::new();
    for r in &found {
        let id = r.string(0);
        if out.last().is_none_or(|(last, _)| *last != id) {
            out.push((
                id.clone(),
                ForeignKey {
                    name: format!("{table}_fk_{id}"),
                    source: ObjectRef::new(NodeKind::Table, database, None, table),
                    columns: Vec::new(),
                    target: ObjectRef::new(NodeKind::Table, database, None, r.string(1)),
                    target_columns: Vec::new(),
                    on_update: r.text(4).filter(|a| a != "NO ACTION"),
                    on_delete: r.text(5).filter(|a| a != "NO ACTION"),
                },
            ));
        }
        if let Some((_, fk)) = out.last_mut() {
            fk.columns.push(r.string(2));
            // A missing `to` means the target's primary key.
            fk.target_columns.push(r.text(3).unwrap_or_default());
        }
    }
    let mut keys: Vec<ForeignKey> = out.into_iter().map(|(_, fk)| fk).collect();
    for fk in &mut keys {
        if fk.target_columns.iter().any(String::is_empty) {
            fk.target_columns = primary_key(conn, &fk.target)?;
        }
    }
    Ok(keys)
}

/// Foreign keys of every table in `database`.
fn all_foreign_keys(conn: &Connection, database: &str) -> DbResult<Vec<ForeignKey>> {
    let tables = rows(
        conn,
        &format!("SELECT name FROM {} WHERE type = 'table' AND {USER_OBJECTS} ORDER BY name", master(database)),
        &[],
    )?;
    let mut out = Vec::new();
    for t in &tables {
        out.extend(foreign_keys_of(conn, database, &t.string(0))?);
    }
    Ok(out)
}

pub fn structure(conn: &Connection, object: &ObjectRef) -> DbResult<TableStructure> {
    let kind = relation_kind(conn, object)?;
    let object = ObjectRef { kind, ..object.clone() };
    let columns = columns(conn, &object)?;
    let list = |cols: &[String]| cols.iter().map(|c| D.display_ident(c)).collect::<Vec<_>>().join(", ");
    let (entries, foreign_keys) = if kind == NodeKind::Table {
        (index_list(conn, &object)?, foreign_keys_of(conn, &object.database, &object.name)?)
    } else {
        (Vec::new(), Vec::new())
    };
    let mut indexes = indexes(conn, &object, &entries)?;
    let primary: Vec<String> = columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
    let primary = if primary.len() > 1 { primary_key(conn, &object)? } else { primary };
    let mut constraints = Vec::new();
    if !primary.is_empty() {
        let name = format!("{}_pkey", object.name);
        let definition = format!("PRIMARY KEY ({})", list(&primary));
        // Listed as an index too, like other engines: an `INTEGER PRIMARY KEY` is the rowid and has none.
        indexes.insert(
            0,
            IndexInfo {
                name: name.clone(),
                definition: definition.clone(),
                unique: true,
                primary: true,
                columns: primary.clone(),
                size_bytes: None,
            },
        );
        constraints.push(ConstraintInfo { kind: ConstraintKind::PrimaryKey, name, definition });
    }
    // `sqlite_autoindex_*` names are reserved, so unique constraints get a name of their own.
    for e in entries.iter().filter(|e| e.origin == "u") {
        constraints.push(ConstraintInfo {
            kind: ConstraintKind::Unique,
            name: format!("{}_{}_key", object.name, e.columns.join("_")),
            definition: format!("UNIQUE ({})", list(&e.columns)),
        });
    }
    for fk in &foreign_keys {
        constraints.push(ConstraintInfo {
            kind: ConstraintKind::ForeignKey,
            name: fk.name.clone(),
            definition: format!(
                "FOREIGN KEY ({}) REFERENCES {} ({})",
                list(&fk.columns),
                D.display_ident(&fk.target.name),
                list(&fk.target_columns)
            ),
        });
    }
    // Without a primary key, a unique index over NOT NULL columns identifies rows.
    let unique_key = entries
        .iter()
        .filter(|e| e.unique)
        .find(|e| e.columns.iter().all(|n| columns.iter().any(|c| &c.name == n && !c.nullable)))
        .map(|e| e.columns.clone());
    let row_identity = match kind {
        NodeKind::Table if primary.is_empty() => unique_key.unwrap_or_default(),
        NodeKind::Table => primary,
        _ => Vec::new(),
    };
    Ok(TableStructure {
        row_estimate: row_estimate(conn, &object),
        object,
        columns,
        constraints,
        indexes,
        row_identity,
        full_row_key: Vec::new(),
        comment: None,
        foreign_keys,
        size: None,
    })
}

pub fn dependencies(conn: &Connection, object: &ObjectRef) -> DbResult<Dependencies> {
    if !object.kind.is_relation() {
        return Ok(Dependencies::default());
    }
    let mut deps = Dependencies::default();
    if object.kind == NodeKind::Table {
        for fk in foreign_keys_of(conn, &object.database, &object.name)? {
            deps.uses.push(Dependency { relation: format!("foreign key {}", fk.name), object: fk.target });
        }
    }
    for fk in all_foreign_keys(conn, &object.database)?.into_iter().filter(|fk| fk.target.name == object.name) {
        deps.used_by.push(Dependency { relation: format!("foreign key {}", fk.name), object: fk.source });
    }
    // Views and triggers that mention the name: SQLite keeps no dependency list.
    let sql = format!(
        "SELECT name, type FROM {} WHERE type IN ('view', 'trigger') AND name <> ?1 \
         AND (' ' || lower(sql) || ' ') GLOB ('*[^a-z0-9_]' || lower(?1) || '[^a-z0-9_]*') ORDER BY type, name",
        master(&object.database)
    );
    for r in rows(conn, &sql, &[&object.name])? {
        let (kind, relation) =
            if r.string(1) == "view" { (NodeKind::View, "view") } else { (NodeKind::Trigger, "trigger") };
        deps.used_by.push(Dependency {
            object: ObjectRef::new(kind, object.database.clone(), None, r.string(0)),
            relation: relation.into(),
        });
    }
    Ok(deps)
}

pub fn schema_model(conn: &Connection, database: &str) -> DbResult<SchemaModel> {
    let found = rows(
        conn,
        &format!(
            "SELECT m.name, m.type, p.name, p.type, p.\"notnull\", p.pk \
             FROM {} m JOIN pragma_table_info(m.name, ?1) p \
             WHERE m.type IN ('table', 'view') AND m.{USER_OBJECTS} ORDER BY m.name, p.cid LIMIT {}",
            master(database),
            SchemaModel::MAX_COLUMNS
        ),
        &[&database],
    )?;
    let mut model = SchemaModel::default();
    for r in &found {
        let name = r.string(0);
        if model.tables.last().is_none_or(|t: &ModelTable| t.object.name != name) {
            let kind = if r.string(1) == "view" { NodeKind::View } else { NodeKind::Table };
            model.tables.push(ModelTable { object: ObjectRef::new(kind, database, None, name), columns: Vec::new() });
        }
        if let Some(table) = model.tables.last_mut() {
            let primary_key = r.number(5).unwrap_or(0) > 0;
            table.columns.push(ModelColumn {
                name: r.string(2),
                data_type: r.string(3),
                nullable: !r.flag(4) && !primary_key,
                primary_key,
            });
        }
    }
    model.truncated = model.tables.len() > SchemaModel::MAX_TABLES || found.len() >= SchemaModel::MAX_COLUMNS;
    model.tables.truncate(SchemaModel::MAX_TABLES);
    model.foreign_keys = all_foreign_keys(conn, database)?;
    Ok(model)
}

/// Definition as `SQLite` keeps it in `sqlite_master`.
pub fn ddl(conn: &Connection, object: &ObjectRef) -> DbResult<String> {
    let kind = match object.kind {
        NodeKind::Table => "table",
        NodeKind::View => "view",
        NodeKind::Index => "index",
        NodeKind::Trigger => "trigger",
        _ => return Err(DbError::invalid("This object has no definition.")),
    };
    let sql = format!("SELECT sql FROM {} WHERE type = ?1 AND name = ?2", master(&object.database));
    let mut text = first(conn, &sql, &[&kind, &object.name])?
        .and_then(|r| r.text(0))
        .ok_or_else(|| not_found(object))?
        .trim()
        .trim_end_matches(';')
        .to_owned();
    text.push_str(";\n");
    // A table's indexes and triggers follow it, as `.schema` prints them.
    if object.kind == NodeKind::Table {
        let extra = format!(
            "SELECT sql FROM {} WHERE tbl_name = ?1 AND type IN ('index', 'trigger') AND sql IS NOT NULL ORDER BY type, name",
            master(&object.database)
        );
        for r in rows(conn, &extra, &[&object.name])? {
            text.push('\n');
            text.push_str(r.string(0).trim().trim_end_matches(';'));
            text.push_str(";\n");
        }
    }
    Ok(text)
}
