//! Catalog reads from the `system` tables. Names travel as `{name:String}`
//! parameters; DDL comes from `create_table_query`.

use dq_core::node::{GROUP_FUNCTIONS, Node, NodeKind, ObjectRef};
use dq_core::schema::{
    ColumnInfo, Dependencies, Dependency, IndexInfo, ModelColumn, ModelTable, SchemaModel, TableSize, TableStructure,
};
use dq_core::sql::{ClickhouseDialect, Dialect};
use dq_core::{DbError, DbResult};

use crate::client::{Client, Request};
use crate::values::{base_type, is_numeric};

const D: ClickhouseDialect = ClickhouseDialect;

/// Databases the server keeps for itself, hidden unless current.
const SYSTEM_DATABASES: &str = "('system', 'INFORMATION_SCHEMA', 'information_schema')";

type TextRow = Vec<Option<String>>;

fn text(row: &TextRow, i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

fn opt(row: &TextRow, i: usize) -> Option<String> {
    row.get(i).cloned().flatten().filter(|v| !v.is_empty())
}

fn number(row: &TextRow, i: usize) -> Option<i64> {
    opt(row, i).and_then(|v| v.parse().ok())
}

fn flag(row: &TextRow, i: usize) -> bool {
    matches!(opt(row, i).as_deref(), Some("1" | "true"))
}

fn params(pairs: &[(&str, &str)]) -> Vec<(String, Option<String>)> {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), Some((*v).to_owned()))).collect()
}

async fn rows(client: &Client, sql: &str, pairs: &[(&str, &str)]) -> DbResult<Vec<TextRow>> {
    client.text_rows(sql, &Request { read_only: true, timeout_secs: 60, params: params(pairs), ..Request::default() }).await
}

fn kind_of(engine: &str) -> NodeKind {
    match engine {
        "View" | "LiveView" | "WindowView" => NodeKind::View,
        "MaterializedView" => NodeKind::MaterializedView,
        _ => NodeKind::Table,
    }
}

fn not_found(object: &ObjectRef) -> DbError {
    DbError::not_found(format!("{} no longer exists.", D.display_name(object)))
}

pub async fn databases(client: &Client, current: &str) -> DbResult<Vec<Node>> {
    let found = rows(
        client,
        &format!(
            "SELECT name, toString((SELECT count() FROM system.tables t WHERE t.database = d.name)), comment \
             FROM system.databases d WHERE name NOT IN {SYSTEM_DATABASES} OR name = {{current:String}} ORDER BY name"
        ),
        &[("current", current)],
    )
    .await?;
    Ok(found
        .iter()
        .map(|r| {
            let name = text(r, 0);
            let mut node = Node::container(ObjectRef::new(NodeKind::Database, name.clone(), None, name.clone()));
            node.current = name == current;
            node.row_estimate = number(r, 1);
            node.comment = opt(r, 2);
            node
        })
        .collect())
}

const RELATIONS: &str = "SELECT database, name, engine, toString(total_rows), comment FROM system.tables \
WHERE NOT is_temporary AND name NOT LIKE '.inner%'";

fn relation_node(r: &TextRow) -> Node {
    let kind = kind_of(&text(r, 2));
    let object = ObjectRef::new(kind, text(r, 0), None, text(r, 1));
    let mut node = if kind == NodeKind::Table { Node::container(object) } else { Node::leaf(object) };
    node.row_estimate = number(r, 3);
    node.comment = opt(r, 4);
    node
}

pub async fn relations(client: &Client, database: &str) -> DbResult<Vec<Node>> {
    let found = rows(
        client,
        &format!("{RELATIONS} AND database = {{db:String}} ORDER BY engine LIKE '%View', name"),
        &[("db", database)],
    )
    .await?;
    let mut nodes: Vec<Node> = found.iter().map(relation_node).collect();
    // User-defined SQL functions are global; they are listed with the current database.
    let functions = rows(client, "SELECT toString(count()) FROM system.functions WHERE origin = 'SQLUserDefined'", &[]).await?;
    let count = functions.first().and_then(|r| number(r, 0)).unwrap_or(0);
    if count > 0 {
        let mut node = Node::container(ObjectRef::new(NodeKind::Group, database, None, GROUP_FUNCTIONS));
        node.row_estimate = Some(count);
        nodes.push(node);
    }
    Ok(nodes)
}

/// Data-skipping indexes of a table.
pub async fn table_children(client: &Client, table: &ObjectRef) -> DbResult<Vec<Node>> {
    let found = rows(
        client,
        "SELECT name FROM system.data_skipping_indices WHERE database = {db:String} AND table = {t:String} ORDER BY name",
        &[("db", &table.database), ("t", &table.name)],
    )
    .await?;
    Ok(found
        .iter()
        .map(|r| Node::leaf(ObjectRef::new(NodeKind::Index, table.database.clone(), None, text(r, 0)).with_parent(&table.name)))
        .collect())
}

pub async fn group_children(client: &Client, group: &ObjectRef) -> DbResult<Vec<Node>> {
    if group.name != GROUP_FUNCTIONS {
        return Err(DbError::invalid(format!("Unknown group `{}`.", group.name)));
    }
    let found = rows(client, "SELECT name FROM system.functions WHERE origin = 'SQLUserDefined' ORDER BY name", &[]).await?;
    Ok(found.iter().map(|r| Node::leaf(ObjectRef::new(NodeKind::Function, group.database.clone(), None, text(r, 0)))).collect())
}

pub async fn search(client: &Client, query: &str, limit: u32) -> DbResult<Vec<Node>> {
    let pattern = dq_core::sql::builder::like_contains(query);
    let found = rows(
        client,
        &format!(
            "{RELATIONS} AND name ILIKE {{p:String}} AND database NOT IN {SYSTEM_DATABASES} \
             ORDER BY length(name), name LIMIT {}",
            limit.min(1_000)
        ),
        &[("p", &pattern)],
    )
    .await?;
    Ok(found.iter().map(relation_node).collect())
}

async fn relation(client: &Client, object: &ObjectRef) -> DbResult<TextRow> {
    rows(client, &format!("{RELATIONS} AND database = {{db:String}} AND name = {{t:String}}"), &[("db", &object.database), ("t", &object.name)])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| not_found(object))
}

pub async fn row_estimate(client: &Client, object: &ObjectRef) -> DbResult<Option<i64>> {
    Ok(number(&relation(client, object).await?, 3))
}

/// Types the grid can show but not filter by equality.
fn read_as_text(t: &str) -> bool {
    let base = base_type(t);
    base.starts_with("AggregateFunction") || base.starts_with("SimpleAggregateFunction") || base.starts_with("Object")
}

async fn columns(client: &Client, object: &ObjectRef) -> DbResult<(Vec<ColumnInfo>, Vec<String>, Vec<String>)> {
    let found = rows(
        client,
        "SELECT name, type, default_kind, default_expression, comment, toString(is_in_primary_key), toString(is_in_sorting_key) \
         FROM system.columns WHERE database = {db:String} AND table = {t:String} ORDER BY position",
        &[("db", &object.database), ("t", &object.name)],
    )
    .await?;
    let mut primary = Vec::new();
    let mut sorting = Vec::new();
    let columns = found
        .iter()
        .map(|r| {
            let name = text(r, 0);
            let data_type = text(r, 1);
            let default_kind = text(r, 2);
            if flag(r, 5) {
                primary.push(name.clone());
            }
            if flag(r, 6) {
                sorting.push(name.clone());
            }
            ColumnInfo {
                nullable: data_type.starts_with("Nullable(") || data_type.contains("(Nullable("),
                default: opt(r, 3).filter(|_| default_kind == "DEFAULT"),
                primary_key: flag(r, 5),
                // MATERIALIZED and ALIAS columns are computed by the server.
                generated: matches!(default_kind.as_str(), "MATERIALIZED" | "ALIAS" | "EPHEMERAL"),
                identity: false,
                editable: false,
                numeric: is_numeric(&data_type),
                comment: opt(r, 4),
                read_as_text: read_as_text(&data_type),
                name,
                data_type,
            }
        })
        .collect();
    Ok((columns, primary, sorting))
}

pub async fn structure(client: &Client, object: &ObjectRef) -> DbResult<TableStructure> {
    let rel = relation(client, object).await?;
    let kind = kind_of(&text(&rel, 2));
    let (columns, primary, sorting) = columns(client, object).await?;
    let list = |cols: &[String]| cols.iter().map(|c| D.display_ident(c)).collect::<Vec<_>>().join(", ");
    let mut indexes = Vec::new();
    if !primary.is_empty() {
        indexes.push(IndexInfo {
            name: "primary key".into(),
            definition: format!("PRIMARY KEY ({})", list(&primary)),
            unique: false,
            primary: true,
            columns: primary.clone(),
            size_bytes: None,
        });
    }
    if !sorting.is_empty() && sorting != primary {
        indexes.push(IndexInfo {
            name: "sorting key".into(),
            definition: format!("ORDER BY ({})", list(&sorting)),
            unique: false,
            primary: false,
            columns: sorting,
            size_bytes: None,
        });
    }
    let skipping = rows(
        client,
        "SELECT name, type_full, expr, toString(granularity), toString(data_compressed_bytes) FROM system.data_skipping_indices \
         WHERE database = {db:String} AND table = {t:String} ORDER BY name",
        &[("db", &object.database), ("t", &object.name)],
    )
    .await?;
    for r in &skipping {
        indexes.push(IndexInfo {
            name: text(r, 0),
            definition: format!("INDEX {} {} TYPE {} GRANULARITY {}", D.display_ident(&text(r, 0)), text(r, 2), text(r, 1), text(r, 3)),
            unique: false,
            primary: false,
            columns: Vec::new(),
            size_bytes: number(r, 4),
        });
    }
    let size = if kind == NodeKind::Table {
        rows(
            client,
            "SELECT toString(sum(bytes_on_disk)), toString(sum(data_compressed_bytes)), toString(sum(primary_key_bytes_in_memory)) \
             FROM system.parts WHERE active AND database = {db:String} AND table = {t:String}",
            &[("db", &object.database), ("t", &object.name)],
        )
        .await
        .ok()
        .and_then(|r| r.into_iter().next())
        .and_then(|r| Some(TableSize { total_bytes: number(&r, 0)?, table_bytes: number(&r, 1).unwrap_or(0), index_bytes: number(&r, 2).unwrap_or(0) }))
    } else {
        None
    };
    Ok(TableStructure {
        object: ObjectRef { kind, ..object.clone() },
        columns,
        constraints: Vec::new(),
        indexes,
        // ClickHouse keys are not unique: rows are not edited from the grid.
        row_identity: Vec::new(),
        full_row_key: Vec::new(),
        comment: opt(&rel, 4),
        row_estimate: number(&rel, 3),
        foreign_keys: Vec::new(),
        size,
    })
}

pub async fn dependencies(client: &Client, object: &ObjectRef) -> DbResult<Dependencies> {
    if !object.kind.is_relation() {
        return Ok(Dependencies::default());
    }
    let found = rows(
        client,
        "SELECT arrayStringConcat(dependencies_database, '\\x1f'), arrayStringConcat(dependencies_table, '\\x1f'), \
                as_select FROM system.tables WHERE database = {db:String} AND name = {t:String}",
        &[("db", &object.database), ("t", &object.name)],
    )
    .await?;
    let mut deps = Dependencies::default();
    let Some(r) = found.first() else { return Err(not_found(object)) };
    let dbs = text(r, 0);
    let tables = text(r, 1);
    for (db, table) in dbs.split('\u{1f}').zip(tables.split('\u{1f}')).filter(|(_, t)| !t.is_empty()) {
        let kind = relation(client, &ObjectRef::new(NodeKind::Table, db, None, table)).await.map(|r| kind_of(&text(&r, 2))).unwrap_or(NodeKind::View);
        deps.used_by.push(Dependency { object: ObjectRef::new(kind, db, None, table), relation: "view".into() });
    }
    // What a view reads: the tables named in its query.
    if let Some(select) = opt(r, 2) {
        let candidates = rows(client, &format!("{RELATIONS} AND database NOT IN {SYSTEM_DATABASES}"), &[]).await?;
        for c in &candidates {
            let (db, name) = (text(c, 0), text(c, 1));
            let mentioned = select.contains(&format!("{db}.{name}")) || (db == object.database && select.split(|ch: char| !ch.is_alphanumeric() && ch != '_').any(|w| w == name));
            if mentioned && !(db == object.database && name == object.name) {
                deps.uses.push(Dependency { object: ObjectRef::new(kind_of(&text(c, 2)), db, None, name), relation: "reads".into() });
            }
        }
    }
    Ok(deps)
}

pub async fn schema_model(client: &Client, database: &str) -> DbResult<SchemaModel> {
    let found = rows(
        client,
        &format!(
            "SELECT c.table, t.engine, c.name, c.type, toString(c.is_in_primary_key) FROM system.columns c \
             JOIN system.tables t ON t.database = c.database AND t.name = c.table \
             WHERE c.database = {{db:String}} AND t.name NOT LIKE '.inner%' ORDER BY c.table, c.position LIMIT {}",
            SchemaModel::MAX_COLUMNS
        ),
        &[("db", database)],
    )
    .await?;
    let mut model = SchemaModel::default();
    for r in &found {
        let name = text(r, 0);
        if model.tables.last().is_none_or(|t: &ModelTable| t.object.name != name) {
            model.tables.push(ModelTable { object: ObjectRef::new(kind_of(&text(r, 1)), database, None, name), columns: Vec::new() });
        }
        if let Some(table) = model.tables.last_mut() {
            let data_type = text(r, 3);
            table.columns.push(ModelColumn { name: text(r, 2), nullable: data_type.starts_with("Nullable("), primary_key: flag(r, 4), data_type });
        }
    }
    model.truncated = model.tables.len() > SchemaModel::MAX_TABLES || found.len() >= SchemaModel::MAX_COLUMNS;
    model.tables.truncate(SchemaModel::MAX_TABLES);
    Ok(model)
}

pub async fn ddl(client: &Client, object: &ObjectRef) -> DbResult<String> {
    let (sql, pairs): (&str, Vec<(&str, &str)>) = match object.kind {
        NodeKind::Table | NodeKind::View | NodeKind::MaterializedView => (
            "SELECT create_table_query FROM system.tables WHERE database = {db:String} AND name = {t:String}",
            vec![("db", &object.database), ("t", &object.name)],
        ),
        NodeKind::Index => {
            let table = object.owner_table().ok_or_else(|| DbError::invalid("An index needs its table."))?;
            let found = rows(
                client,
                "SELECT expr, type_full, toString(granularity) FROM system.data_skipping_indices \
                 WHERE database = {db:String} AND table = {t:String} AND name = {i:String}",
                &[("db", &object.database), ("t", &table.name), ("i", &object.name)],
            )
            .await?;
            let r = found.first().ok_or_else(|| not_found(object))?;
            return Ok(format!(
                "ALTER TABLE {} ADD INDEX {} {} TYPE {} GRANULARITY {};\n",
                D.display_name(&table),
                D.display_ident(&object.name),
                text(r, 0),
                text(r, 1),
                text(r, 2)
            ));
        }
        NodeKind::Function => ("SELECT create_query FROM system.functions WHERE name = {t:String}", vec![("t", &object.name)]),
        _ => return Err(DbError::invalid("This object has no definition.")),
    };
    let found = rows(client, sql, &pairs).await?;
    let text = found.first().and_then(|r| opt(r, 0)).ok_or_else(|| not_found(object))?;
    Ok(format!("{};\n", text.trim().trim_end_matches(';')))
}
