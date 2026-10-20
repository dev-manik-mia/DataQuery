//! The explorer and table structure, from the REST resources (projects,
//! datasets, tables, routines: no query job, nothing billed) and, for
//! definitions and the schema model, each dataset's `INFORMATION_SCHEMA`.
//! Values travel as query parameters; names are quoted identifiers.

use std::fmt::Write;

use dq_core::node::{GROUP_FUNCTIONS, GROUP_PROCEDURES, Node, NodeKind, ObjectRef};
use dq_core::result::TextRow;
use dq_core::schema::{
    ColumnInfo, ConstraintInfo, ConstraintKind, Dependencies, Dependency, ForeignKey, IndexInfo, ModelColumn,
    ModelTable, SchemaModel, TableSize, TableStructure,
};
use dq_core::sql::{BigQueryDialect, Dialect, SqlFlavor, bind_named};
use dq_core::value::{Cell, CellLimits};
use dq_core::{DbError, DbResult};
use serde_json::Value;

use crate::client::{Client, Field, JobSpec, MAX_LISTED, new_job_id};
use crate::results::ResultSet;
use crate::values::{is_numeric, parameter, type_label};

const D: BigQueryDialect = BigQueryDialect;
/// Datasets searched by name when no default dataset is set.
const SEARCHED_DATASETS: usize = 50;
/// `BigQuery` stops a catalog query that runs longer than this.
const CATALOG_TIMEOUT_SECS: u32 = 120;

/// Runs a catalog query (`@p1`, `@p2`, … bound as `STRING`) and returns its rows as text.
pub async fn query(client: &Client, sql: &str, binds: &[&str]) -> DbResult<Vec<TextRow>> {
    let values: Vec<Option<String>> = binds.iter().map(|b| Some((*b).to_owned())).collect();
    let params = bind_named(sql, SqlFlavor::Bigquery, &values)?
        .iter()
        .map(|(name, value)| parameter(Some(name), value.as_deref(), false))
        .collect();
    let spec = JobSpec { sql, params, named: true, timeout_secs: CATALOG_TIMEOUT_SECS, ..JobSpec::default() };
    let (job, page) = client.run(&spec, &new_job_id(), 10_000, |_| {}).await?;
    let rows = ResultSet::new(job, page, CellLimits::FULL).all(client).await?;
    Ok(rows.into_iter().map(|r| TextRow(r.iter().map(Cell::to_text).collect())).collect())
}

fn dataset_of(object: &ObjectRef) -> DbResult<&str> {
    object.schema.as_deref().ok_or_else(|| DbError::invalid("A dataset is required."))
}

/// `` `project`.`dataset`.INFORMATION_SCHEMA.<view> ``.
fn info_schema(project: &str, dataset: &str, view: &str) -> String {
    format!("{}.{}.INFORMATION_SCHEMA.{view}", D.quote_ident(project), D.quote_ident(dataset))
}

fn str_at<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str)
}

fn number_at(value: &Value, pointer: &str) -> Option<i64> {
    str_at(value, pointer).and_then(|n| n.parse().ok())
}

/// The billing project first, then every other project the account can use.
pub async fn projects(client: &Client) -> DbResult<Vec<Node>> {
    let mut names = vec![client.project.clone()];
    // Listing projects needs `resourcemanager` rights some accounts lack; the billing project suffices.
    for p in client.list("", &[], "projects", MAX_LISTED).await.unwrap_or_default() {
        if let Some(id) = str_at(&p, "/projectReference/projectId").or_else(|| str_at(&p, "/id"))
            && !names.iter().any(|n| n == id)
        {
            names.push(id.to_owned());
        }
    }
    names[1..].sort();
    Ok(names
        .into_iter()
        .map(|name| {
            let mut node = Node::container(ObjectRef::new(NodeKind::Database, name.clone(), None, name.clone()));
            node.current = name == client.project;
            node
        })
        .collect())
}

pub async fn datasets(client: &Client, project: &str) -> DbResult<Vec<Node>> {
    let found = client.list(project, &["datasets"], "datasets", MAX_LISTED).await?;
    let mut nodes: Vec<Node> = found
        .iter()
        .filter_map(|d| str_at(d, "/datasetReference/datasetId"))
        .map(|d| {
            let mut node = Node::container(ObjectRef::new(NodeKind::Schema, project, Some(d.to_owned()), d));
            node.current = project == client.project && client.dataset.as_deref() == Some(d);
            node
        })
        .collect();
    nodes.sort_by(|a, b| a.object.name.cmp(&b.object.name));
    Ok(nodes)
}

/// The explorer kind of a `tables.list` type, and a note for the unusual ones.
fn kind_of(table_type: &str) -> (NodeKind, Option<&'static str>) {
    match table_type {
        "VIEW" => (NodeKind::View, None),
        "MATERIALIZED_VIEW" => (NodeKind::MaterializedView, None),
        "EXTERNAL" => (NodeKind::Table, Some("External table")),
        "SNAPSHOT" => (NodeKind::Table, Some("Table snapshot (read-only)")),
        _ => (NodeKind::Table, None),
    }
}

fn table_node(project: &str, dataset: &str, table: &Value) -> Option<Node> {
    let name = str_at(table, "/tableReference/tableId")?;
    let (kind, note) = kind_of(str_at(table, "/type").unwrap_or("TABLE"));
    let mut node = Node::leaf(ObjectRef::new(kind, project, Some(dataset.to_owned()), name));
    node.comment = note.map(str::to_owned);
    Some(node)
}

async fn tables(client: &Client, project: &str, dataset: &str) -> DbResult<Vec<Value>> {
    client.list(project, &["datasets", dataset, "tables"], "tables", MAX_LISTED).await
}

/// Functions and procedures of a dataset; emulators without `routines.list` show none.
async fn routines(client: &Client, project: &str, dataset: &str) -> Vec<(String, NodeKind)> {
    let found = client.list(project, &["datasets", dataset, "routines"], "routines", MAX_LISTED).await;
    found
        .unwrap_or_default()
        .iter()
        .filter_map(|r| {
            let name = str_at(r, "/routineReference/routineId")?;
            let kind =
                if str_at(r, "/routineType") == Some("PROCEDURE") { NodeKind::Procedure } else { NodeKind::Function };
            Some((name.to_owned(), kind))
        })
        .collect()
}

pub async fn relations(client: &Client, project: &str, dataset: &str) -> DbResult<Vec<Node>> {
    let mut nodes: Vec<Node> =
        tables(client, project, dataset).await?.iter().filter_map(|t| table_node(project, dataset, t)).collect();
    nodes.sort_by_key(|n| (n.object.kind != NodeKind::Table, n.object.name.clone()));
    let routines = routines(client, project, dataset).await;
    for (group, kind) in [(GROUP_FUNCTIONS, NodeKind::Function), (GROUP_PROCEDURES, NodeKind::Procedure)] {
        let count = routines.iter().filter(|(_, k)| *k == kind).count();
        if count > 0 {
            let mut node = Node::container(ObjectRef::new(NodeKind::Group, project, Some(dataset.to_owned()), group));
            node.row_estimate = Some(count as i64);
            nodes.push(node);
        }
    }
    Ok(nodes)
}

pub async fn group_children(client: &Client, group: &ObjectRef) -> DbResult<Vec<Node>> {
    let dataset = dataset_of(group)?;
    let kind = match group.name.as_str() {
        GROUP_FUNCTIONS => NodeKind::Function,
        GROUP_PROCEDURES => NodeKind::Procedure,
        other => return Err(DbError::invalid(format!("Unknown group `{other}`."))),
    };
    let mut names: Vec<String> = routines(client, &group.database, dataset)
        .await
        .into_iter()
        .filter(|(_, k)| *k == kind)
        .map(|(n, _)| n)
        .collect();
    names.sort();
    Ok(names
        .into_iter()
        .map(|n| Node::leaf(ObjectRef::new(kind, group.database.clone(), Some(dataset.to_owned()), n)))
        .collect())
}

/// Tables whose name contains `text`: in the default dataset, else in the first datasets of the project.
pub async fn search(client: &Client, text: &str, limit: u32) -> DbResult<Vec<Node>> {
    let project = client.project.as_str();
    let datasets: Vec<String> = match &client.dataset {
        Some(d) => vec![d.clone()],
        None => datasets(client, project).await?.into_iter().take(SEARCHED_DATASETS).map(|n| n.object.name).collect(),
    };
    let needle = text.to_lowercase();
    let mut found = Vec::new();
    for dataset in datasets {
        for table in tables(client, project, &dataset).await.unwrap_or_default() {
            if let Some(node) =
                table_node(project, &dataset, &table).filter(|n| n.object.name.to_lowercase().contains(&needle))
            {
                found.push(node);
            }
        }
    }
    found.sort_by_key(|n| (n.object.name.len(), n.object.name.clone()));
    found.truncate(limit as usize);
    Ok(found)
}

/// The `tables.get` resource: schema, type, partitioning, clustering, keys, sizes.
async fn table(client: &Client, object: &ObjectRef) -> DbResult<Value> {
    let dataset = dataset_of(object)?;
    client.get(&object.database, &["datasets", dataset, "tables", &object.name], &[]).await
}

pub async fn row_estimate(client: &Client, object: &ObjectRef) -> DbResult<Option<i64>> {
    Ok(number_at(&table(client, object).await?, "/numRows"))
}

/// Types a text round-trip in the grid cannot write back.
const NOT_EDITABLE: &[&str] = &["STRUCT", "ARRAY", "JSON", "GEOGRAPHY", "BYTES", "RANGE"];

fn column(field: &Field, primary: &[String]) -> ColumnInfo {
    let data_type = type_label(field);
    let base = data_type.split(['(', '<']).next().unwrap_or_default().to_owned();
    ColumnInfo {
        nullable: field.mode.as_deref() != Some("REQUIRED"),
        default: field.default_value_expression.clone(),
        primary_key: primary.contains(&field.name),
        generated: false,
        identity: false,
        editable: !NOT_EDITABLE.contains(&base.as_str()),
        numeric: is_numeric(field),
        comment: field.description.clone().filter(|d| !d.is_empty()),
        read_as_text: false,
        name: field.name.clone(),
        data_type,
    }
}

fn fields(table: &Value) -> Vec<Field> {
    serde_json::from_value(table.pointer("/schema/fields").cloned().unwrap_or_default()).unwrap_or_default()
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect()
}

fn foreign_keys(object: &ObjectRef, table: &Value) -> Vec<ForeignKey> {
    let keys = table.pointer("/tableConstraints/foreignKeys").and_then(Value::as_array);
    keys.into_iter()
        .flatten()
        .map(|fk| {
            let pairs = fk["columnReferences"].as_array().map(Vec::as_slice).unwrap_or_default();
            let target = &fk["referencedTable"];
            ForeignKey {
                name: fk["name"].as_str().unwrap_or_default().to_owned(),
                source: object.clone(),
                columns: pairs.iter().filter_map(|p| p["referencingColumn"].as_str()).map(str::to_owned).collect(),
                target: ObjectRef::new(
                    NodeKind::Table,
                    target["projectId"].as_str().unwrap_or(&object.database),
                    target["datasetId"].as_str().map(str::to_owned).or_else(|| object.schema.clone()),
                    target["tableId"].as_str().unwrap_or_default(),
                ),
                target_columns: pairs
                    .iter()
                    .filter_map(|p| p["referencedColumn"].as_str())
                    .map(str::to_owned)
                    .collect(),
                on_update: None,
                on_delete: None,
            }
        })
        .collect()
}

/// `PARTITION BY …` of a table, and the column it uses.
fn partitioning(table: &Value, columns: &[ColumnInfo]) -> Option<(String, Option<String>)> {
    if let Some(range) = table.get("rangePartitioning") {
        let field = range["field"].as_str()?;
        let r = &range["range"];
        let bound = |k: &str| r[k].as_str().unwrap_or("0").to_owned();
        return Some((
            format!(
                "PARTITION BY RANGE_BUCKET({}, GENERATE_ARRAY({}, {}, {}))",
                D.display_ident(field),
                bound("start"),
                bound("end"),
                bound("interval")
            ),
            Some(field.to_owned()),
        ));
    }
    let time = table.get("timePartitioning")?;
    let unit = time["type"].as_str().unwrap_or("DAY");
    Some(match time["field"].as_str() {
        None if unit == "DAY" => ("PARTITION BY _PARTITIONDATE".into(), None),
        None => (format!("PARTITION BY TIMESTAMP_TRUNC(_PARTITIONTIME, {unit})"), None),
        Some(field) => {
            let column = D.display_ident(field);
            let data_type = columns.iter().find(|c| c.name == field).map_or("TIMESTAMP", |c| c.data_type.as_str());
            let expression = match (data_type, unit) {
                ("DATE", "DAY") => column,
                (_, "DAY") => format!("DATE({column})"),
                (t, unit) => format!("{t}_TRUNC({column}, {unit})"),
            };
            (format!("PARTITION BY {expression}"), Some(field.to_owned()))
        }
    })
}

/// Partitioning and clustering, shown with the indexes (`BigQuery` has no others).
fn layout(table: &Value, columns: &[ColumnInfo]) -> Vec<IndexInfo> {
    let mut out = Vec::new();
    if let Some((definition, field)) = partitioning(table, columns) {
        out.push(IndexInfo {
            name: "partitioning".into(),
            definition,
            unique: false,
            primary: false,
            columns: field.into_iter().collect(),
            size_bytes: None,
        });
    }
    let clustering = strings(table.pointer("/clustering/fields"));
    if !clustering.is_empty() {
        let list = clustering.iter().map(|c| D.display_ident(c)).collect::<Vec<_>>().join(", ");
        out.push(IndexInfo {
            name: "clustering".into(),
            definition: format!("CLUSTER BY {list}"),
            unique: false,
            primary: false,
            columns: clustering,
            size_bytes: None,
        });
    }
    out
}

fn list(columns: &[String]) -> String {
    columns.iter().map(|c| D.display_ident(c)).collect::<Vec<_>>().join(", ")
}

pub async fn structure(client: &Client, object: &ObjectRef) -> DbResult<TableStructure> {
    let table = table(client, object).await?;
    let (kind, _) = kind_of(str_at(&table, "/type").unwrap_or("TABLE"));
    let primary = strings(table.pointer("/tableConstraints/primaryKey/columns"));
    let columns: Vec<ColumnInfo> = fields(&table).iter().map(|f| column(f, &primary)).collect();
    let foreign_keys = foreign_keys(object, &table);
    let mut constraints = Vec::new();
    if !primary.is_empty() {
        constraints.push(ConstraintInfo {
            kind: ConstraintKind::PrimaryKey,
            name: "PRIMARY KEY".into(),
            definition: format!("PRIMARY KEY ({}) NOT ENFORCED", list(&primary)),
        });
    }
    for fk in &foreign_keys {
        constraints.push(ConstraintInfo {
            kind: ConstraintKind::ForeignKey,
            name: fk.name.clone(),
            definition: format!(
                "FOREIGN KEY ({}) REFERENCES {} ({}) NOT ENFORCED",
                list(&fk.columns),
                D.display_name(&fk.target),
                list(&fk.target_columns)
            ),
        });
    }
    let bytes = number_at(&table, "/numBytes");
    // Snapshots and external tables cannot be changed; keys are declared, not enforced.
    let writable = kind == NodeKind::Table && matches!(str_at(&table, "/type"), Some("TABLE") | None);
    Ok(TableStructure {
        object: ObjectRef { kind, ..object.clone() },
        indexes: layout(&table, &columns),
        row_identity: if writable { primary } else { Vec::new() },
        full_row_key: Vec::new(),
        comment: str_at(&table, "/description").filter(|d| !d.is_empty()).map(str::to_owned),
        row_estimate: number_at(&table, "/numRows"),
        size: bytes.map(|b| TableSize { total_bytes: b, table_bytes: b, index_bytes: 0 }),
        columns,
        constraints,
        foreign_keys,
    })
}

pub async fn dependencies(client: &Client, object: &ObjectRef) -> DbResult<Dependencies> {
    if !object.kind.is_relation() {
        return Ok(Dependencies::default());
    }
    let table = table(client, object).await?;
    let mut deps = Dependencies::default();
    for fk in foreign_keys(object, &table) {
        deps.uses.push(Dependency { object: fk.target, relation: format!("foreign key {}", fk.name) });
    }
    Ok(deps)
}

pub async fn schema_model(client: &Client, project: &str, dataset: Option<&str>) -> DbResult<SchemaModel> {
    let datasets: Vec<String> = match dataset {
        Some(d) => vec![d.to_owned()],
        None => datasets(client, project).await?.into_iter().map(|n| n.object.name).collect(),
    };
    let mut model = SchemaModel::default();
    let mut columns = 0usize;
    for dataset in datasets {
        if model.tables.len() >= SchemaModel::MAX_TABLES || columns >= SchemaModel::MAX_COLUMNS {
            model.truncated = true;
            break;
        }
        let sql = format!(
            "SELECT c.table_name, t.table_type, c.column_name, c.data_type, c.is_nullable \
             FROM {} c JOIN {} t ON t.table_name = c.table_name \
             ORDER BY c.table_name, c.ordinal_position LIMIT {}",
            info_schema(project, &dataset, "COLUMNS"),
            info_schema(project, &dataset, "TABLES"),
            SchemaModel::MAX_COLUMNS - columns
        );
        let found = query(client, &sql, &[]).await?;
        columns += found.len();
        let primary = primary_keys(client, project, &dataset).await;
        for r in &found {
            let name = r.string(0);
            if model
                .tables
                .last()
                .is_none_or(|t: &ModelTable| t.object.name != name || t.object.schema.as_deref() != Some(&dataset))
            {
                let (kind, _) = kind_of(&r.string(1).replace(' ', "_").replace("BASE_TABLE", "TABLE"));
                model.tables.push(ModelTable {
                    object: ObjectRef::new(kind, project, Some(dataset.clone()), name.clone()),
                    columns: Vec::new(),
                });
            }
            if let Some(table) = model.tables.last_mut() {
                let column = r.string(2);
                table.columns.push(ModelColumn {
                    primary_key: primary.contains(&(name.clone(), column.clone())),
                    data_type: r.string(3),
                    nullable: r.flag(4),
                    name: column,
                });
            }
        }
    }
    model.truncated |= model.tables.len() > SchemaModel::MAX_TABLES || columns >= SchemaModel::MAX_COLUMNS;
    model.tables.truncate(SchemaModel::MAX_TABLES);
    Ok(model)
}

/// `(table, column)` of every declared primary key in a dataset; none where the views are missing.
async fn primary_keys(client: &Client, project: &str, dataset: &str) -> Vec<(String, String)> {
    let sql = format!(
        "SELECT k.table_name, k.column_name FROM {} k JOIN {} c \
         ON c.constraint_name = k.constraint_name AND c.table_name = k.table_name \
         WHERE c.constraint_type = 'PRIMARY KEY'",
        info_schema(project, dataset, "KEY_COLUMN_USAGE"),
        info_schema(project, dataset, "TABLE_CONSTRAINTS")
    );
    query(client, &sql, &[]).await.unwrap_or_default().iter().map(|r| (r.string(0), r.string(1))).collect()
}

/// The definition from `INFORMATION_SCHEMA` (`TABLES.ddl`, `ROUTINES.ddl`), or
/// one built from the table resource where the column is not available.
pub async fn ddl(client: &Client, object: &ObjectRef) -> DbResult<String> {
    let dataset = dataset_of(object)?;
    let (view, column) = match object.kind {
        NodeKind::Table | NodeKind::View | NodeKind::MaterializedView => ("TABLES", "table_name"),
        NodeKind::Function | NodeKind::Procedure => ("ROUTINES", "routine_name"),
        _ => return Err(DbError::invalid("This object has no definition.")),
    };
    let sql = format!("SELECT ddl FROM {} WHERE {column} = @p1", info_schema(&object.database, dataset, view));
    if let Ok(found) = query(client, &sql, &[&object.name]).await
        && let Some(text) = found.first().and_then(|r| r.text(0)).filter(|t| !t.trim().is_empty())
    {
        return Ok(format!("{}\n", text.trim()));
    }
    if !object.kind.is_relation() {
        return routine_ddl(client, object, dataset).await;
    }
    let table = table(client, object).await?;
    let name = D.display_name(object);
    for (kind, pointer, keyword) in [
        (NodeKind::View, "/view/query", "VIEW"),
        (NodeKind::MaterializedView, "/materializedView/query", "MATERIALIZED VIEW"),
    ] {
        if object.kind == kind {
            let body = str_at(&table, pointer).unwrap_or_default().trim();
            return Ok(format!("CREATE {keyword} {name} AS\n{body};\n"));
        }
    }
    Ok(table_ddl(object, &table))
}

fn table_ddl(object: &ObjectRef, table: &Value) -> String {
    let primary = strings(table.pointer("/tableConstraints/primaryKey/columns"));
    let columns: Vec<ColumnInfo> = fields(table).iter().map(|f| column(f, &primary)).collect();
    let mut lines: Vec<String> = columns
        .iter()
        .map(|c| {
            let mut line = format!("{} {}", D.display_ident(&c.name), c.data_type);
            if !c.nullable {
                line.push_str(" NOT NULL");
            }
            if let Some(d) = &c.default {
                let _ = write!(line, " DEFAULT {d}");
            }
            if let Some(comment) = &c.comment {
                let _ = write!(line, " OPTIONS(description = {})", D.quote_literal(comment));
            }
            line
        })
        .collect();
    if !primary.is_empty() {
        lines.push(format!("PRIMARY KEY ({}) NOT ENFORCED", list(&primary)));
    }
    for fk in foreign_keys(object, table) {
        lines.push(format!(
            "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) NOT ENFORCED",
            D.display_ident(&fk.name),
            list(&fk.columns),
            D.display_name(&fk.target),
            list(&fk.target_columns)
        ));
    }
    let mut sql = format!("CREATE TABLE {} (\n  {}\n)", D.display_name(object), lines.join(",\n  "));
    for index in layout(table, &columns) {
        let _ = write!(sql, "\n{}", index.definition);
    }
    if let Some(description) = str_at(table, "/description").filter(|d| !d.is_empty()) {
        let _ = write!(sql, "\nOPTIONS(description = {})", D.quote_literal(description));
    }
    sql.push_str(";\n");
    sql
}

/// `CREATE FUNCTION` / `CREATE PROCEDURE` from `routines.get`.
async fn routine_ddl(client: &Client, object: &ObjectRef, dataset: &str) -> DbResult<String> {
    let routine = client.get(&object.database, &["datasets", dataset, "routines", &object.name], &[]).await?;
    let arguments = routine["arguments"].as_array().map(Vec::as_slice).unwrap_or_default();
    let type_of = |t: &Value| t["typeKind"].as_str().unwrap_or("ANY TYPE").to_owned();
    let args = arguments
        .iter()
        .map(|a| format!("{} {}", D.display_ident(a["name"].as_str().unwrap_or_default()), type_of(&a["dataType"])))
        .collect::<Vec<_>>()
        .join(", ");
    let name = D.display_name(object);
    let body = routine["definitionBody"].as_str().unwrap_or_default().trim();
    if object.kind == NodeKind::Procedure {
        return Ok(format!("CREATE PROCEDURE {name}({args})\n{body};\n"));
    }
    let returns = routine.get("returnType").map(|t| format!(" RETURNS {}", type_of(t))).unwrap_or_default();
    Ok(match routine["language"].as_str() {
        Some("SQL") | None => format!("CREATE FUNCTION {name}({args}){returns} AS (\n  {body}\n);\n"),
        Some(language) => {
            format!(
                "CREATE FUNCTION {name}({args}){returns}\nLANGUAGE {} AS r\"\"\"\n{body}\n\"\"\";\n",
                language.to_ascii_lowercase()
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builds_table_definitions_with_layout() {
        let object = ObjectRef::new(NodeKind::Table, "my-project", Some("sales".into()), "orders");
        let table = json!({
            "type": "TABLE",
            "description": "Orders",
            "schema": { "fields": [
                { "name": "id", "type": "INTEGER", "mode": "REQUIRED" },
                { "name": "created", "type": "TIMESTAMP", "defaultValueExpression": "CURRENT_TIMESTAMP()" },
                { "name": "tags", "type": "STRING", "mode": "REPEATED", "description": "it's" } ] },
            "timePartitioning": { "type": "DAY", "field": "created" },
            "clustering": { "fields": ["id"] },
            "tableConstraints": {
                "primaryKey": { "columns": ["id"] },
                "foreignKeys": [{ "name": "fk_c", "referencedTable": { "projectId": "my-project", "datasetId": "crm", "tableId": "customers" },
                    "columnReferences": [{ "referencingColumn": "id", "referencedColumn": "order_id" }] }] }
        });
        assert_eq!(
            table_ddl(&object, &table),
            "CREATE TABLE sales.orders (\n  id INT64 NOT NULL,\n  created TIMESTAMP DEFAULT CURRENT_TIMESTAMP(),\n  \
             tags ARRAY<STRING> OPTIONS(description = 'it\\'s'),\n  PRIMARY KEY (id) NOT ENFORCED,\n  \
             CONSTRAINT fk_c FOREIGN KEY (id) REFERENCES crm.customers (order_id) NOT ENFORCED\n)\n\
             PARTITION BY DATE(created)\nCLUSTER BY id\nOPTIONS(description = 'Orders');\n"
        );
        let columns: Vec<ColumnInfo> = fields(&table).iter().map(|f| column(f, &["id".into()])).collect();
        assert!(columns[0].primary_key && !columns[0].nullable && columns[0].numeric);
        assert!(!columns[2].editable, "arrays are not edited as text");
        let range = json!({ "rangePartitioning": { "field": "id", "range": { "start": "0", "end": "100", "interval": "10" } } });
        assert_eq!(
            partitioning(&range, &columns).unwrap().0,
            "PARTITION BY RANGE_BUCKET(id, GENERATE_ARRAY(0, 100, 10))"
        );
        assert_eq!(
            partitioning(&json!({ "timePartitioning": { "type": "HOUR" } }), &[]).unwrap().0,
            "PARTITION BY TIMESTAMP_TRUNC(_PARTITIONTIME, HOUR)"
        );
        assert_eq!(kind_of("SNAPSHOT").0, NodeKind::Table);
        assert_eq!(kind_of("MATERIALIZED_VIEW").0, NodeKind::MaterializedView);
    }
}
