//! Catalog reads from each database's `INFORMATION_SCHEMA` and from `SHOW`
//! commands (keys). Values travel as bound parameters; database names are
//! quoted identifiers.

use dq_core::node::{GROUP_FUNCTIONS, GROUP_PROCEDURES, GROUP_SEQUENCES, Node, NodeKind, ObjectRef};
use dq_core::result::TextRow;
use dq_core::schema::{
    ColumnInfo, ConstraintInfo, ConstraintKind, Dependencies, Dependency, ForeignKey, ModelColumn, ModelTable, SchemaModel,
    TableStructure,
};
use dq_core::sql::{Dialect, SnowflakeDialect};
use dq_core::value::{Cell, CellLimits};
use dq_core::{DbError, DbResult};

use crate::client::Client;
use crate::results::ResultSet;

const D: SnowflakeDialect = SnowflakeDialect;

/// Runs a catalog statement and returns rows as text, with column names.
pub async fn query(client: &Client, sql: &str, binds: &[&str]) -> DbResult<(Vec<String>, Vec<TextRow>)> {
    let binds: Vec<Option<String>> = binds.iter().map(|b| Some((*b).to_owned())).collect();
    let data = client.query(sql, &binds, &uuid::Uuid::new_v4().to_string()).await?;
    let result = ResultSet::new(data, CellLimits::FULL)?;
    let names = result.rowtype.iter().map(|t| t.name.to_ascii_lowercase()).collect();
    let rows = result.all(client).await?;
    Ok((names, rows.into_iter().map(|r| TextRow(r.iter().map(Cell::to_text).collect())).collect()))
}

async fn rows(client: &Client, sql: &str, binds: &[&str]) -> DbResult<Vec<TextRow>> {
    Ok(query(client, sql, binds).await?.1)
}

/// A `SHOW` result's column by name.
fn column(names: &[String], name: &str) -> usize {
    names.iter().position(|n| n == name).unwrap_or(usize::MAX)
}

fn info_schema(database: &str) -> String {
    format!("{}.INFORMATION_SCHEMA", D.quote_ident(database))
}

fn schema_of(object: &ObjectRef) -> DbResult<&str> {
    object.schema.as_deref().ok_or_else(|| DbError::invalid("A schema is required."))
}

fn not_found(object: &ObjectRef) -> DbError {
    DbError::not_found(format!("{} no longer exists.", D.display_name(object)))
}

fn kind_of(table_type: &str) -> NodeKind {
    match table_type {
        "VIEW" => NodeKind::View,
        "MATERIALIZED VIEW" => NodeKind::MaterializedView,
        _ => NodeKind::Table,
    }
}

pub async fn databases(client: &Client, current: &str) -> DbResult<Vec<Node>> {
    let (names, found) = query(client, "SHOW DATABASES", &[]).await?;
    let (name, comment) = (column(&names, "name"), column(&names, "comment"));
    let mut nodes: Vec<Node> = found
        .iter()
        .map(|r| {
            let db = r.string(name);
            let mut node = Node::container(ObjectRef::new(NodeKind::Database, db.clone(), None, db.clone()));
            node.current = db == current;
            node.comment = r.text(comment).filter(|c| !c.is_empty());
            node
        })
        .collect();
    nodes.sort_by(|a, b| a.object.name.cmp(&b.object.name));
    Ok(nodes)
}

pub async fn schemas(client: &Client, database: &str) -> DbResult<Vec<Node>> {
    let (names, found) = query(client, &format!("SHOW SCHEMAS IN DATABASE {}", D.quote_ident(database)), &[]).await?;
    let name = column(&names, "name");
    let mut nodes: Vec<Node> = found
        .iter()
        .map(|r| r.string(name))
        .filter(|s| s != "INFORMATION_SCHEMA")
        .map(|s| Node::container(ObjectRef::new(NodeKind::Schema, database, Some(s.clone()), s)))
        .collect();
    nodes.sort_by_key(|n| (n.object.name != "PUBLIC", n.object.name.clone()));
    Ok(nodes)
}

fn relation_node(database: &str, r: &TextRow) -> Node {
    let kind = kind_of(&r.string(2));
    let object = ObjectRef::new(kind, database, Some(r.string(0)), r.string(1));
    let mut node = Node::leaf(object);
    node.row_estimate = r.number(3);
    node.comment = r.text(4).filter(|c| !c.is_empty());
    node
}

pub async fn relations(client: &Client, database: &str, schema: &str) -> DbResult<Vec<Node>> {
    let found = rows(
        client,
        &format!(
            "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE, ROW_COUNT, COMMENT FROM {}.TABLES WHERE TABLE_SCHEMA = ? \
             ORDER BY TABLE_TYPE <> 'BASE TABLE', TABLE_NAME",
            info_schema(database)
        ),
        &[schema],
    )
    .await?;
    let mut nodes: Vec<Node> = found.iter().map(|r| relation_node(database, r)).collect();
    // Not every account (or emulator) exposes these views; a missing one just hides its group.
    for (group, view, column) in
        [(GROUP_SEQUENCES, "SEQUENCES", "SEQUENCE_SCHEMA"), (GROUP_FUNCTIONS, "FUNCTIONS", "FUNCTION_SCHEMA"), (GROUP_PROCEDURES, "PROCEDURES", "PROCEDURE_SCHEMA")]
    {
        let sql = format!("SELECT COUNT(*) FROM {}.{view} WHERE {column} = ?", info_schema(database));
        let count = rows(client, &sql, &[schema]).await.ok().and_then(|r| r.first().and_then(|r| r.number(0))).unwrap_or(0);
        if count > 0 {
            let mut node = Node::container(ObjectRef::new(NodeKind::Group, database, Some(schema.to_owned()), group));
            node.row_estimate = Some(count);
            nodes.push(node);
        }
    }
    Ok(nodes)
}

pub async fn group_children(client: &Client, group: &ObjectRef) -> DbResult<Vec<Node>> {
    let schema = schema_of(group)?;
    let (view, name, column, kind) = match group.name.as_str() {
        GROUP_SEQUENCES => ("SEQUENCES", "SEQUENCE_NAME", "SEQUENCE_SCHEMA", NodeKind::Sequence),
        GROUP_FUNCTIONS => ("FUNCTIONS", "FUNCTION_NAME", "FUNCTION_SCHEMA", NodeKind::Function),
        GROUP_PROCEDURES => ("PROCEDURES", "PROCEDURE_NAME", "PROCEDURE_SCHEMA", NodeKind::Procedure),
        other => return Err(DbError::invalid(format!("Unknown group `{other}`."))),
    };
    let found = rows(
        client,
        &format!("SELECT DISTINCT {name} FROM {}.{view} WHERE {column} = ? ORDER BY 1", info_schema(&group.database)),
        &[schema],
    )
    .await?;
    Ok(found.iter().map(|r| Node::leaf(ObjectRef::new(kind, group.database.clone(), group.schema.clone(), r.string(0)))).collect())
}

pub async fn search(client: &Client, database: &str, query_text: &str, limit: u32) -> DbResult<Vec<Node>> {
    let pattern = dq_core::sql::builder::like_contains(query_text);
    let found = rows(
        client,
        &format!(
            "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE, ROW_COUNT, COMMENT FROM {}.TABLES \
             WHERE TABLE_NAME ILIKE ? ESCAPE '\\\\' AND TABLE_SCHEMA <> 'INFORMATION_SCHEMA' \
             ORDER BY LENGTH(TABLE_NAME), TABLE_NAME LIMIT {}",
            info_schema(database),
            limit.min(1_000)
        ),
        &[&pattern],
    )
    .await?;
    Ok(found.iter().map(|r| relation_node(database, r)).collect())
}

async fn relation(client: &Client, object: &ObjectRef) -> DbResult<TextRow> {
    let schema = schema_of(object)?;
    rows(
        client,
        &format!(
            "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE, ROW_COUNT, COMMENT FROM {}.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
            info_schema(&object.database)
        ),
        &[schema, &object.name],
    )
    .await?
    .into_iter()
    .next()
    .ok_or_else(|| not_found(object))
}

pub async fn row_estimate(client: &Client, object: &ObjectRef) -> DbResult<Option<i64>> {
    Ok(relation(client, object).await?.number(3))
}

/// `NUMBER(12,2)`, `VARCHAR(255)`, `TIMESTAMP_NTZ(9)`, … from `INFORMATION_SCHEMA.COLUMNS`.
fn type_spelling(data_type: &str, length: Option<i64>, precision: Option<i64>, scale: Option<i64>, datetime: Option<i64>) -> String {
    match data_type {
        "NUMBER" => format!("NUMBER({},{})", precision.unwrap_or(38), scale.unwrap_or(0)),
        "TEXT" | "VARCHAR" => length.filter(|l| *l < 16_777_216).map_or_else(|| "VARCHAR".into(), |l| format!("VARCHAR({l})")),
        "TIMESTAMP_NTZ" | "TIMESTAMP_LTZ" | "TIMESTAMP_TZ" | "TIME" => datetime.map_or_else(|| data_type.to_owned(), |p| format!("{data_type}({p})")),
        other => other.to_owned(),
    }
}

const NUMERIC: &[&str] = &["NUMBER", "FLOAT", "DECIMAL", "INTEGER", "REAL", "DOUBLE"];
const NOT_EDITABLE: &[&str] = &["BINARY", "VARIANT", "OBJECT", "ARRAY", "GEOGRAPHY", "GEOMETRY", "VECTOR"];

async fn columns(client: &Client, object: &ObjectRef, primary: &[String]) -> DbResult<Vec<ColumnInfo>> {
    let schema = schema_of(object)?;
    let found = rows(
        client,
        &format!(
            "SELECT COLUMN_NAME, DATA_TYPE, CHARACTER_MAXIMUM_LENGTH, NUMERIC_PRECISION, NUMERIC_SCALE, IS_NULLABLE, \
                    COLUMN_DEFAULT, COMMENT, DATETIME_PRECISION FROM {}.COLUMNS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
            info_schema(&object.database)
        ),
        &[schema, &object.name],
    )
    .await?;
    Ok(found
        .iter()
        .map(|r| {
            let base = r.string(1).to_ascii_uppercase();
            let name = r.string(0);
            let default = r.text(6).filter(|d| !d.is_empty());
            let identity = default.as_deref().is_some_and(|d| d.contains(".NEXTVAL") || d.starts_with("IDENTITY"));
            ColumnInfo {
                data_type: type_spelling(&base, r.number(2), r.number(3), r.number(4), r.number(8)),
                nullable: r.flag(5),
                primary_key: primary.contains(&name),
                generated: identity,
                identity,
                editable: !NOT_EDITABLE.contains(&base.as_str()),
                numeric: NUMERIC.contains(&base.as_str()),
                comment: r.text(7).filter(|c| !c.is_empty()),
                read_as_text: matches!(base.as_str(), "VARIANT" | "OBJECT" | "ARRAY" | "GEOGRAPHY" | "GEOMETRY"),
                default,
                name,
            }
        })
        .collect())
}

/// Columns of each key of `SHOW <kind> KEYS IN TABLE …`; constraints Snowflake
/// declares but does not enforce (except NOT NULL).
async fn keys(client: &Client, kind: &str, scope: &str) -> Vec<(String, Vec<TextRow>, Vec<String>)> {
    let Ok((names, found)) = query(client, &format!("SHOW {kind} KEYS IN {scope}"), &[]).await else { return Vec::new() };
    let mut groups: Vec<(String, Vec<TextRow>, Vec<String>)> = Vec::new();
    let constraint = names.iter().position(|n| n == "constraint_name" || n == "fk_name").unwrap_or(usize::MAX);
    let table = names.iter().position(|n| n == "table_name" || n == "fk_table_name").unwrap_or(usize::MAX);
    for r in found {
        let key = format!("{}\u{1f}{}", r.string(table), r.string(constraint));
        match groups.iter_mut().find(|g| g.0 == key) {
            Some(g) => g.1.push(r),
            None => groups.push((key, vec![r], names.clone())),
        }
    }
    groups
}

fn by_name(names: &[String], row: &TextRow, name: &str) -> String {
    row.string(column(names, name))
}

fn foreign_keys_from(database: &str, groups: &[(String, Vec<TextRow>, Vec<String>)]) -> Vec<ForeignKey> {
    groups
        .iter()
        .map(|(_, rows, names)| {
            let first = &rows[0];
            let table = |db: String, schema: String, name: String| {
                ObjectRef::new(NodeKind::Table, if db.is_empty() { database.to_owned() } else { db }, Some(schema), name)
            };
            let rule = |name: &str| Some(by_name(names, first, name)).filter(|r| !r.is_empty() && r != "NO ACTION");
            ForeignKey {
                name: by_name(names, first, "fk_name"),
                source: table(by_name(names, first, "fk_database_name"), by_name(names, first, "fk_schema_name"), by_name(names, first, "fk_table_name")),
                columns: rows.iter().map(|r| by_name(names, r, "fk_column_name")).collect(),
                target: table(by_name(names, first, "pk_database_name"), by_name(names, first, "pk_schema_name"), by_name(names, first, "pk_table_name")),
                target_columns: rows.iter().map(|r| by_name(names, r, "pk_column_name")).collect(),
                on_update: rule("update_rule"),
                on_delete: rule("delete_rule"),
            }
        })
        .collect()
}

fn table_scope(object: &ObjectRef) -> String {
    format!("TABLE {}", D.quoted_name(object))
}

pub async fn structure(client: &Client, object: &ObjectRef) -> DbResult<TableStructure> {
    let rel = relation(client, object).await?;
    let kind = kind_of(&rel.string(2));
    let scope = table_scope(object);
    let primary_groups = if kind == NodeKind::Table { keys(client, "PRIMARY", &scope).await } else { Vec::new() };
    let primary: Vec<String> = primary_groups.first().map(|(_, rows, names)| rows.iter().map(|r| by_name(names, r, "column_name")).collect()).unwrap_or_default();
    let columns = columns(client, object, &primary).await?;
    let list = |cols: &[String]| cols.iter().map(|c| D.display_ident(c)).collect::<Vec<_>>().join(", ");
    let mut constraints = Vec::new();
    if let Some((_, rows, names)) = primary_groups.first() {
        constraints.push(ConstraintInfo { kind: ConstraintKind::PrimaryKey, name: by_name(names, &rows[0], "constraint_name"), definition: format!("PRIMARY KEY ({})", list(&primary)) });
    }
    for (_, rows, names) in keys(client, "UNIQUE", &scope).await {
        let cols: Vec<String> = rows.iter().map(|r| by_name(&names, r, "column_name")).collect();
        constraints.push(ConstraintInfo { kind: ConstraintKind::Unique, name: by_name(&names, &rows[0], "constraint_name"), definition: format!("UNIQUE ({})", list(&cols)) });
    }
    let foreign_keys = if kind == NodeKind::Table { foreign_keys_from(&object.database, &keys(client, "IMPORTED", &scope).await) } else { Vec::new() };
    for fk in &foreign_keys {
        constraints.push(ConstraintInfo {
            kind: ConstraintKind::ForeignKey,
            name: fk.name.clone(),
            definition: format!("FOREIGN KEY ({}) REFERENCES {} ({})", list(&fk.columns), D.display_name(&fk.target), list(&fk.target_columns)),
        });
    }
    Ok(TableStructure {
        object: ObjectRef { kind, ..object.clone() },
        columns,
        constraints,
        indexes: Vec::new(),
        row_identity: if kind == NodeKind::Table { primary } else { Vec::new() },
        full_row_key: Vec::new(),
        comment: rel.text(4).filter(|c| !c.is_empty()),
        row_estimate: rel.number(3),
        foreign_keys,
        size: None,
    })
}

pub async fn dependencies(client: &Client, object: &ObjectRef) -> DbResult<Dependencies> {
    if !object.kind.is_relation() {
        return Ok(Dependencies::default());
    }
    let scope = table_scope(object);
    let mut deps = Dependencies::default();
    for fk in foreign_keys_from(&object.database, &keys(client, "IMPORTED", &scope).await) {
        deps.uses.push(Dependency { object: fk.target, relation: format!("foreign key {}", fk.name) });
    }
    for fk in foreign_keys_from(&object.database, &keys(client, "EXPORTED", &scope).await) {
        deps.used_by.push(Dependency { object: fk.source, relation: format!("foreign key {}", fk.name) });
    }
    Ok(deps)
}

pub async fn schema_model(client: &Client, database: &str, schema: &str) -> DbResult<SchemaModel> {
    let found = rows(
        client,
        &format!(
            "SELECT c.TABLE_NAME, t.TABLE_TYPE, c.COLUMN_NAME, c.DATA_TYPE, c.CHARACTER_MAXIMUM_LENGTH, c.NUMERIC_PRECISION, \
                    c.NUMERIC_SCALE, c.IS_NULLABLE, c.DATETIME_PRECISION \
             FROM {is}.COLUMNS c JOIN {is}.TABLES t ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME \
             WHERE c.TABLE_SCHEMA = ? ORDER BY c.TABLE_NAME, c.ORDINAL_POSITION LIMIT {}",
            SchemaModel::MAX_COLUMNS,
            is = info_schema(database)
        ),
        &[schema],
    )
    .await?;
    let scope = format!("SCHEMA {}.{}", D.quote_ident(database), D.quote_ident(schema));
    let primary: Vec<(String, String)> = keys(client, "PRIMARY", &scope)
        .await
        .iter()
        .flat_map(|(_, rows, names)| rows.iter().map(|r| (by_name(names, r, "table_name"), by_name(names, r, "column_name"))).collect::<Vec<_>>())
        .collect();
    let mut model = SchemaModel::default();
    for r in &found {
        let name = r.string(0);
        if model.tables.last().is_none_or(|t: &ModelTable| t.object.name != name) {
            model.tables.push(ModelTable { object: ObjectRef::new(kind_of(&r.string(1)), database, Some(schema.to_owned()), name.clone()), columns: Vec::new() });
        }
        if let Some(table) = model.tables.last_mut() {
            let column = r.string(2);
            table.columns.push(ModelColumn {
                primary_key: primary.contains(&(name.clone(), column.clone())),
                data_type: type_spelling(&r.string(3).to_ascii_uppercase(), r.number(4), r.number(5), r.number(6), r.number(8)),
                nullable: r.flag(7),
                name: column,
            });
        }
    }
    model.truncated = model.tables.len() > SchemaModel::MAX_TABLES || found.len() >= SchemaModel::MAX_COLUMNS;
    model.tables.truncate(SchemaModel::MAX_TABLES);
    model.foreign_keys = foreign_keys_from(database, &keys(client, "IMPORTED", &scope).await);
    Ok(model)
}

/// `GET_DDL`, or a `CREATE TABLE` built from the columns where it is not available.
pub async fn ddl(client: &Client, object: &ObjectRef) -> DbResult<String> {
    let object_type = match object.kind {
        NodeKind::Table => "TABLE",
        NodeKind::View => "VIEW",
        NodeKind::MaterializedView => "MATERIALIZED_VIEW",
        NodeKind::Sequence => "SEQUENCE",
        NodeKind::Function => "FUNCTION",
        NodeKind::Procedure => "PROCEDURE",
        _ => return Err(DbError::invalid("This object has no definition.")),
    };
    let name = D.quoted_name(object);
    if let Ok(found) = rows(client, "SELECT GET_DDL(?, ?)", &[object_type, &name]).await {
        if let Some(text) = found.first().and_then(|r| r.text(0)).filter(|t| !t.trim().is_empty()) {
            return Ok(format!("{}\n", text.trim()));
        }
    }
    if object.kind != NodeKind::Table {
        return Err(DbError::unsupported("This server does not provide definitions (GET_DDL)."));
    }
    let structure = structure(client, object).await?;
    let mut lines: Vec<String> = structure
        .columns
        .iter()
        .map(|c| {
            let mut line = format!("{} {}", D.display_ident(&c.name), c.data_type);
            if let Some(d) = &c.default {
                line.push_str(&format!(" DEFAULT {d}"));
            }
            if !c.nullable {
                line.push_str(" NOT NULL");
            }
            line
        })
        .collect();
    lines.extend(structure.constraints.iter().map(|c| format!("CONSTRAINT {} {}", D.display_ident(&c.name), c.definition)));
    Ok(format!("CREATE TABLE {} (\n  {}\n);\n", D.display_name(object), lines.join(",\n  ")))
}
