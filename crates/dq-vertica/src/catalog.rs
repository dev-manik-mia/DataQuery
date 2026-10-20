//! Catalog reads from `v_catalog` (schemas, tables, views, columns,
//! constraints, projections, sequences, functions) and `v_monitor`
//! (projection storage for sizes and row estimates). Values travel as bound
//! parameters; definitions come from `EXPORT_OBJECTS`.

use dq_core::node::{GROUP_FUNCTIONS, GROUP_SEQUENCES, Node, NodeKind, ObjectRef};
use dq_core::result::TextRow;
use dq_core::schema::{
    ColumnInfo, ConstraintInfo, ConstraintKind, Dependencies, Dependency, ForeignKey, IndexInfo, ModelColumn,
    ModelTable, SchemaModel, TableSize, TableStructure,
};
use dq_core::sql::{Dialect, VerticaDialect};
use dq_core::{DbError, DbResult};

use crate::conn::Connection;

const D: VerticaDialect = VerticaDialect;

fn schema_of(object: &ObjectRef) -> DbResult<&str> {
    object.schema.as_deref().ok_or_else(|| DbError::invalid("A schema is required."))
}

fn not_found(object: &ObjectRef) -> DbError {
    DbError::not_found(format!("{} no longer exists.", D.display_name(object)))
}

fn kind_of(kind: &str) -> NodeKind {
    if kind == "VIEW" { NodeKind::View } else { NodeKind::Table }
}

/// The cluster's one database.
pub fn database(name: &str) -> Node {
    let mut node = Node::container(ObjectRef::new(NodeKind::Database, name, None, name));
    node.current = true;
    node
}

pub async fn schemas(conn: &mut Connection, database: &str) -> DbResult<Vec<Node>> {
    let rows = conn
        .text_rows(
            "SELECT schema_name FROM v_catalog.schemata WHERE NOT is_system_schema \
             ORDER BY schema_name <> 'public', schema_name",
            &[],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| Node::container(ObjectRef::new(NodeKind::Schema, database, Some(r.string(0)), r.string(0))))
        .collect())
}

/// Tables and views of a schema, or the ones whose name contains a pattern:
/// name, `TABLE`/`VIEW`, comment, schema.
const RELATIONS: &str = "SELECT r.name, r.kind, c.comment, r.schema FROM ( \
       SELECT table_schema AS schema, table_name AS name, 'TABLE' AS kind FROM v_catalog.tables \
       WHERE NOT is_system_table AND NOT is_temp_table \
       UNION ALL SELECT table_schema, table_name, 'VIEW' FROM v_catalog.views WHERE NOT is_system_view \
     ) r LEFT JOIN v_catalog.comments c ON c.object_schema = r.schema AND c.object_name = r.name \
       AND c.object_type = r.kind";

/// Rows per table from the storage of its projections (the smallest total, so
/// buddy projections are not counted twice); `anchor_table_schema = ?` follows.
const ROW_COUNTS: &str = "SELECT anchor_table_name, MIN(rows)::INT FROM ( \
       SELECT anchor_table_name, projection_id, SUM(row_count) AS rows FROM v_monitor.projection_storage \
       WHERE anchor_table_schema = ? GROUP BY anchor_table_name, projection_id) p GROUP BY anchor_table_name";

fn relation_node(database: &str, r: &TextRow) -> Node {
    let object = ObjectRef::new(kind_of(&r.string(1)), database, Some(r.string(3)), r.string(0));
    let mut node = Node::leaf(object);
    node.comment = r.text(2).filter(|c| !c.is_empty());
    node
}

/// Sequences (not those behind `IDENTITY` columns) and SQL functions, as tree groups.
const GROUPS: &[(&str, &str, NodeKind)] = &[
    (
        GROUP_SEQUENCES,
        "SELECT sequence_name FROM v_catalog.sequences WHERE sequence_schema = ? AND identity_table_name IS NULL \
         ORDER BY 1",
        NodeKind::Sequence,
    ),
    (
        GROUP_FUNCTIONS,
        "SELECT DISTINCT function_name FROM v_catalog.user_functions WHERE schema_name = ? ORDER BY 1",
        NodeKind::Function,
    ),
];

pub async fn relations(conn: &mut Connection, database: &str, schema: &str) -> DbResult<Vec<Node>> {
    let sql = format!("{RELATIONS} WHERE r.schema = ? ORDER BY r.kind, r.name");
    let mut nodes: Vec<Node> =
        conn.text_rows(&sql, &[schema]).await?.iter().map(|r| relation_node(database, r)).collect();
    // Storage figures need the SYSMONITOR role or ownership; without them the tree shows no estimates.
    if let Ok(counts) = conn.text_rows(ROW_COUNTS, &[schema]).await {
        for count in counts {
            if let Some(node) =
                nodes.iter_mut().find(|n| n.object.kind == NodeKind::Table && n.object.name == count.string(0))
            {
                node.row_estimate = count.number(1);
            }
        }
    }
    for (group, sql, _) in GROUPS {
        let count = conn.text_rows(sql, &[schema]).await.map_or(0, |r| r.len());
        if count > 0 {
            let mut node = Node::container(ObjectRef::new(NodeKind::Group, database, Some(schema.to_owned()), *group));
            node.row_estimate = Some(count as i64);
            nodes.push(node);
        }
    }
    Ok(nodes)
}

pub async fn group_children(conn: &mut Connection, group: &ObjectRef) -> DbResult<Vec<Node>> {
    let schema = schema_of(group)?;
    let (_, sql, kind) = GROUPS
        .iter()
        .find(|(name, ..)| *name == group.name)
        .ok_or_else(|| DbError::invalid(format!("Unknown group `{}`.", group.name)))?;
    Ok(conn
        .text_rows(sql, &[schema])
        .await?
        .iter()
        .map(|r| Node::leaf(ObjectRef::new(*kind, group.database.clone(), group.schema.clone(), r.string(0))))
        .collect())
}

pub async fn search(conn: &mut Connection, database: &str, query: &str, limit: u32) -> DbResult<Vec<Node>> {
    let pattern = dq_core::sql::builder::like_contains(query);
    let sql = format!(
        "{RELATIONS} WHERE r.name ILIKE ? ESCAPE '\\' ORDER BY LENGTH(r.name), r.name LIMIT {}",
        limit.min(1_000)
    );
    Ok(conn.text_rows(&sql, &[&pattern]).await?.iter().map(|r| relation_node(database, r)).collect())
}

async fn relation(conn: &mut Connection, object: &ObjectRef) -> DbResult<TextRow> {
    let sql = format!("{RELATIONS} WHERE r.schema = ? AND r.name = ?");
    conn.text_rows(&sql, &[schema_of(object)?, &object.name]).await?.into_iter().next().ok_or_else(|| not_found(object))
}

pub async fn row_estimate(conn: &mut Connection, object: &ObjectRef) -> DbResult<Option<i64>> {
    let schema = schema_of(object)?;
    Ok(conn
        .text_rows(ROW_COUNTS, &[schema])
        .await
        .unwrap_or_default()
        .iter()
        .find(|r| r.string(0) == object.name)
        .and_then(|r| r.number(1)))
}

const NUMERIC: &[&str] = &[
    "int",
    "integer",
    "bigint",
    "smallint",
    "tinyint",
    "float",
    "numeric",
    "decimal",
    "number",
    "money",
    "real",
    "double precision",
];

fn base(data_type: &str) -> String {
    data_type.split('(').next().unwrap_or(data_type).trim().to_ascii_lowercase()
}

/// Spatial types, which the grid reads as WKT text.
fn spatial(data_type: &str) -> bool {
    matches!(base(data_type).as_str(), "geometry" | "geography")
}

/// Spatial and complex types: no text round-trip for edits.
fn not_editable(data_type: &str) -> bool {
    let b = base(data_type);
    spatial(&b) || ["array", "set", "row", "map"].iter().any(|p| b.starts_with(p))
}

/// Columns of a table (`v_catalog.columns`) or a view (`view_columns`):
/// name, type, nullable, default, identity.
async fn column_rows(conn: &mut Connection, object: &ObjectRef, kind: NodeKind) -> DbResult<Vec<TextRow>> {
    let sql = if kind == NodeKind::View {
        "SELECT column_name, data_type, true, NULL, false FROM v_catalog.view_columns \
         WHERE table_schema = ? AND table_name = ? ORDER BY ordinal_position"
    } else {
        "SELECT column_name, data_type, is_nullable, column_default, is_identity FROM v_catalog.columns \
         WHERE table_schema = ? AND table_name = ? ORDER BY ordinal_position"
    };
    conn.text_rows(sql, &[schema_of(object)?, &object.name]).await
}

/// Constraint columns of tables in a schema (one table when `table` is set):
/// table, constraint, type (`p`, `f`, `u`, `c`), column, referenced schema,
/// table and column, check predicate. Key columns come in key order.
async fn constraint_rows(conn: &mut Connection, schema: &str, table: Option<&str>) -> DbResult<Vec<TextRow>> {
    let filter = if table.is_some() { " AND cc.table_name = ?" } else { "" };
    let sql = format!(
        "SELECT cc.table_name, cc.constraint_name, cc.constraint_type, cc.column_name, \
                cc.reference_table_schema, cc.reference_table_name, cc.reference_column_name, tc.predicate \
         FROM v_catalog.constraint_columns cc \
         LEFT JOIN v_catalog.table_constraints tc ON tc.constraint_id = cc.constraint_id \
         LEFT JOIN v_catalog.primary_keys pk ON cc.constraint_type = 'p' AND pk.constraint_name = cc.constraint_name \
              AND pk.table_schema = cc.table_schema AND pk.table_name = cc.table_name AND pk.column_name = cc.column_name \
         LEFT JOIN v_catalog.foreign_keys fk ON cc.constraint_type = 'f' AND fk.constraint_name = cc.constraint_name \
              AND fk.table_schema = cc.table_schema AND fk.table_name = cc.table_name AND fk.column_name = cc.column_name \
         WHERE cc.table_schema = ?{filter} AND cc.constraint_type IN ('p', 'f', 'u', 'c') \
         ORDER BY cc.table_name, cc.constraint_name, COALESCE(pk.ordinal_position, fk.ordinal_position), cc.column_name"
    );
    let mut params = vec![schema];
    params.extend(table);
    conn.text_rows(&sql, &params).await
}

/// One constraint with its columns in order.
struct Constraint {
    table: String,
    name: String,
    kind: String,
    columns: Vec<String>,
    target: Option<(String, String)>,
    target_columns: Vec<String>,
    predicate: Option<String>,
}

fn group_constraints(rows: &[TextRow]) -> Vec<Constraint> {
    let mut out: Vec<Constraint> = Vec::new();
    for r in rows {
        let (table, name) = (r.string(0), r.string(1));
        if out.last().is_none_or(|c| c.table != table || c.name != name) {
            out.push(Constraint {
                table,
                name,
                kind: r.string(2),
                columns: Vec::new(),
                target: r.text(5).map(|t| (r.string(4), t)),
                target_columns: Vec::new(),
                predicate: r.text(7),
            });
        }
        let c = out.last_mut().expect("pushed above");
        c.columns.extend(r.text(3).filter(|col| !c.columns.contains(col)));
        c.target_columns.extend(r.text(6));
    }
    out
}

fn foreign_key(database: &str, schema: &str, c: &Constraint) -> Option<ForeignKey> {
    let (target_schema, target_table) = c.target.clone()?;
    Some(ForeignKey {
        name: c.name.clone(),
        source: ObjectRef::new(NodeKind::Table, database, Some(schema.to_owned()), c.table.clone()),
        columns: c.columns.clone(),
        target: ObjectRef::new(NodeKind::Table, database, Some(target_schema), target_table),
        target_columns: c.target_columns.clone(),
        on_update: None,
        on_delete: None,
    })
}

fn list(columns: &[String]) -> String {
    columns.iter().map(|c| D.display_ident(c)).collect::<Vec<_>>().join(", ")
}

/// Projections of a table, shown as its indexes: sort order, segmentation and storage.
async fn projections(conn: &mut Connection, object: &ObjectRef) -> DbResult<Vec<IndexInfo>> {
    let schema = schema_of(object)?;
    let found = conn
        .text_rows(
            "SELECT p.projection_name, p.is_segmented, p.segment_expression, p.is_super_projection, \
                    (SELECT SUM(s.used_bytes) FROM v_monitor.projection_storage s \
                     WHERE s.projection_id = p.projection_id)::INT \
             FROM v_catalog.projections p WHERE p.projection_schema = ? AND p.anchor_table_name = ? \
             ORDER BY p.is_super_projection DESC, p.projection_name",
            &[schema, &object.name],
        )
        .await?;
    let sort = conn
        .text_rows(
            "SELECT projection_name, table_column_name FROM v_catalog.projection_columns \
             WHERE table_schema = ? AND table_name = ? AND sort_position IS NOT NULL \
             ORDER BY projection_name, sort_position",
            &[schema, &object.name],
        )
        .await?;
    Ok(found
        .iter()
        .map(|p| {
            let name = p.string(0);
            let columns: Vec<String> = sort.iter().filter(|s| s.string(0) == name).map(|s| s.string(1)).collect();
            let segmentation = if p.flag(1) {
                format!("SEGMENTED BY {} ALL NODES", p.string(2))
            } else {
                "UNSEGMENTED ALL NODES".to_owned()
            };
            let order = if columns.is_empty() { String::new() } else { format!("ORDER BY {} ", list(&columns)) };
            let kind = if p.flag(3) { "super projection" } else { "projection" };
            IndexInfo {
                definition: format!("{kind}: {order}{segmentation}"),
                unique: false,
                primary: false,
                size_bytes: p.number(4),
                columns,
                name,
            }
        })
        .collect())
}

pub async fn structure(conn: &mut Connection, object: &ObjectRef) -> DbResult<TableStructure> {
    let rel = relation(conn, object).await?;
    let kind = kind_of(&rel.string(1));
    let schema = schema_of(object)?;
    let constraints = if kind == NodeKind::Table {
        group_constraints(&constraint_rows(conn, schema, Some(&object.name)).await?)
    } else {
        Vec::new()
    };
    let primary: Vec<String> =
        constraints.iter().find(|c| c.kind == "p").map(|c| c.columns.clone()).unwrap_or_default();
    let columns: Vec<ColumnInfo> = column_rows(conn, object, kind)
        .await?
        .iter()
        .map(|r| {
            let name = r.string(0);
            let data_type = r.string(1);
            let identity = r.flag(4);
            let default = r.text(3).filter(|d| !d.is_empty());
            ColumnInfo {
                nullable: r.flag(2),
                primary_key: primary.contains(&name),
                generated: identity,
                identity,
                // Vertica refuses values for identity columns.
                editable: !not_editable(&data_type) && !identity,
                numeric: NUMERIC.contains(&base(&data_type).as_str()),
                comment: None,
                read_as_text: spatial(&data_type),
                default,
                data_type,
                name,
            }
        })
        .collect();
    let infos = constraints
        .iter()
        .map(|c| {
            let (kind, definition) = match c.kind.as_str() {
                "p" => (ConstraintKind::PrimaryKey, format!("PRIMARY KEY ({})", list(&c.columns))),
                "u" => (ConstraintKind::Unique, format!("UNIQUE ({})", list(&c.columns))),
                "f" => {
                    let (s, t) = c.target.clone().unwrap_or_default();
                    let target = ObjectRef::new(NodeKind::Table, &object.database, Some(s), t);
                    (
                        ConstraintKind::ForeignKey,
                        format!(
                            "FOREIGN KEY ({}) REFERENCES {} ({})",
                            list(&c.columns),
                            D.display_name(&target),
                            list(&c.target_columns)
                        ),
                    )
                }
                _ => (ConstraintKind::Check, format!("CHECK ({})", c.predicate.clone().unwrap_or_default())),
            };
            ConstraintInfo { kind, name: c.name.clone(), definition }
        })
        .collect();
    let foreign_keys: Vec<ForeignKey> =
        constraints.iter().filter(|c| c.kind == "f").filter_map(|c| foreign_key(&object.database, schema, c)).collect();
    let (indexes, row_estimate) = if kind == NodeKind::Table {
        (projections(conn, object).await.unwrap_or_default(), row_estimate(conn, object).await?)
    } else {
        (Vec::new(), None)
    };
    let stored: i64 = indexes.iter().filter_map(|i| i.size_bytes).sum();
    Ok(TableStructure {
        object: ObjectRef { kind, ..object.clone() },
        columns,
        constraints: infos,
        indexes,
        row_identity: if kind == NodeKind::Table { primary } else { Vec::new() },
        full_row_key: Vec::new(),
        comment: rel.text(2).filter(|c| !c.is_empty()),
        row_estimate,
        foreign_keys,
        size: (kind == NodeKind::Table).then_some(TableSize {
            total_bytes: stored,
            table_bytes: stored,
            index_bytes: 0,
        }),
    })
}

pub async fn dependencies(conn: &mut Connection, object: &ObjectRef) -> DbResult<Dependencies> {
    if !object.kind.is_relation() {
        return Ok(Dependencies::default());
    }
    let schema = schema_of(object)?;
    let mut deps = Dependencies::default();
    let rows = constraint_rows(conn, schema, None).await?;
    let table = |s: String, t: String| ObjectRef::new(NodeKind::Table, &object.database, Some(s), t);
    for c in group_constraints(&rows).iter().filter(|c| c.kind == "f") {
        let Some((target_schema, target)) = c.target.clone() else { continue };
        if c.table == object.name {
            deps.uses.push(Dependency {
                object: table(target_schema.clone(), target.clone()),
                relation: format!("foreign key {}", c.name),
            });
        }
        if target_schema == schema && target == object.name {
            deps.used_by.push(Dependency {
                object: table(schema.to_owned(), c.table.clone()),
                relation: format!("foreign key {}", c.name),
            });
        }
    }
    // Foreign keys from other schemas.
    let others = conn
        .text_rows(
            "SELECT DISTINCT table_schema, table_name, constraint_name FROM v_catalog.foreign_keys \
             WHERE reference_table_schema = ? AND reference_table_name = ? AND table_schema <> ?",
            &[schema, &object.name, schema],
        )
        .await?;
    for r in others {
        deps.used_by.push(Dependency {
            object: table(r.string(0), r.string(1)),
            relation: format!("foreign key {}", r.string(2)),
        });
    }
    let views = conn
        .text_rows(
            "SELECT table_schema, table_name, reference_table_schema, reference_table_name FROM v_catalog.view_tables \
             WHERE (table_schema = ? AND table_name = ?) OR (reference_table_schema = ? AND reference_table_name = ?)",
            &[schema, &object.name, schema, &object.name],
        )
        .await?;
    for r in views {
        if r.string(0) == schema && r.string(1) == object.name {
            deps.uses.push(Dependency { object: table(r.string(2), r.string(3)), relation: "view".into() });
        } else {
            let view = ObjectRef::new(NodeKind::View, &object.database, Some(r.string(0)), r.string(1));
            deps.used_by.push(Dependency { object: view, relation: "view".into() });
        }
    }
    Ok(deps)
}

pub async fn schema_model(conn: &mut Connection, database: &str, schema: Option<&str>) -> DbResult<SchemaModel> {
    let filter = if schema.is_some() { "c.schema = ?" } else { "NOT s.is_system_schema" };
    let sql = format!(
        "SELECT c.schema, c.name, c.kind, c.column_name, c.data_type, c.nullable FROM ( \
           SELECT table_schema AS schema, table_name AS name, 'TABLE' AS kind, column_name, data_type, \
                  is_nullable AS nullable, ordinal_position FROM v_catalog.columns \
           UNION ALL SELECT table_schema, table_name, 'VIEW', column_name, data_type, true, ordinal_position \
           FROM v_catalog.view_columns \
         ) c JOIN v_catalog.schemata s ON s.schema_name = c.schema \
         WHERE {filter} ORDER BY c.schema, c.name, c.ordinal_position LIMIT {}",
        SchemaModel::MAX_COLUMNS
    );
    let found = conn.text_rows(&sql, &schema.into_iter().collect::<Vec<_>>()).await?;
    let mut model = SchemaModel::default();
    let mut schemas: Vec<String> = Vec::new();
    for r in &found {
        let (s, name) = (r.string(0), r.string(1));
        if !schemas.contains(&s) {
            schemas.push(s.clone());
        }
        if model
            .tables
            .last()
            .is_none_or(|t: &ModelTable| t.object.name != name || t.object.schema.as_deref() != Some(s.as_str()))
        {
            model.tables.push(ModelTable {
                object: ObjectRef::new(kind_of(&r.string(2)), database, Some(s), name),
                columns: Vec::new(),
            });
        }
        if let Some(table) = model.tables.last_mut() {
            table.columns.push(ModelColumn {
                name: r.string(3),
                data_type: r.string(4),
                nullable: r.flag(5),
                primary_key: false,
            });
        }
    }
    for s in &schemas {
        for c in group_constraints(&constraint_rows(conn, s, None).await?) {
            match c.kind.as_str() {
                "p" => {
                    let table = model
                        .tables
                        .iter_mut()
                        .find(|t| t.object.schema.as_deref() == Some(s.as_str()) && t.object.name == c.table);
                    for column in table.into_iter().flat_map(|t| t.columns.iter_mut()) {
                        column.primary_key |= c.columns.contains(&column.name);
                    }
                }
                "f" => model.foreign_keys.extend(foreign_key(database, s, &c)),
                _ => {}
            }
        }
    }
    model.truncated = model.tables.len() > SchemaModel::MAX_TABLES || found.len() >= SchemaModel::MAX_COLUMNS;
    model.tables.truncate(SchemaModel::MAX_TABLES);
    Ok(model)
}

/// `EXPORT_OBJECTS` for tables (with their projections), views and
/// sequences; the stored definition for functions.
pub async fn ddl(conn: &mut Connection, object: &ObjectRef) -> DbResult<String> {
    let schema = schema_of(object)?;
    let text = match object.kind {
        NodeKind::Table | NodeKind::View | NodeKind::Sequence => {
            let name = format!("{}.{}", D.quote_ident(schema), D.quote_ident(&object.name));
            conn.text_rows("SELECT EXPORT_OBJECTS('', ?, false)", &[&name]).await?.first().and_then(|r| r.text(0))
        }
        NodeKind::Function => {
            let rows = conn
                .text_rows(
                    "SELECT function_name, function_argument_type, function_return_type, function_definition \
                     FROM v_catalog.user_functions WHERE schema_name = ? AND function_name = ?",
                    &[schema, &object.name],
                )
                .await?;
            (!rows.is_empty()).then(|| {
                rows.iter()
                    .map(|r| {
                        format!(
                            "CREATE FUNCTION {}.{}({}) RETURN {}\nAS BEGIN\n  {};\nEND;",
                            D.display_ident(schema),
                            D.display_ident(&r.string(0)),
                            r.string(1),
                            r.string(2),
                            r.string(3).trim().trim_start_matches("RETURN ").trim_end_matches(';'),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n")
            })
        }
        _ => return Err(DbError::invalid("This object has no definition.")),
    };
    let text = text.filter(|t| !t.trim().is_empty()).ok_or_else(|| not_found(object))?;
    Ok(format!("{}\n", text.trim()))
}
