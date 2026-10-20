//! Catalog reads from `sqlite_master` and the `pragma_*` table functions.
//! Object names travel as parameters; a database (schema) name can only be
//! spliced in quoted, as `"name".sqlite_master`.
//!
//! The queries run on a [`CatalogSource`]: a local [`Connection`] here, or a
//! remote `SQLite` service (libSQL, Cloudflare D1) in `dq-sqlite-http`.

use std::collections::HashMap;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::result::TextRow;
use dq_core::schema::{
    ColumnInfo, ConstraintInfo, ConstraintKind, Dependencies, Dependency, ForeignKey, IndexInfo, ModelColumn,
    ModelTable, SchemaModel, TableStructure,
};
use dq_core::sql::{Dialect, SqliteDialect};
use dq_core::{DbError, DbResult};
use rusqlite::Connection;
use rusqlite::types::ValueRef;

use crate::error::map_error;
use crate::values::numeric_affinity;

const D: SqliteDialect = SqliteDialect;

/// Where catalog queries run. Every parameter is text.
pub trait CatalogSource {
    /// Runs a catalog query, reading every value as text.
    fn rows(&self, sql: &str, params: &[&str]) -> impl Future<Output = DbResult<Vec<TextRow>>>;

    /// Can `ATTACH` other databases. Remote services serve only `main`.
    fn attaches(&self) -> bool {
        true
    }

    /// `LIKE` prefixes (`\` escapes) of internal tables to hide, such as
    /// `sqlite_sequence` and `sqlite_stat1`.
    fn internal_prefixes(&self) -> &'static [&'static str] {
        &["sqlite\\_"]
    }
}

impl CatalogSource for Connection {
    fn rows(&self, sql: &str, params: &[&str]) -> impl Future<Output = DbResult<Vec<TextRow>>> {
        std::future::ready(local_rows(self, sql, params))
    }
}

/// Runs catalog code on a local connection. Its queries are ready at once,
/// so one poll completes it.
pub fn now<T>(future: impl Future<Output = T>) -> T {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(value) => value,
        Poll::Pending => unreachable!("local catalog reads never wait"),
    }
}

/// `column NOT LIKE 'sqlite\_%' ESCAPE '\' AND …`: hides internal tables.
fn user_objects(src: &impl CatalogSource, column: &str) -> String {
    src.internal_prefixes()
        .iter()
        .map(|prefix| format!("{column} NOT LIKE '{prefix}%' ESCAPE '\\'"))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// A value as catalog text: numbers in their usual spelling, BLOBs as UTF-8.
pub fn text_of(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(f.to_string()),
        ValueRef::Text(t) | ValueRef::Blob(t) => Some(String::from_utf8_lossy(t).into_owned()),
    }
}

fn local_rows(conn: &Connection, sql: &str, params: &[&str]) -> DbResult<Vec<TextRow>> {
    let mut statement = conn.prepare(sql).map_err(|e| map_error(&e))?;
    let width = statement.column_count();
    let mut found = statement.query(rusqlite::params_from_iter(params)).map_err(|e| map_error(&e))?;
    let mut out = Vec::new();
    while let Some(row) = found.next().map_err(|e| map_error(&e))? {
        out.push(TextRow((0..width).map(|i| row.get_ref(i).ok().and_then(text_of)).collect()));
    }
    Ok(out)
}

async fn first(src: &impl CatalogSource, sql: &str, params: &[&str]) -> DbResult<Option<TextRow>> {
    Ok(src.rows(sql, params).await?.into_iter().next())
}

/// `"db".sqlite_master`.
fn master(database: &str) -> String {
    format!("{}.sqlite_master", D.quote_ident(database))
}

fn not_found(object: &ObjectRef) -> DbError {
    DbError::not_found(format!("{} no longer exists.", D.display_name(object)))
}

/// Attached databases: `main`, `temp` when it holds objects, then the rest.
async fn database_names(src: &impl CatalogSource) -> DbResult<Vec<String>> {
    if !src.attaches() {
        return Ok(vec!["main".to_owned()]);
    }
    let names: Vec<String> = src
        .rows("SELECT name FROM pragma_database_list ORDER BY seq", &[])
        .await?
        .iter()
        .map(|r| r.string(0))
        .collect();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        if name == "temp" && relation_count(src, &name).await? == 0 {
            continue;
        }
        out.push(name);
    }
    Ok(out)
}

async fn relation_count(src: &impl CatalogSource, database: &str) -> DbResult<i64> {
    let sql = format!(
        "SELECT count(*) FROM {} WHERE type IN ('table', 'view') AND {}",
        master(database),
        user_objects(src, "name")
    );
    Ok(first(src, &sql, &[]).await?.and_then(|r| r.number(0)).unwrap_or(0))
}

pub async fn databases(src: &impl CatalogSource) -> DbResult<Vec<Node>> {
    let mut out = Vec::new();
    for name in database_names(src).await? {
        let mut node = Node::container(ObjectRef::new(NodeKind::Database, name.clone(), None, name.clone()));
        node.current = name == "main";
        node.row_estimate = Some(relation_count(src, &name).await?);
        out.push(node);
    }
    Ok(out)
}

/// The explorer's children of `parent`: databases at the top, then tables
/// and views, then a table's indexes and triggers.
pub async fn children(src: &impl CatalogSource, parent: Option<&ObjectRef>) -> DbResult<Vec<Node>> {
    match parent {
        None => databases(src).await,
        Some(p) if p.kind == NodeKind::Database => relations(src, &p.name).await,
        Some(p) if p.kind == NodeKind::Table => table_children(src, p).await,
        Some(_) => Ok(Vec::new()),
    }
}

/// Row counts from `ANALYZE` statistics, when the file has them.
/// Row estimates by table name (one per table: grouped by `tbl`).
async fn stat_estimates(src: &impl CatalogSource, database: &str) -> HashMap<String, i64> {
    let sql =
        format!("SELECT tbl, max(CAST(stat AS INTEGER)) FROM {}.sqlite_stat1 GROUP BY tbl", D.quote_ident(database));
    src.rows(&sql, &[])
        .await
        .map(|found| found.iter().filter_map(|r| Some((r.string(0), r.number(1)?))).collect())
        .unwrap_or_default()
}

fn relation_node(database: &str, name: String, kind: &str) -> Node {
    let kind = if kind == "view" { NodeKind::View } else { NodeKind::Table };
    let object = ObjectRef::new(kind, database, None, name);
    if kind == NodeKind::Table { Node::container(object) } else { Node::leaf(object) }
}

pub async fn relations(src: &impl CatalogSource, database: &str) -> DbResult<Vec<Node>> {
    let sql = format!(
        "SELECT name, type FROM {} WHERE type IN ('table', 'view') AND {} ORDER BY type, name",
        master(database),
        user_objects(src, "name")
    );
    let estimates = stat_estimates(src, database).await;
    Ok(src
        .rows(&sql, &[])
        .await?
        .iter()
        .map(|r| {
            let mut node = relation_node(database, r.string(0), &r.string(1));
            node.row_estimate = estimates.get(&node.object.name).copied();
            node
        })
        .collect())
}

/// Indexes and triggers of a table.
pub async fn table_children(src: &impl CatalogSource, table: &ObjectRef) -> DbResult<Vec<Node>> {
    let sql = format!(
        "SELECT name, type FROM {} WHERE tbl_name = ?1 AND type IN ('index', 'trigger') \
         AND name NOT LIKE 'sqlite\\_autoindex%' ESCAPE '\\' ORDER BY type, name",
        master(&table.database)
    );
    Ok(src
        .rows(&sql, &[&table.name])
        .await?
        .iter()
        .map(|r| {
            let kind = if r.string(1) == "index" { NodeKind::Index } else { NodeKind::Trigger };
            Node::leaf(ObjectRef::new(kind, table.database.clone(), None, r.string(0)).with_parent(&table.name))
        })
        .collect())
}

pub async fn search(src: &impl CatalogSource, query: &str, limit: u32) -> DbResult<Vec<Node>> {
    let pattern = dq_core::sql::builder::like_contains(query);
    let mut out = Vec::new();
    for database in database_names(src).await? {
        let sql = format!(
            "SELECT name, type FROM {} WHERE type IN ('table', 'view') AND {} \
             AND name LIKE ?1 ESCAPE '\\' ORDER BY length(name), name LIMIT {}",
            master(&database),
            user_objects(src, "name"),
            limit.min(1_000)
        );
        let found = src.rows(&sql, &[&pattern]).await?;
        out.extend(found.iter().map(|r| relation_node(&database, r.string(0), &r.string(1))));
    }
    out.truncate(limit as usize);
    Ok(out)
}

/// The `type` of a table or view, or not found.
async fn relation_kind(src: &impl CatalogSource, object: &ObjectRef) -> DbResult<NodeKind> {
    let sql = format!("SELECT type FROM {} WHERE name = ?1 AND type IN ('table', 'view')", master(&object.database));
    match first(src, &sql, &[&object.name]).await?.map(|r| r.string(0)) {
        Some(kind) if kind == "view" => Ok(NodeKind::View),
        Some(_) => Ok(NodeKind::Table),
        None => Err(not_found(object)),
    }
}

pub async fn row_estimate(src: &impl CatalogSource, object: &ObjectRef) -> Option<i64> {
    stat_estimates(src, &object.database).await.remove(&object.name)
}

async fn columns(src: &impl CatalogSource, object: &ObjectRef) -> DbResult<Vec<ColumnInfo>> {
    // `hidden`: 1 = hidden column of a virtual table, 2/3 = generated column.
    let found = src
        .rows(
            "SELECT name, type, \"notnull\", dflt_value, pk, hidden FROM pragma_table_xinfo(?1, ?2) WHERE hidden <> 1 ORDER BY cid",
            &[&object.name, &object.database],
        )
        .await?;
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

async fn primary_key(src: &impl CatalogSource, object: &ObjectRef) -> DbResult<Vec<String>> {
    Ok(src
        .rows("SELECT name FROM pragma_table_info(?1, ?2) WHERE pk > 0 ORDER BY pk", &[&object.name, &object.database])
        .await?
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

/// Indexes of a table with their columns, in one query.
async fn index_list(src: &impl CatalogSource, object: &ObjectRef) -> DbResult<Vec<IndexEntry>> {
    let found = src
        .rows(
            "SELECT l.name, l.\"unique\", l.origin, coalesce(i.name, '(expression)') \
             FROM pragma_index_list(?1, ?2) l JOIN pragma_index_info(l.name, ?2) i ORDER BY l.name, i.seqno",
            &[&object.name, &object.database],
        )
        .await?;
    let mut out: Vec<IndexEntry> = Vec::new();
    for r in &found {
        let name = r.string(0);
        if out.last().is_none_or(|e| e.name != name) {
            out.push(IndexEntry { name, unique: r.flag(1), origin: r.string(2), columns: Vec::new() });
        }
        if let Some(entry) = out.last_mut() {
            entry.columns.push(r.string(3));
        }
    }
    Ok(out)
}

/// Indexes created with `CREATE INDEX`. Those behind PRIMARY KEY and UNIQUE
/// constraints (`sqlite_autoindex_*`) are shown as the constraints instead.
async fn indexes(src: &impl CatalogSource, object: &ObjectRef, entries: &[IndexEntry]) -> DbResult<Vec<IndexInfo>> {
    let sql = format!("SELECT name, sql FROM {} WHERE type = 'index' AND tbl_name = ?1", master(&object.database));
    let definitions: HashMap<String, String> =
        src.rows(&sql, &[&object.name]).await?.into_iter().map(|r| (r.string(0), r.string(1))).collect();
    Ok(entries
        .iter()
        .filter(|e| e.origin == "c")
        .map(|e| IndexInfo {
            name: e.name.clone(),
            definition: definitions.get(&e.name).cloned().unwrap_or_default(),
            unique: e.unique,
            primary: false,
            columns: e.columns.clone(),
            size_bytes: None,
        })
        .collect())
}

/// Foreign keys from rows of `(table, id, "table", "from", "to", on_update, on_delete)`
/// ordered by table, id and seq.
async fn group_foreign_keys(src: &impl CatalogSource, database: &str, found: &[TextRow]) -> DbResult<Vec<ForeignKey>> {
    let mut out: Vec<(String, String, ForeignKey)> = Vec::new();
    for r in found {
        let (table, id) = (r.string(0), r.string(1));
        if out.last().is_none_or(|(t, last, _)| *t != table || *last != id) {
            out.push((
                table.clone(),
                id.clone(),
                ForeignKey {
                    name: format!("{table}_fk_{id}"),
                    source: ObjectRef::new(NodeKind::Table, database, None, table),
                    columns: Vec::new(),
                    target: ObjectRef::new(NodeKind::Table, database, None, r.string(2)),
                    target_columns: Vec::new(),
                    on_update: r.text(5).filter(|a| a != "NO ACTION"),
                    on_delete: r.text(6).filter(|a| a != "NO ACTION"),
                },
            ));
        }
        if let Some((_, _, fk)) = out.last_mut() {
            fk.columns.push(r.string(3));
            // A missing `to` means the target's primary key.
            fk.target_columns.push(r.text(4).unwrap_or_default());
        }
    }
    let mut keys: Vec<ForeignKey> = out.into_iter().map(|(_, _, fk)| fk).collect();
    for fk in &mut keys {
        if fk.target_columns.iter().any(String::is_empty) {
            fk.target_columns = primary_key(src, &fk.target).await?;
        }
    }
    Ok(keys)
}

/// Foreign keys declared on `table` in `database`.
async fn foreign_keys_of(src: &impl CatalogSource, database: &str, table: &str) -> DbResult<Vec<ForeignKey>> {
    let found = src
        .rows(
            "SELECT ?1, id, \"table\", \"from\", \"to\", on_update, on_delete FROM pragma_foreign_key_list(?1, ?2) ORDER BY id, seq",
            &[table, database],
        )
        .await?;
    group_foreign_keys(src, database, &found).await
}

/// Foreign keys of every table in `database`, in one query.
async fn all_foreign_keys(src: &impl CatalogSource, database: &str) -> DbResult<Vec<ForeignKey>> {
    let sql = format!(
        "SELECT m.name, f.id, f.\"table\", f.\"from\", f.\"to\", f.on_update, f.on_delete \
         FROM {} m JOIN pragma_foreign_key_list(m.name, ?1) f WHERE m.type = 'table' AND {} \
         ORDER BY m.name, f.id, f.seq",
        master(database),
        user_objects(src, "m.name")
    );
    let found = src.rows(&sql, &[database]).await?;
    group_foreign_keys(src, database, &found).await
}

pub async fn structure(src: &impl CatalogSource, object: &ObjectRef) -> DbResult<TableStructure> {
    let kind = relation_kind(src, object).await?;
    let object = ObjectRef { kind, ..object.clone() };
    let columns = columns(src, &object).await?;
    let list = |cols: &[String]| cols.iter().map(|c| D.display_ident(c)).collect::<Vec<_>>().join(", ");
    let (entries, foreign_keys) = if kind == NodeKind::Table {
        (index_list(src, &object).await?, foreign_keys_of(src, &object.database, &object.name).await?)
    } else {
        (Vec::new(), Vec::new())
    };
    let mut indexes = indexes(src, &object, &entries).await?;
    let primary: Vec<String> = columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
    let primary = if primary.len() > 1 { primary_key(src, &object).await? } else { primary };
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
        row_estimate: row_estimate(src, &object).await,
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

pub async fn dependencies(src: &impl CatalogSource, object: &ObjectRef) -> DbResult<Dependencies> {
    if !object.kind.is_relation() {
        return Ok(Dependencies::default());
    }
    let mut deps = Dependencies::default();
    if object.kind == NodeKind::Table {
        for fk in foreign_keys_of(src, &object.database, &object.name).await? {
            deps.uses.push(Dependency { relation: format!("foreign key {}", fk.name), object: fk.target });
        }
    }
    for fk in all_foreign_keys(src, &object.database).await?.into_iter().filter(|fk| fk.target.name == object.name) {
        deps.used_by.push(Dependency { relation: format!("foreign key {}", fk.name), object: fk.source });
    }
    // Views and triggers that mention the name: SQLite keeps no dependency list.
    let sql = format!(
        "SELECT name, type FROM {} WHERE type IN ('view', 'trigger') AND name <> ?1 \
         AND (' ' || lower(sql) || ' ') GLOB ('*[^a-z0-9_]' || lower(?1) || '[^a-z0-9_]*') ORDER BY type, name",
        master(&object.database)
    );
    for r in src.rows(&sql, &[&object.name]).await? {
        let (kind, relation) =
            if r.string(1) == "view" { (NodeKind::View, "view") } else { (NodeKind::Trigger, "trigger") };
        deps.used_by.push(Dependency {
            object: ObjectRef::new(kind, object.database.clone(), None, r.string(0)),
            relation: relation.into(),
        });
    }
    Ok(deps)
}

pub async fn schema_model(src: &impl CatalogSource, database: &str) -> DbResult<SchemaModel> {
    let found = src
        .rows(
            &format!(
                "SELECT m.name, m.type, p.name, p.type, p.\"notnull\", p.pk \
                 FROM {} m JOIN pragma_table_info(m.name, ?1) p \
                 WHERE m.type IN ('table', 'view') AND {} ORDER BY m.name, p.cid LIMIT {}",
                master(database),
                user_objects(src, "m.name"),
                SchemaModel::MAX_COLUMNS
            ),
            &[database],
        )
        .await?;
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
    model.foreign_keys = all_foreign_keys(src, database).await?;
    Ok(model)
}

/// Definition as `SQLite` keeps it in `sqlite_master`.
pub async fn ddl(src: &impl CatalogSource, object: &ObjectRef) -> DbResult<String> {
    let kind = match object.kind {
        NodeKind::Table => "table",
        NodeKind::View => "view",
        NodeKind::Index => "index",
        NodeKind::Trigger => "trigger",
        _ => return Err(DbError::invalid("This object has no definition.")),
    };
    let sql = format!("SELECT sql FROM {} WHERE type = ?1 AND name = ?2", master(&object.database));
    let mut text = first(src, &sql, &[kind, &object.name])
        .await?
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
        for r in src.rows(&extra, &[&object.name]).await? {
            text.push('\n');
            text.push_str(r.string(0).trim().trim_end_matches(';'));
            text.push_str(";\n");
        }
    }
    Ok(text)
}
