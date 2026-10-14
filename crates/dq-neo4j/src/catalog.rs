//! The explorer: databases → labels (as tables), relationship types (as views)
//! and indexes; structures sampled from the graph; schema statements.

use std::collections::BTreeMap;

use dq_core::DbResult;
use dq_core::node::{GROUP_INDEXES, GROUP_LABELS, GROUP_RELATIONSHIPS, Node, NodeKind, ObjectRef};
use dq_core::schema::{
    ColumnInfo, ConstraintInfo, ConstraintKind, Dependencies, Dependency, IndexInfo, ModelColumn, ModelTable,
    SchemaModel, TableStructure,
};
use dq_core::sql::{CypherDialect, Dialect};
use serde_json::Value;

use crate::client::{Client, Query};
use crate::values::{self, int, text, type_label};

/// Labels or relationship types counted individually; beyond this, counts are skipped.
const MAX_COUNTED: usize = 200;
const SAMPLE: u32 = 100;

/// A label, type or key, quoted with backticks only when needed.
pub fn quote(name: &str) -> String {
    CypherDialect.display_ident(name)
}

/// Rows of a read query as typed values.
pub async fn rows(client: &Client, database: &str, statement: &str) -> DbResult<Vec<Vec<Value>>> {
    Ok(client.run(&Query::read(database, statement), false).await?.values)
}

fn column_text(row: &[Value], i: usize) -> String {
    row.get(i).and_then(text).unwrap_or_default()
}

pub async fn databases(client: &Client, current: &str) -> DbResult<Vec<Node>> {
    let rows =
        rows(client, current, "SHOW DATABASES YIELD name, currentStatus RETURN DISTINCT name, currentStatus").await?;
    let mut nodes: Vec<Node> = rows
        .iter()
        .map(|r| {
            let name = column_text(r, 0);
            let mut node = Node::container(ObjectRef::new(NodeKind::Database, name.clone(), None, name.clone()));
            node.current = name == current;
            let status = column_text(r, 1);
            node.comment = (status != "online").then_some(status);
            node
        })
        .collect();
    // `system` holds users and databases, not graph data: last.
    nodes.sort_by_key(|n| (n.object.name == "system", n.object.name.clone()));
    Ok(nodes)
}

pub async fn labels(client: &Client, database: &str) -> DbResult<Vec<String>> {
    let rows = rows(client, database, "CALL db.labels() YIELD label RETURN label ORDER BY label").await?;
    Ok(rows.iter().map(|r| column_text(r, 0)).collect())
}

pub async fn relationship_types(client: &Client, database: &str) -> DbResult<Vec<String>> {
    let rows = rows(
        client,
        database,
        "CALL db.relationshipTypes() YIELD relationshipType RETURN relationshipType ORDER BY relationshipType",
    )
    .await?;
    Ok(rows.iter().map(|r| column_text(r, 0)).collect())
}

/// Nodes with a label or relationships of a type, from the count store.
pub async fn count(client: &Client, object: &ObjectRef) -> DbResult<Option<i64>> {
    let statement = match object.kind {
        NodeKind::Table => format!("MATCH (n:{}) RETURN count(n)", quote(&object.name)),
        NodeKind::View => format!("MATCH ()-[r:{}]->() RETURN count(r)", quote(&object.name)),
        _ => return Ok(None),
    };
    Ok(rows(client, &object.database, &statement).await?.first().and_then(|r| r.first()).and_then(int))
}

/// One index or constraint from `SHOW INDEXES` / `SHOW CONSTRAINTS`.
#[derive(Debug, Clone)]
pub struct SchemaItem {
    pub name: String,
    pub kind: String,
    pub entity: String,
    pub owners: Vec<String>,
    pub properties: Vec<String>,
    pub state: Option<String>,
    pub constraint: bool,
    /// For indexes: the constraint that owns it.
    pub owning_constraint: Option<String>,
    pub create_statement: String,
}

fn list(v: Option<&Value>) -> Vec<String> {
    v.and_then(|v| v.get("_value"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(text).collect())
        .unwrap_or_default()
}

pub async fn schema_items(client: &Client, database: &str) -> DbResult<Vec<SchemaItem>> {
    let mut out = Vec::new();
    let indexes = rows(
        client,
        database,
        "SHOW INDEXES YIELD name, type, entityType, labelsOrTypes, properties, state, owningConstraint, createStatement",
    )
    .await?;
    for r in &indexes {
        out.push(SchemaItem {
            name: column_text(r, 0),
            kind: column_text(r, 1),
            entity: column_text(r, 2),
            owners: list(r.get(3)),
            properties: list(r.get(4)),
            state: r.get(5).and_then(text),
            constraint: false,
            owning_constraint: r.get(6).and_then(text),
            create_statement: column_text(r, 7),
        });
    }
    let constraints = rows(
        client,
        database,
        "SHOW CONSTRAINTS YIELD name, type, entityType, labelsOrTypes, properties, createStatement",
    )
    .await?;
    for r in &constraints {
        out.push(SchemaItem {
            name: column_text(r, 0),
            kind: column_text(r, 1),
            entity: column_text(r, 2),
            owners: list(r.get(3)),
            properties: list(r.get(4)),
            state: None,
            constraint: true,
            owning_constraint: None,
            create_statement: column_text(r, 5),
        });
    }
    Ok(out)
}

/// Indexes and constraints on a label or relationship type.
pub async fn items_for(client: &Client, object: &ObjectRef) -> DbResult<Vec<SchemaItem>> {
    let entity = if object.kind == NodeKind::View { "RELATIONSHIP" } else { "NODE" };
    Ok(schema_items(client, &object.database)
        .await?
        .into_iter()
        .filter(|i| i.entity == entity && i.owners.contains(&object.name))
        .collect())
}

pub async fn children(client: &Client, parent: &ObjectRef) -> DbResult<Vec<Node>> {
    let db = parent.database.as_str();
    match (parent.kind, parent.name.as_str()) {
        (NodeKind::Database, _) => {
            let (labels, types, items) =
                tokio::try_join!(labels(client, db), relationship_types(client, db), schema_items(client, db))?;
            let indexes = items.iter().filter(|i| !i.constraint).count();
            let group = |name: &str, n: usize| {
                let mut node = Node::container(ObjectRef::new(NodeKind::Group, db, None, name));
                node.row_estimate = Some(n as i64);
                node
            };
            Ok(vec![
                group(GROUP_LABELS, labels.len()),
                group(GROUP_RELATIONSHIPS, types.len()),
                group(GROUP_INDEXES, indexes),
            ])
        }
        (NodeKind::Group, GROUP_LABELS) => counted(client, db, labels(client, db).await?, NodeKind::Table).await,
        (NodeKind::Group, GROUP_RELATIONSHIPS) => {
            counted(client, db, relationship_types(client, db).await?, NodeKind::View).await
        }
        (NodeKind::Group, GROUP_INDEXES) => Ok(schema_items(client, db)
            .await?
            .into_iter()
            .filter(|i| !i.constraint)
            .map(|i| {
                let mut object = ObjectRef::new(NodeKind::Index, db, None, i.name.clone());
                if let Some(owner) = i.owners.first() {
                    object = object.with_parent(owner.clone());
                }
                let mut node = Node::leaf(object);
                node.comment = Some(format!("{} {}", i.kind, i.state.unwrap_or_default()).trim().to_owned());
                node
            })
            .collect()),
        _ => Ok(Vec::new()),
    }
}

async fn counted(client: &Client, db: &str, names: Vec<String>, kind: NodeKind) -> DbResult<Vec<Node>> {
    let mut out = Vec::with_capacity(names.len());
    for (i, name) in names.into_iter().enumerate() {
        let object = ObjectRef::new(kind, db, None, name);
        let mut node = Node::leaf(object);
        if i < MAX_COUNTED {
            node.row_estimate = count(client, &node.object).await.ok().flatten();
        }
        out.push(node);
    }
    Ok(out)
}

pub async fn search(client: &Client, database: &str, query: &str, limit: u32) -> DbResult<Vec<Node>> {
    let needle = query.to_lowercase();
    let (labels, types) = tokio::try_join!(labels(client, database), relationship_types(client, database))?;
    Ok(labels
        .into_iter()
        .map(|l| (NodeKind::Table, l))
        .chain(types.into_iter().map(|t| (NodeKind::View, t)))
        .filter(|(_, n)| n.to_lowercase().contains(&needle))
        .take(limit as usize)
        .map(|(k, n)| Node::leaf(ObjectRef::new(k, database, None, n)))
        .collect())
}

/// Properties of up to [`SAMPLE`] nodes or relationships: name → most common type.
pub async fn sampled_properties(client: &Client, object: &ObjectRef, sample: u32) -> DbResult<Vec<(String, String)>> {
    let statement = match object.kind {
        NodeKind::View => format!("MATCH ()-[n:{}]->() RETURN n LIMIT {sample}", quote(&object.name)),
        _ => format!("MATCH (n:{}) RETURN n LIMIT {sample}", quote(&object.name)),
    };
    let mut seen: BTreeMap<String, BTreeMap<&'static str, u32>> = BTreeMap::new();
    for row in rows(client, &object.database, &statement).await? {
        let Some(props) =
            row.first().and_then(values::entity).and_then(|e| e.get("_properties")).and_then(Value::as_object)
        else {
            continue;
        };
        for (k, v) in props {
            *seen.entry(k.clone()).or_default().entry(type_label(v)).or_default() += 1;
        }
    }
    Ok(seen
        .into_iter()
        .map(|(k, types)| {
            let t = types.into_iter().max_by_key(|(_, n)| *n).map_or("Null", |(t, _)| t);
            (k, t.to_owned())
        })
        .collect())
}

fn column(name: &str, data_type: &str, key: bool) -> ColumnInfo {
    ColumnInfo {
        name: name.to_owned(),
        data_type: data_type.to_owned(),
        nullable: !key,
        default: None,
        primary_key: key,
        generated: key,
        identity: false,
        // Properties take imported values; grid editing is off for Neo4j.
        editable: !key,
        numeric: values::is_numeric(data_type),
        comment: None,
        read_as_text: false,
    }
}

pub async fn structure(client: &Client, object: &ObjectRef) -> DbResult<TableStructure> {
    if !matches!(object.kind, NodeKind::Table | NodeKind::View) {
        return Err(dq_core::DbError::unsupported("Only labels and relationship types have a structure."));
    }
    let (props, items, estimate) =
        tokio::try_join!(sampled_properties(client, object, SAMPLE), items_for(client, object), count(client, object))?;
    let mut columns = vec![column("elementId", "String", true)];
    if object.kind == NodeKind::View {
        columns.push(column("start", "String", false));
        columns.push(column("end", "String", false));
    }
    columns.extend(props.iter().map(|(n, t)| column(n, t, false)));
    let constraints = items
        .iter()
        .filter(|i| i.constraint)
        .map(|i| ConstraintInfo {
            kind: match i.kind.as_str() {
                k if k.contains("UNIQUENESS") => ConstraintKind::Unique,
                k if k.contains("KEY") => ConstraintKind::PrimaryKey,
                _ => ConstraintKind::Check,
            },
            name: i.name.clone(),
            definition: i.create_statement.clone(),
        })
        .collect();
    let indexes = items
        .iter()
        .filter(|i| !i.constraint)
        .map(|i| IndexInfo {
            name: i.name.clone(),
            definition: i.create_statement.clone(),
            unique: i.owning_constraint.is_some(),
            primary: false,
            columns: i.properties.clone(),
            size_bytes: None,
        })
        .collect();
    Ok(TableStructure {
        object: object.clone(),
        columns,
        constraints,
        indexes,
        row_identity: vec!["elementId".to_owned()],
        full_row_key: Vec::new(),
        comment: None,
        row_estimate: estimate,
        foreign_keys: Vec::new(),
        size: None,
    })
}

pub async fn ddl(client: &Client, object: &ObjectRef) -> DbResult<String> {
    let statements: Vec<String> = match object.kind {
        NodeKind::Index => schema_items(client, &object.database)
            .await?
            .into_iter()
            .filter(|i| i.name == object.name)
            .map(|i| i.create_statement)
            .collect(),
        NodeKind::Table | NodeKind::View => items_for(client, object)
            .await?
            .into_iter()
            .filter(|i| i.owning_constraint.is_none())
            .map(|i| i.create_statement)
            .collect(),
        _ => Vec::new(),
    };
    if statements.is_empty() {
        let what =
            if object.kind == NodeKind::View { format!("[:{}]", object.name) } else { format!(":{}", object.name) };
        return Ok(format!("// No indexes or constraints on {what}.\n"));
    }
    Ok(statements.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n") + "\n")
}

/// Labels and relationship types connected in `db.schema.visualization()`.
pub async fn dependencies(client: &Client, object: &ObjectRef) -> DbResult<Dependencies> {
    let rows = rows(client, &object.database, "CALL db.schema.visualization()").await?;
    let Some(row) = rows.first() else {
        return Ok(Dependencies::default());
    };
    let entities = |i: usize| -> Vec<Value> {
        row.get(i).and_then(|v| v.get("_value")).and_then(Value::as_array).cloned().unwrap_or_default()
    };
    let label_of: BTreeMap<String, String> = entities(0)
        .iter()
        .filter_map(values::entity)
        .filter_map(|n| {
            let id = n.get("_element_id")?.as_str()?.to_owned();
            let label = n.get("_labels")?.as_array()?.first()?.as_str()?.to_owned();
            Some((id, label))
        })
        .collect();
    let mut deps = Dependencies::default();
    for rel in entities(1).iter().filter_map(values::entity) {
        let ty = rel.get("_type").and_then(Value::as_str).unwrap_or_default();
        let end =
            |k: &str| rel.get(k).and_then(Value::as_str).and_then(|id| label_of.get(id)).cloned().unwrap_or_default();
        let (start, finish) = (end("_start_node_element_id"), end("_end_node_element_id"));
        let rel_ref = ObjectRef::new(NodeKind::View, object.database.clone(), None, ty);
        let label_ref = |l: &str| ObjectRef::new(NodeKind::Table, object.database.clone(), None, l);
        match object.kind {
            NodeKind::Table if start == object.name => {
                deps.uses.push(Dependency { object: rel_ref, relation: format!("outgoing to :{finish}") });
            }
            NodeKind::Table if finish == object.name => {
                deps.used_by.push(Dependency { object: rel_ref, relation: format!("incoming from :{start}") });
            }
            NodeKind::View if ty == object.name => {
                deps.uses.push(Dependency { object: label_ref(&start), relation: "start node".into() });
                deps.uses.push(Dependency { object: label_ref(&finish), relation: "end node".into() });
            }
            _ => {}
        }
    }
    Ok(deps)
}

pub async fn schema_model(client: &Client, database: &str) -> DbResult<SchemaModel> {
    let labels = labels(client, database).await?;
    let truncated = labels.len() > SchemaModel::MAX_TABLES;
    let mut tables = Vec::new();
    let mut columns = 0;
    for label in labels.into_iter().take(SchemaModel::MAX_TABLES) {
        let object = ObjectRef::new(NodeKind::Table, database, None, label);
        let props = sampled_properties(client, &object, 20).await.unwrap_or_default();
        columns += props.len();
        if columns > SchemaModel::MAX_COLUMNS {
            break;
        }
        tables.push(ModelTable {
            object,
            columns: props
                .into_iter()
                .map(|(name, data_type)| ModelColumn { name, data_type, nullable: true, primary_key: false })
                .collect(),
        });
    }
    Ok(SchemaModel { tables, foreign_keys: Vec::new(), truncated })
}
