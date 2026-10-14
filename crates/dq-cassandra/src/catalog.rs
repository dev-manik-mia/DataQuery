//! Schema from `system_schema` (the same on Cassandra and `ScyllaDB`), size
//! estimates from `system.size_estimates` when the server keeps them.

use std::collections::HashMap;

use dq_core::node::{GROUP_FUNCTIONS, GROUP_TYPES, Node, NodeKind, ObjectRef};
use dq_core::result::TextRow;
use dq_core::schema::{
    ColumnInfo, ConstraintInfo, ConstraintKind, Dependencies, Dependency, IndexInfo, KeyRole, KeyRoleKind, ModelColumn,
    ModelTable, SchemaModel, TableSize, TableStructure,
};
use dq_core::sql::{CqlDialect, Dialect};
use dq_core::{DbError, DbResult};
use scylla::client::session::Session as Driver;

use crate::client::text_rows;
use crate::values::is_numeric;

/// Keyspaces that hold the server's own data; listed after user keyspaces.
pub fn is_system(keyspace: &str) -> bool {
    keyspace.starts_with("system") || keyspace == "audit"
}

fn name(n: &str) -> String {
    CqlDialect.display_ident(n)
}

fn qualified(keyspace: &str, n: &str) -> String {
    format!("{}.{}", name(keyspace), name(n))
}

/// `["a", "b"]` (a list read as JSON text) as strings.
fn list(text: Option<String>) -> Vec<String> {
    text.and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok()).unwrap_or_default()
}

pub async fn keyspaces(driver: &Driver, current: &str) -> DbResult<Vec<Node>> {
    let mut names: Vec<String> = text_rows(driver, "SELECT keyspace_name FROM system_schema.keyspaces", ())
        .await?
        .iter()
        .map(|r| r.string(0))
        .collect();
    names.sort_by(|a, b| (is_system(a), a).cmp(&(is_system(b), b)));
    Ok(names
        .into_iter()
        .map(|ks| {
            let mut node = Node::container(ObjectRef::new(NodeKind::Database, ks.clone(), None, ks.clone()));
            node.current = ks == current;
            node
        })
        .collect())
}

/// Partitions per table from the size estimates (absent on some servers).
async fn estimates(driver: &Driver, keyspace: &str) -> HashMap<String, (i64, i64)> {
    let rows = text_rows(
        driver,
        "SELECT table_name, partitions_count, mean_partition_size FROM system.size_estimates WHERE keyspace_name = ?",
        (keyspace,),
    )
    .await
    .unwrap_or_default();
    let mut out: HashMap<String, (i64, i64)> = HashMap::new();
    for r in rows {
        let entry = out.entry(r.string(0)).or_default();
        let partitions = r.number(1).unwrap_or(0);
        entry.0 += partitions;
        entry.1 += partitions.saturating_mul(r.number(2).unwrap_or(0));
    }
    out
}

pub async fn keyspace_children(driver: &Driver, keyspace: &str) -> DbResult<Vec<Node>> {
    let (tables, views, types, functions, aggregates, sizes) = tokio::join!(
        text_rows(driver, "SELECT table_name, comment FROM system_schema.tables WHERE keyspace_name = ?", (keyspace,)),
        text_rows(driver, "SELECT view_name FROM system_schema.views WHERE keyspace_name = ?", (keyspace,)),
        text_rows(driver, "SELECT type_name FROM system_schema.types WHERE keyspace_name = ?", (keyspace,)),
        text_rows(driver, "SELECT function_name FROM system_schema.functions WHERE keyspace_name = ?", (keyspace,)),
        text_rows(driver, "SELECT aggregate_name FROM system_schema.aggregates WHERE keyspace_name = ?", (keyspace,)),
        estimates(driver, keyspace),
    );
    let mut out = Vec::new();
    for r in tables? {
        let mut node = Node::leaf(ObjectRef::new(NodeKind::Table, keyspace, None, r.string(0)));
        node.row_estimate = sizes.get(&r.string(0)).map(|s| s.0).filter(|n| *n > 0);
        node.comment = r.text(1).filter(|c| !c.is_empty());
        out.push(node);
    }
    out.extend(
        views?.iter().map(|r| Node::leaf(ObjectRef::new(NodeKind::MaterializedView, keyspace, None, r.string(0)))),
    );
    let group = |id: &str, count: usize| {
        let mut node = Node::container(ObjectRef::new(NodeKind::Group, keyspace, None, id));
        node.row_estimate = Some(count as i64);
        node
    };
    let types = types?;
    if !types.is_empty() {
        out.push(group(GROUP_TYPES, types.len()));
    }
    // Some servers have no aggregates table for user functions; treat as none.
    let routines = functions.unwrap_or_default().len() + aggregates.unwrap_or_default().len();
    if routines > 0 {
        out.push(group(GROUP_FUNCTIONS, routines));
    }
    Ok(out)
}

pub async fn group_children(driver: &Driver, group: &ObjectRef) -> DbResult<Vec<Node>> {
    let ks = group.database.as_str();
    if group.name == GROUP_TYPES {
        let rows =
            text_rows(driver, "SELECT type_name FROM system_schema.types WHERE keyspace_name = ?", (ks,)).await?;
        return Ok(rows.iter().map(|r| Node::leaf(ObjectRef::new(NodeKind::Type, ks, None, r.string(0)))).collect());
    }
    let mut out = Vec::new();
    for table in ["functions", "aggregates"] {
        let column = if table == "functions" { "function_name" } else { "aggregate_name" };
        let cql = format!("SELECT {column}, argument_types FROM system_schema.{table} WHERE keyspace_name = ?");
        for r in text_rows(driver, &cql, (ks,)).await.unwrap_or_default() {
            let args = list(r.text(1)).join(", ");
            out.push(Node::leaf(ObjectRef::new(NodeKind::Function, ks, None, r.string(0)).with_parent(args)));
        }
    }
    Ok(out)
}

pub async fn search(driver: &Driver, query: &str, limit: u32) -> DbResult<Vec<Node>> {
    let needle = query.to_lowercase();
    let mut out = Vec::new();
    for (sql, kind) in [
        ("SELECT keyspace_name, table_name FROM system_schema.tables", NodeKind::Table),
        ("SELECT keyspace_name, view_name FROM system_schema.views", NodeKind::MaterializedView),
    ] {
        for r in text_rows(driver, sql, ()).await? {
            if out.len() >= limit as usize {
                return Ok(out);
            }
            if r.string(1).to_lowercase().contains(&needle) && !is_system(&r.string(0)) {
                out.push(Node::leaf(ObjectRef::new(kind, r.string(0), None, r.string(1))));
            }
        }
    }
    Ok(out)
}

/// A column from `system_schema.columns`.
struct Col {
    name: String,
    kind: String,
    position: i64,
    descending: bool,
    data_type: String,
}

async fn columns(driver: &Driver, object: &ObjectRef) -> DbResult<Vec<Col>> {
    let rows = text_rows(
        driver,
        "SELECT column_name, kind, position, clustering_order, type FROM system_schema.columns WHERE keyspace_name = ? AND table_name = ?",
        (object.database.as_str(), object.name.as_str()),
    )
    .await?;
    if rows.is_empty() {
        return Err(DbError::not_found(format!("{} does not exist.", qualified(&object.database, &object.name))));
    }
    let mut cols: Vec<Col> = rows
        .iter()
        .map(|r: &TextRow| Col {
            name: r.string(0),
            kind: r.string(1),
            position: r.number(2).unwrap_or(-1),
            descending: r.string(3).eq_ignore_ascii_case("desc"),
            data_type: r.string(4),
        })
        .collect();
    let rank = |k: &str| match k {
        "partition_key" => 0,
        "clustering" => 1,
        "static" => 2,
        _ => 3,
    };
    cols.sort_by(|a, b| (rank(&a.kind), a.position, &a.name).cmp(&(rank(&b.kind), b.position, &b.name)));
    Ok(cols)
}

fn roles(cols: &[Col]) -> Vec<KeyRole> {
    cols.iter()
        .filter_map(|c| {
            let role = match c.kind.as_str() {
                "partition_key" => KeyRoleKind::Partition,
                "clustering" => KeyRoleKind::Clustering,
                _ => return None,
            };
            Some(KeyRole { column: c.name.clone(), role, descending: c.descending })
        })
        .collect()
}

pub async fn key_roles(driver: &Driver, object: &ObjectRef) -> DbResult<Vec<KeyRole>> {
    Ok(roles(&columns(driver, object).await?))
}

/// `PRIMARY KEY ((a, b), c DESC)`.
fn primary_key(keys: &[KeyRole]) -> String {
    let partition: Vec<String> =
        keys.iter().filter(|k| k.role == KeyRoleKind::Partition).map(|k| name(&k.column)).collect();
    let clustering: Vec<String> = keys
        .iter()
        .filter(|k| k.role == KeyRoleKind::Clustering)
        .map(|k| format!("{}{}", name(&k.column), if k.descending { " DESC" } else { "" }))
        .collect();
    let partition = if partition.len() == 1 { partition[0].clone() } else { format!("({})", partition.join(", ")) };
    if clustering.is_empty() {
        format!("PRIMARY KEY ({partition})")
    } else {
        format!("PRIMARY KEY ({partition}, {})", clustering.join(", "))
    }
}

pub async fn structure(driver: &Driver, object: &ObjectRef) -> DbResult<TableStructure> {
    let view = object.kind == NodeKind::MaterializedView;
    let (cols, indexes, comment, sizes) = tokio::join!(
        columns(driver, object),
        text_rows(
            driver,
            "SELECT index_name, options FROM system_schema.indexes WHERE keyspace_name = ? AND table_name = ?",
            (object.database.as_str(), object.name.as_str()),
        ),
        text_rows(
            driver,
            "SELECT comment FROM system_schema.tables WHERE keyspace_name = ? AND table_name = ?",
            (object.database.as_str(), object.name.as_str()),
        ),
        estimates(driver, &object.database),
    );
    let cols = cols?;
    let keys = roles(&cols);
    let columns = cols
        .iter()
        .map(|c| {
            let key = c.kind == "partition_key" || c.kind == "clustering";
            ColumnInfo {
                name: c.name.clone(),
                data_type: c.data_type.clone(),
                nullable: !key,
                default: None,
                primary_key: key,
                generated: false,
                identity: false,
                editable: !view && !matches!(c.data_type.as_str(), "counter" | "duration"),
                numeric: is_numeric(&c.data_type),
                comment: (c.kind == "static").then(|| "static".to_owned()),
                read_as_text: false,
            }
        })
        .collect();
    let indexes = indexes
        .unwrap_or_default()
        .iter()
        .map(|r| {
            let options: HashMap<String, String> =
                r.text(1).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
            let target = options.get("target").cloned().unwrap_or_default();
            IndexInfo {
                name: r.string(0),
                definition: format!(
                    "CREATE INDEX {} ON {} ({target})",
                    name(&r.string(0)),
                    qualified(&object.database, &object.name)
                ),
                unique: false,
                primary: false,
                columns: vec![target.trim_matches('"').to_owned()],
                size_bytes: None,
            }
        })
        .collect();
    let estimate = sizes.get(&object.name).copied();
    Ok(TableStructure {
        object: object.clone(),
        columns,
        constraints: vec![ConstraintInfo {
            kind: ConstraintKind::PrimaryKey,
            name: "primary key".into(),
            definition: primary_key(&keys),
        }],
        indexes,
        row_identity: if view { Vec::new() } else { keys.iter().map(|k| k.column.clone()).collect() },
        full_row_key: Vec::new(),
        comment: comment.ok().and_then(|r| r.first().and_then(|r| r.text(0))).filter(|c| !c.is_empty()),
        row_estimate: estimate.map(|e| e.0).filter(|n| *n > 0),
        foreign_keys: Vec::new(),
        size: estimate.filter(|e| e.1 > 0).map(|e| TableSize { total_bytes: e.1, table_bytes: e.1, index_bytes: 0 }),
    })
}

pub async fn row_estimate(driver: &Driver, object: &ObjectRef) -> DbResult<Option<i64>> {
    Ok(estimates(driver, &object.database).await.get(&object.name).map(|e| e.0).filter(|n| *n > 0))
}

/// The server's `DESCRIBE` (Cassandra 4+, `ScyllaDB` 5+), else rebuilt from `system_schema`.
pub async fn ddl(driver: &Driver, object: &ObjectRef) -> DbResult<String> {
    let what = match object.kind {
        NodeKind::Table => "TABLE",
        NodeKind::MaterializedView => "MATERIALIZED VIEW",
        NodeKind::Type => "TYPE",
        NodeKind::Function => "FUNCTION",
        NodeKind::Index => "INDEX",
        NodeKind::Database => "KEYSPACE",
        _ => return Err(DbError::unsupported("This object has no definition.")),
    };
    let target =
        if object.kind == NodeKind::Database { name(&object.name) } else { qualified(&object.database, &object.name) };
    let described = text_rows(driver, &format!("DESCRIBE {what} {target}"), ()).await;
    if let Ok(rows) = described {
        let statements: Vec<String> = rows.iter().filter_map(|r| r.text(3)).collect();
        if !statements.is_empty() {
            return Ok(statements.join("\n\n"));
        }
    }
    match object.kind {
        NodeKind::Table | NodeKind::MaterializedView => {
            let cols = columns(driver, object).await?;
            let keys = roles(&cols);
            let mut lines: Vec<String> = cols
                .iter()
                .map(|c| {
                    format!("  {} {}{}", name(&c.name), c.data_type, if c.kind == "static" { " STATIC" } else { "" })
                })
                .collect();
            lines.push(format!("  {}", primary_key(&keys)));
            let order: Vec<String> = keys
                .iter()
                .filter(|k| k.role == KeyRoleKind::Clustering)
                .map(|k| format!("{} {}", name(&k.column), if k.descending { "DESC" } else { "ASC" }))
                .collect();
            let with = if order.is_empty() {
                String::new()
            } else {
                format!(" WITH CLUSTERING ORDER BY ({})", order.join(", "))
            };
            Ok(format!("CREATE TABLE {target} (\n{}\n){with};", lines.join(",\n")))
        }
        NodeKind::Type => {
            let rows = text_rows(
                driver,
                "SELECT field_names, field_types FROM system_schema.types WHERE keyspace_name = ? AND type_name = ?",
                (object.database.as_str(), object.name.as_str()),
            )
            .await?;
            let row = rows.first().ok_or_else(|| DbError::not_found("The type no longer exists."))?;
            let fields: Vec<String> =
                list(row.text(0)).iter().zip(list(row.text(1))).map(|(n, t)| format!("  {} {t}", name(n))).collect();
            Ok(format!("CREATE TYPE {target} (\n{}\n);", fields.join(",\n")))
        }
        _ => Err(DbError::unsupported("The server cannot describe this object.")),
    }
}

pub async fn dependencies(driver: &Driver, object: &ObjectRef) -> DbResult<Dependencies> {
    let ks = object.database.as_str();
    let views =
        text_rows(driver, "SELECT view_name, base_table_name FROM system_schema.views WHERE keyspace_name = ?", (ks,))
            .await?;
    let mut deps = Dependencies::default();
    for v in views {
        if object.kind == NodeKind::MaterializedView && v.string(0) == object.name {
            deps.uses.push(Dependency {
                object: ObjectRef::new(NodeKind::Table, ks, None, v.string(1)),
                relation: "base table".into(),
            });
        } else if object.kind == NodeKind::Table && v.string(1) == object.name {
            deps.used_by.push(Dependency {
                object: ObjectRef::new(NodeKind::MaterializedView, ks, None, v.string(0)),
                relation: "materialized view".into(),
            });
        }
    }
    Ok(deps)
}

pub async fn schema_model(driver: &Driver, keyspace: &str) -> DbResult<SchemaModel> {
    let tables =
        text_rows(driver, "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ?", (keyspace,)).await?;
    let cols = text_rows(
        driver,
        "SELECT table_name, column_name, kind, position, type FROM system_schema.columns WHERE keyspace_name = ?",
        (keyspace,),
    )
    .await?;
    let truncated = tables.len() > SchemaModel::MAX_TABLES;
    let mut model = SchemaModel { truncated, ..SchemaModel::default() };
    for t in tables.iter().take(SchemaModel::MAX_TABLES) {
        let table = t.string(0);
        let mut columns: Vec<(i64, i64, ModelColumn)> = cols
            .iter()
            .filter(|c| c.string(0) == table)
            .map(|c| {
                let kind = c.string(2);
                let rank = match kind.as_str() {
                    "partition_key" => 0,
                    "clustering" => 1,
                    _ => 2,
                };
                let key = rank < 2;
                (
                    rank,
                    c.number(3).unwrap_or(-1),
                    ModelColumn { name: c.string(1), data_type: c.string(4), nullable: !key, primary_key: key },
                )
            })
            .collect();
        columns.sort_by(|a, b| (a.0, a.1, &a.2.name).cmp(&(b.0, b.1, &b.2.name)));
        model.tables.push(ModelTable {
            object: ObjectRef::new(NodeKind::Table, keyspace, None, table),
            columns: columns.into_iter().map(|c| c.2).collect(),
        });
    }
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_key_text() {
        let k = |c: &str, role, descending| KeyRole { column: c.into(), role, descending };
        let keys = vec![
            k("device_id", KeyRoleKind::Partition, false),
            k("bucket", KeyRoleKind::Partition, false),
            k("ts", KeyRoleKind::Clustering, true),
        ];
        assert_eq!(primary_key(&keys), "PRIMARY KEY ((device_id, bucket), ts DESC)");
        assert_eq!(primary_key(&keys[..1]), "PRIMARY KEY (device_id)");
        assert_eq!(primary_key(&[k("Id", KeyRoleKind::Partition, false)]), "PRIMARY KEY (\"Id\")");
        assert!(is_system("system_schema") && !is_system("telemetry"));
    }
}
