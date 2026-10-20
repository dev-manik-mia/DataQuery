//! Catalog queries for the explorer, structure tab and generated DDL.
//! Every name travels as a parameter; nothing is interpolated.

use std::fmt::Write;

use dq_core::node::{GROUP_FUNCTIONS, GROUP_PROCEDURES, GROUP_SEQUENCES, Node, NodeKind, ObjectRef};
use dq_core::schema::{
    ColumnInfo, ConstraintInfo, ConstraintKind, Dependencies, Dependency, ForeignKey, IndexInfo, ModelColumn,
    ModelTable, SchemaModel, TableSize, TableStructure,
};
use dq_core::sql::{Dialect, PostgresDialect};
use dq_core::{DbError, DbResult};
use tokio_postgres::Client;

use crate::connect::{Flavor, Server};
use crate::error::map_error;
use crate::values::is_native_oid;

const RELKINDS: &str = "('r','p','f','v','m')";

fn kind_of(relkind: &str) -> NodeKind {
    match relkind {
        "v" => NodeKind::View,
        "m" => NodeKind::MaterializedView,
        _ => NodeKind::Table,
    }
}

fn relkind_order() -> &'static str {
    "CASE WHEN c.relkind IN ('r','p','f') THEN 0 WHEN c.relkind = 'v' THEN 1 ELSE 2 END"
}

fn estimate(reltuples: i64) -> Option<i64> {
    (reltuples >= 0).then_some(reltuples)
}

fn pg(e: tokio_postgres::Error) -> DbError {
    map_error(&e, None)
}

/// Catalog SQL that differs on a `PostgreSQL` 9.x catalog (Greenplum 6).
impl Server {
    /// Hides partitions, which are listed under their parent table.
    fn not_partition(self) -> &'static str {
        match (self.legacy, self.flavor) {
            (false, _) => "NOT COALESCE(c.relispartition, false)",
            (true, Flavor::Greenplum) => {
                "NOT EXISTS (SELECT 1 FROM pg_partition_rule pr WHERE pr.parchildrelid = c.oid)"
            }
            (true, _) => "true",
        }
    }

    /// Functions or procedures; there were no procedures before `prokind`.
    fn routines(self, procedures: bool) -> &'static str {
        match (self.legacy, procedures) {
            (false, false) => "p.prokind IN ('f','a','w')",
            (false, true) => "p.prokind = 'p'",
            (true, false) => "true",
            (true, true) => "false",
        }
    }

    /// `attidentity` and `attgenerated`, or empty strings where they do not exist.
    fn identity_columns(self) -> &'static str {
        if self.legacy { "''::text, ''::text" } else { "a.attidentity::text, a.attgenerated::text" }
    }
}

pub async fn databases(client: &Client, current: &str) -> DbResult<Vec<Node>> {
    let rows = client
        .query("SELECT datname::text FROM pg_database WHERE datallowconn AND NOT datistemplate ORDER BY datname", &[])
        .await
        .map_err(pg)?;
    Ok(rows
        .iter()
        .map(|r| {
            let name: String = r.get(0);
            let mut node = Node::container(ObjectRef::new(NodeKind::Database, name.clone(), None, name.clone()));
            node.current = name == current;
            node
        })
        .collect())
}

pub async fn schemas(client: &Client, database: &str) -> DbResult<Vec<Node>> {
    let sql = format!(
        "SELECT n.nspname::text, \
                (SELECT count(*) FROM pg_class c WHERE c.relnamespace = n.oid AND c.relkind IN {RELKINDS}), \
                obj_description(n.oid, 'pg_namespace') \
         FROM pg_namespace n \
         WHERE n.nspname NOT LIKE 'pg\\_%' AND n.nspname NOT IN ('information_schema', 'crdb_internal', 'gp_toolkit') \
         ORDER BY n.nspname <> 'public', n.nspname"
    );
    let rows = client.query(&sql, &[]).await.map_err(pg)?;
    Ok(rows
        .iter()
        .map(|r| {
            let name: String = r.get(0);
            let mut node = Node::container(ObjectRef::new(NodeKind::Schema, database, Some(name.clone()), name));
            node.row_estimate = Some(r.get(1));
            node.comment = r.get(2);
            node
        })
        .collect())
}

fn relation_node(database: &str, row: &tokio_postgres::Row) -> Node {
    let kind = kind_of(&row.get::<_, String>(2));
    let object = ObjectRef::new(kind, database, Some(row.get(0)), row.get::<_, String>(1));
    // Tables and materialized views expand to their indexes and triggers.
    let mut node = if kind == NodeKind::View { Node::leaf(object) } else { Node::container(object) };
    node.row_estimate = estimate(row.get(3));
    node.comment = row.get(4);
    node
}

const RELATION_COLUMNS: &str = "n.nspname::text, c.relname::text, c.relkind::text, COALESCE(c.reltuples, -1)::int8, obj_description(c.oid, 'pg_class')";

pub async fn relations(client: &Client, server: Server, database: &str, schema: &str) -> DbResult<Vec<Node>> {
    let sql = format!(
        "SELECT {RELATION_COLUMNS} FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relkind IN {RELKINDS} AND {} \
         ORDER BY {}, c.relname",
        server.not_partition(),
        relkind_order()
    );
    let rows = client.query(&sql, &[&schema]).await.map_err(pg)?;
    let mut nodes: Vec<Node> = rows.iter().map(|r| relation_node(database, r)).collect();
    let counts = client
        .query_one(
            &format!(
                "SELECT (SELECT count(*) FROM pg_class c WHERE c.relnamespace = n.oid AND c.relkind = 'S'), \
                        (SELECT count(*) FROM pg_proc p WHERE p.pronamespace = n.oid AND {}), \
                        (SELECT count(*) FROM pg_proc p WHERE p.pronamespace = n.oid AND {}) \
                 FROM pg_namespace n WHERE n.nspname = $1",
                server.routines(false),
                server.routines(true)
            ),
            &[&schema],
        )
        .await
        .map_err(pg)?;
    for (i, group) in [GROUP_SEQUENCES, GROUP_FUNCTIONS, GROUP_PROCEDURES].into_iter().enumerate() {
        let count: i64 = counts.get(i);
        if count > 0 {
            let mut node = Node::container(ObjectRef::new(NodeKind::Group, database, Some(schema.to_owned()), group));
            node.row_estimate = Some(count);
            nodes.push(node);
        }
    }
    Ok(nodes)
}

/// Indexes and triggers of a table or materialized view.
pub async fn table_children(client: &Client, table: &ObjectRef) -> DbResult<Vec<Node>> {
    let rel = relation(client, table).await?;
    let child = |kind, name: String| {
        Node::leaf(ObjectRef::new(kind, table.database.clone(), table.schema.clone(), name).with_parent(&table.name))
    };
    let indexes = client
        .query(
            "SELECT i.relname::text FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid \
             WHERE x.indrelid = $1 ORDER BY x.indisprimary DESC, i.relname",
            &[&rel.oid],
        )
        .await
        .map_err(pg)?;
    let triggers = client
        .query("SELECT tgname::text FROM pg_trigger WHERE tgrelid = $1 AND NOT tgisinternal ORDER BY 1", &[&rel.oid])
        .await
        .map_err(pg)?;
    Ok(indexes
        .iter()
        .map(|r| child(NodeKind::Index, r.get(0)))
        .chain(triggers.iter().map(|r| child(NodeKind::Trigger, r.get(0))))
        .collect())
}

/// Sequences, functions or procedures of a schema.
pub async fn group_children(client: &Client, server: Server, group: &ObjectRef) -> DbResult<Vec<Node>> {
    let schema = group.schema.as_deref().ok_or_else(|| DbError::invalid("A schema is required."))?;
    let object = |kind, name: String| ObjectRef::new(kind, group.database.clone(), Some(schema.to_owned()), name);
    let routines = |procedures: bool| {
        format!(
            "SELECT p.proname::text, pg_get_function_identity_arguments(p.oid), obj_description(p.oid, 'pg_proc') \
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = $1 AND {} ORDER BY 1, 2",
            server.routines(procedures)
        )
    };
    let (sql, kind) = match group.name.as_str() {
        GROUP_SEQUENCES => (
            "SELECT c.relname::text, NULL::text, obj_description(c.oid, 'pg_class') FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relkind = 'S' ORDER BY 1"
                .to_owned(),
            NodeKind::Sequence,
        ),
        GROUP_FUNCTIONS => (routines(false), NodeKind::Function),
        GROUP_PROCEDURES => (routines(true), NodeKind::Procedure),
        other => return Err(DbError::invalid(format!("Unknown group `{other}`."))),
    };
    let rows = client.query(&sql, &[&schema]).await.map_err(pg)?;
    Ok(rows
        .iter()
        .map(|r| {
            let mut o = object(kind, r.get(0));
            o.parent = r.get(1);
            let mut node = Node::leaf(o);
            node.comment = r.get(2);
            node
        })
        .collect())
}

pub async fn search(client: &Client, database: &str, query: &str, limit: u32) -> DbResult<Vec<Node>> {
    let pattern = format!("%{}%", query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let sql = format!(
        "SELECT {RELATION_COLUMNS} FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relname ILIKE $1 AND c.relkind IN {RELKINDS} \
           AND n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema' \
         ORDER BY length(c.relname), c.relname LIMIT $2"
    );
    let rows = client.query(&sql, &[&pattern, &i64::from(limit)]).await.map_err(pg)?;
    Ok(rows.iter().map(|r| relation_node(database, r)).collect())
}

struct Relation {
    oid: u32,
    kind: NodeKind,
    estimate: Option<i64>,
    comment: Option<String>,
}

async fn relation(client: &Client, object: &ObjectRef) -> DbResult<Relation> {
    let schema = object.schema.as_deref().ok_or_else(|| DbError::invalid("A schema is required."))?;
    let row = client
        .query_opt(
            &format!(
                "SELECT c.oid, c.relkind::text, COALESCE(c.reltuples, -1)::int8, obj_description(c.oid, 'pg_class') \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN {RELKINDS}"
            ),
            &[&schema, &object.name],
        )
        .await
        .map_err(pg)?
        .ok_or_else(|| DbError::not_found(format!("{} no longer exists.", PostgresDialect.display_name(object))))?;
    Ok(Relation {
        oid: row.get(0),
        kind: kind_of(&row.get::<_, String>(1)),
        estimate: estimate(row.get(2)),
        comment: row.get(3),
    })
}

pub async fn row_estimate(client: &Client, object: &ObjectRef) -> DbResult<Option<i64>> {
    Ok(relation(client, object).await?.estimate)
}

pub async fn structure(client: &Client, server: Server, object: &ObjectRef) -> DbResult<TableStructure> {
    let rel = relation(client, object).await?;
    let (columns, constraints, indexes, identity, foreign_keys, size) = tokio::try_join!(
        columns(client, server, rel.oid),
        constraints(client, rel.oid),
        indexes(client, rel.oid),
        row_identity(client, rel.oid),
        foreign_keys(client, "c.conrelid = $1", &object.database, rel.oid),
        size(client, rel.oid, rel.kind),
    )?;
    Ok(TableStructure {
        object: ObjectRef { kind: rel.kind, ..object.clone() },
        columns,
        constraints,
        indexes,
        row_identity: if rel.kind == NodeKind::Table { identity } else { Vec::new() },
        full_row_key: Vec::new(),
        comment: rel.comment,
        row_estimate: rel.estimate,
        foreign_keys,
        size,
    })
}

async fn columns(client: &Client, server: Server, oid: u32) -> DbResult<Vec<ColumnInfo>> {
    let rows = client
        .query(
            &format!(
                "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull, \
                        pg_get_expr(d.adbin, d.adrelid), {}, \
                        a.atttypid, t.typtype::text, CASE WHEN t.typcategory = 'A' THEN t.typelem ELSE 0::oid END, t.typbasetype, col_description(a.attrelid, a.attnum), \
                        EXISTS (SELECT 1 FROM pg_index x WHERE x.indrelid = a.attrelid AND x.indisprimary AND a.attnum = ANY(x.indkey)) \
                 FROM pg_attribute a \
                 JOIN pg_type t ON t.oid = a.atttypid \
                 LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                server.identity_columns()
            ),
            &[&oid],
        )
        .await
        .map_err(pg)?;
    Ok(rows
        .iter()
        .map(|r| {
            let data_type: String = r.get(1);
            let identity: String = r.get(4);
            let generated_expr: String = r.get(5);
            let type_oid: u32 = r.get(6);
            let typtype: String = r.get(7);
            let native = is_native_oid(type_oid)
                || typtype == "e"
                || is_native_oid(r.get::<_, u32>(8))
                || (typtype == "d" && is_native_oid(r.get::<_, u32>(9)));
            let name: String = r.get(0);
            let default: Option<String> = r.get(3);
            // CockroachDB gives keyless tables a hidden `rowid` key filled by `unique_rowid()`.
            let rowid = name == "rowid" && default.as_deref() == Some("unique_rowid()");
            let always_generated = identity == "a" || !generated_expr.is_empty() || rowid;
            ColumnInfo {
                name,
                numeric: [20, 21, 23, 26, 700, 701, 790, 1700].contains(&type_oid),
                editable: data_type != "bytea" && !always_generated,
                generated: !identity.is_empty() || !generated_expr.is_empty() || rowid,
                identity: !identity.is_empty(),
                data_type,
                nullable: r.get(2),
                default,
                primary_key: r.get(11),
                comment: r.get(10),
                read_as_text: !native,
            }
        })
        .collect())
}

async fn constraints(client: &Client, oid: u32) -> DbResult<Vec<ConstraintInfo>> {
    let rows = client
        .query(
            "SELECT conname::text, contype::text, pg_get_constraintdef(oid, true) FROM pg_constraint \
             WHERE conrelid = $1 \
             ORDER BY CASE contype WHEN 'p' THEN 0 WHEN 'u' THEN 1 WHEN 'f' THEN 2 WHEN 'c' THEN 3 ELSE 4 END, conname",
            &[&oid],
        )
        .await
        .map_err(pg)?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let kind = match r.get::<_, String>(1).as_str() {
                "p" => ConstraintKind::PrimaryKey,
                "f" => ConstraintKind::ForeignKey,
                "u" => ConstraintKind::Unique,
                "c" => ConstraintKind::Check,
                "x" => ConstraintKind::Exclusion,
                _ => return None,
            };
            Some(ConstraintInfo { kind, name: r.get(0), definition: r.get(2) })
        })
        .collect())
}

fn undefined_function(e: &tokio_postgres::Error) -> bool {
    e.code() == Some(&tokio_postgres::error::SqlState::UNDEFINED_FUNCTION)
}

async fn indexes(client: &Client, oid: u32) -> DbResult<Vec<IndexInfo>> {
    let query = |size: &str| {
        format!(
            "SELECT i.relname::text, pg_get_indexdef(x.indexrelid), x.indisunique, x.indisprimary, \
                    ARRAY(SELECT a.attname::text FROM unnest(x.indkey) WITH ORDINALITY k(attnum, ord) \
                          JOIN pg_attribute a ON a.attrelid = x.indrelid AND a.attnum = k.attnum ORDER BY k.ord), \
                    {size} \
             FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid WHERE x.indrelid = $1 \
             ORDER BY x.indisprimary DESC, i.relname"
        )
    };
    let rows = match client.query(&query("pg_relation_size(x.indexrelid)"), &[&oid]).await {
        // CockroachDB has no per-index size.
        Err(e) if undefined_function(&e) => client.query(&query("NULL::int8"), &[&oid]).await,
        other => other,
    }
    .map_err(pg)?;
    Ok(rows
        .iter()
        .map(|r| IndexInfo {
            name: r.get(0),
            definition: r.get(1),
            unique: r.get(2),
            primary: r.get(3),
            columns: r.get(4),
            size_bytes: r.get(5),
        })
        .collect())
}

async fn size(client: &Client, oid: u32, kind: NodeKind) -> DbResult<Option<TableSize>> {
    if kind == NodeKind::View {
        return Ok(None);
    }
    let r = client
        .query_one(
            "SELECT pg_total_relation_size($1::oid), pg_relation_size($1::oid), pg_indexes_size($1::oid)",
            &[&oid],
        )
        .await;
    match r {
        Ok(r) => Ok(Some(TableSize { total_bytes: r.get(0), table_bytes: r.get(1), index_bytes: r.get(2) })),
        // CockroachDB has no size functions; its sizes are per range, not per table.
        Err(e) if undefined_function(&e) => Ok(None),
        Err(e) => Err(pg(e)),
    }
}

const FK_SELECT: &str = "SELECT c.conname::text, tn.nspname::text, t.relname::text, \
       ARRAY(SELECT a.attname::text FROM unnest(c.conkey) WITH ORDINALITY k(n, o) \
             JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.n ORDER BY k.o), \
       fn.nspname::text, f.relname::text, \
       ARRAY(SELECT a.attname::text FROM unnest(c.confkey) WITH ORDINALITY k(n, o) \
             JOIN pg_attribute a ON a.attrelid = c.confrelid AND a.attnum = k.n ORDER BY k.o), \
       c.confupdtype::text, c.confdeltype::text \
FROM pg_constraint c \
JOIN pg_class t ON t.oid = c.conrelid JOIN pg_namespace tn ON tn.oid = t.relnamespace \
JOIN pg_class f ON f.oid = c.confrelid JOIN pg_namespace fn ON fn.oid = f.relnamespace \
WHERE c.contype = 'f' AND ";

fn fk_action(code: &str) -> Option<String> {
    Some(
        match code {
            "r" => "RESTRICT",
            "c" => "CASCADE",
            "n" => "SET NULL",
            "d" => "SET DEFAULT",
            _ => return None,
        }
        .to_owned(),
    )
}

fn fk_from_row(database: &str, r: &tokio_postgres::Row) -> ForeignKey {
    let table = |schema: String, name: String| ObjectRef::new(NodeKind::Table, database, Some(schema), name);
    ForeignKey {
        name: r.get(0),
        source: table(r.get(1), r.get(2)),
        columns: r.get(3),
        target: table(r.get(4), r.get(5)),
        target_columns: r.get(6),
        on_update: fk_action(&r.get::<_, String>(7)),
        on_delete: fk_action(&r.get::<_, String>(8)),
    }
}

/// Foreign keys matching `predicate` (over `c`, with `$1` bound to `oid`).
async fn foreign_keys(client: &Client, predicate: &str, database: &str, oid: u32) -> DbResult<Vec<ForeignKey>> {
    let rows = client.query(&format!("{FK_SELECT}{predicate} ORDER BY c.conname"), &[&oid]).await.map_err(pg)?;
    Ok(rows.iter().map(|r| fk_from_row(database, r)).collect())
}

/// Foreign keys, view sources, triggers and owned sequences around a relation.
pub async fn dependencies(client: &Client, object: &ObjectRef) -> DbResult<Dependencies> {
    if !object.kind.is_relation() {
        return Ok(Dependencies::default());
    }
    let rel = relation(client, object).await?;
    let db = object.database.as_str();
    let rel_ref =
        |schema: String, name: String, relkind: &str| ObjectRef::new(kind_of(relkind), db, Some(schema), name);
    let mut deps = Dependencies::default();
    for fk in foreign_keys(client, "c.conrelid = $1", db, rel.oid).await? {
        deps.uses.push(Dependency { object: fk.target, relation: format!("foreign key {}", fk.name) });
    }
    for fk in foreign_keys(client, "c.confrelid = $1", db, rel.oid).await? {
        deps.used_by.push(Dependency { object: fk.source, relation: format!("foreign key {}", fk.name) });
    }
    let sources = client
        .query(
            "SELECT DISTINCT rn.nspname::text, rc.relname::text, rc.relkind::text FROM pg_rewrite r \
             JOIN pg_depend d ON d.objid = r.oid AND d.classid = 'pg_rewrite'::regclass AND d.refclassid = 'pg_class'::regclass \
             JOIN pg_class rc ON rc.oid = d.refobjid JOIN pg_namespace rn ON rn.oid = rc.relnamespace \
             WHERE r.ev_class = $1 AND d.refobjid <> $1 ORDER BY 1, 2",
            &[&rel.oid],
        )
        .await
        .map_err(pg)?;
    for r in &sources {
        deps.uses
            .push(Dependency { object: rel_ref(r.get(0), r.get(1), &r.get::<_, String>(2)), relation: "reads".into() });
    }
    let views = client
        .query(
            "SELECT DISTINCT vn.nspname::text, v.relname::text, v.relkind::text FROM pg_depend d \
             JOIN pg_rewrite r ON r.oid = d.objid JOIN pg_class v ON v.oid = r.ev_class \
             JOIN pg_namespace vn ON vn.oid = v.relnamespace \
             WHERE d.classid = 'pg_rewrite'::regclass AND d.refobjid = $1 AND r.ev_class <> $1 ORDER BY 1, 2",
            &[&rel.oid],
        )
        .await
        .map_err(pg)?;
    for r in &views {
        deps.used_by
            .push(Dependency { object: rel_ref(r.get(0), r.get(1), &r.get::<_, String>(2)), relation: "view".into() });
    }
    let triggers = client
        .query(
            "SELECT t.tgname::text, pn.nspname::text, p.proname::text, pg_get_function_identity_arguments(p.oid) \
             FROM pg_trigger t JOIN pg_proc p ON p.oid = t.tgfoid JOIN pg_namespace pn ON pn.oid = p.pronamespace \
             WHERE t.tgrelid = $1 AND NOT t.tgisinternal ORDER BY 1",
            &[&rel.oid],
        )
        .await
        .map_err(pg)?;
    for r in &triggers {
        let trigger: String = r.get(0);
        let function = ObjectRef::new(NodeKind::Function, db, Some(r.get(1)), r.get::<_, String>(2))
            .with_parent(r.get::<_, String>(3));
        deps.uses.push(Dependency { object: function, relation: format!("trigger {trigger}") });
        let t = ObjectRef::new(NodeKind::Trigger, db, object.schema.clone(), trigger).with_parent(&object.name);
        deps.used_by.push(Dependency { object: t, relation: "trigger".into() });
    }
    let sequences = client
        .query(
            "SELECT sn.nspname::text, s.relname::text FROM pg_depend d \
             JOIN pg_class s ON s.oid = d.objid AND s.relkind = 'S' JOIN pg_namespace sn ON sn.oid = s.relnamespace \
             WHERE d.refobjid = $1 AND d.classid = 'pg_class'::regclass AND d.deptype IN ('a', 'i') ORDER BY 1, 2",
            &[&rel.oid],
        )
        .await
        .map_err(pg)?;
    for r in &sequences {
        let seq = ObjectRef::new(NodeKind::Sequence, db, Some(r.get(0)), r.get::<_, String>(1));
        deps.uses.push(Dependency { object: seq, relation: "owned sequence".into() });
    }
    Ok(deps)
}

/// Tables, columns and foreign keys of one schema or every user schema.
pub async fn schema_model(
    client: &Client,
    server: Server,
    database: &str,
    schema: Option<&str>,
) -> DbResult<SchemaModel> {
    let filter = "(($1::text IS NULL AND n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema') \
                   OR n.nspname = $1)";
    let max_tables = SchemaModel::MAX_TABLES as i64;
    let rows = client
        .query(
            &format!(
                "WITH rels AS (SELECT c.oid, n.nspname, c.relname, c.relkind FROM pg_class c \
                     JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE c.relkind IN {RELKINDS} AND {} AND {filter} \
                     ORDER BY n.nspname, c.relname LIMIT $2) \
                 SELECT r.nspname::text, r.relname::text, r.relkind::text, a.attname::text, \
                        format_type(a.atttypid, a.atttypmod), NOT a.attnotnull, \
                        EXISTS (SELECT 1 FROM pg_index x WHERE x.indrelid = r.oid AND x.indisprimary AND a.attnum = ANY(x.indkey)) \
                 FROM rels r JOIN pg_attribute a ON a.attrelid = r.oid AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY r.nspname, r.relname, a.attnum LIMIT $3",
                server.not_partition()
            ),
            &[&schema, &(max_tables + 1), &(SchemaModel::MAX_COLUMNS as i64)],
        )
        .await
        .map_err(pg)?;
    let mut model = SchemaModel::default();
    for r in &rows {
        let (schema, name): (String, String) = (r.get(0), r.get(1));
        let same = model
            .tables
            .last()
            .is_some_and(|t: &ModelTable| t.object.schema.as_deref() == Some(&schema) && t.object.name == name);
        if !same {
            let object = ObjectRef::new(kind_of(&r.get::<_, String>(2)), database, Some(schema), name);
            model.tables.push(ModelTable { object, columns: Vec::new() });
        }
        if let Some(table) = model.tables.last_mut() {
            table.columns.push(ModelColumn {
                name: r.get(3),
                data_type: r.get(4),
                nullable: r.get(5),
                primary_key: r.get(6),
            });
        }
    }
    model.truncated = model.tables.len() > SchemaModel::MAX_TABLES || rows.len() >= SchemaModel::MAX_COLUMNS;
    model.tables.truncate(SchemaModel::MAX_TABLES);
    let fks = client
        .query(
            &format!(
                "{FK_SELECT}{} ORDER BY tn.nspname, t.relname, c.conname",
                filter.replace("n.nspname", "tn.nspname")
            ),
            &[&schema],
        )
        .await
        .map_err(pg)?;
    model.foreign_keys = fks.iter().map(|r| fk_from_row(database, r)).collect();
    Ok(model)
}

/// Primary key columns, else the columns of the first NOT NULL unique index
/// without expressions or predicates.
async fn row_identity(client: &Client, oid: u32) -> DbResult<Vec<String>> {
    let rows = client
        .query(
            "SELECT x.indexrelid, a.attname::text, a.attnotnull \
             FROM pg_index x \
             JOIN LATERAL unnest(x.indkey) WITH ORDINALITY AS k(attnum, ord) ON true \
             JOIN pg_attribute a ON a.attrelid = x.indrelid AND a.attnum = k.attnum \
             WHERE x.indrelid = $1 AND x.indisunique AND x.indexprs IS NULL AND x.indpred IS NULL AND x.indisvalid \
             ORDER BY x.indisprimary DESC, x.indexrelid, k.ord",
            &[&oid],
        )
        .await
        .map_err(pg)?;
    let mut current: Option<u32> = None;
    let mut columns = Vec::new();
    let mut usable = true;
    for r in &rows {
        let index: u32 = r.get(0);
        if current.is_some_and(|c| c != index) {
            if usable && !columns.is_empty() {
                return Ok(columns);
            }
            columns.clear();
            usable = true;
        }
        current = Some(index);
        usable &= r.get::<_, bool>(2);
        columns.push(r.get(1));
    }
    Ok(if usable { columns } else { Vec::new() })
}

/// Definition of an index, sequence, trigger, function or procedure.
async fn object_ddl(client: &Client, server: Server, object: &ObjectRef) -> DbResult<String> {
    let schema = object.schema.as_deref().ok_or_else(|| DbError::invalid("A schema is required."))?;
    let missing = || DbError::not_found(format!("{} no longer exists.", object.name));
    let d = PostgresDialect;
    let text: Option<String> = match object.kind {
        NodeKind::Index => client
            .query_opt(
                "SELECT pg_get_indexdef(c.oid) || ';' FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ('i', 'I')",
                &[&schema, &object.name],
            )
            .await
            .map_err(pg)?
            .map(|r| r.get(0)),
        NodeKind::Trigger => {
            let table = object.owner_table().ok_or_else(|| DbError::invalid("A trigger needs its table."))?;
            let rel = relation(client, &table).await?;
            client
                .query_opt(
                    "SELECT pg_get_triggerdef(oid, true) || ';' FROM pg_trigger WHERE tgrelid = $1 AND tgname = $2",
                    &[&rel.oid, &object.name],
                )
                .await
                .map_err(pg)?
                .map(|r| r.get(0))
        }
        NodeKind::Function | NodeKind::Procedure => client
            .query_opt(
                "SELECT pg_get_functiondef(p.oid) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
                 WHERE n.nspname = $1 AND p.proname = $2 AND pg_get_function_identity_arguments(p.oid) = $3",
                &[&schema, &object.name, &object.parent.as_deref().unwrap_or_default()],
            )
            .await
            .map_err(pg)?
            .map(|r| r.get(0)),
        NodeKind::Sequence if server.legacy => {
            return Err(DbError::unsupported("Sequence definitions need a server built on PostgreSQL 10 or newer."));
        }
        NodeKind::Sequence => {
            let row = client
                .query_opt(
                    "SELECT s.data_type::text, s.start_value, s.min_value, s.max_value, s.increment_by, s.cycle, \
                            s.cache_size, \
                            (SELECT quote_ident(tn.nspname) || '.' || quote_ident(t.relname) || '.' || quote_ident(a.attname) \
                             FROM pg_depend dp JOIN pg_class t ON t.oid = dp.refobjid \
                             JOIN pg_namespace tn ON tn.oid = t.relnamespace \
                             JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = dp.refobjsubid \
                             WHERE dp.objid = c.oid AND dp.classid = 'pg_class'::regclass \
                               AND dp.refclassid = 'pg_class'::regclass AND dp.deptype IN ('a', 'i') LIMIT 1) \
                     FROM pg_sequences s \
                     JOIN pg_namespace n ON n.nspname = s.schemaname \
                     JOIN pg_class c ON c.relnamespace = n.oid AND c.relname = s.sequencename \
                     WHERE s.schemaname = $1 AND s.sequencename = $2",
                    &[&schema, &object.name],
                )
                .await
                .map_err(pg)?;
            row.map(|r| {
                let mut out = format!(
                    "CREATE SEQUENCE {} AS {}\n  INCREMENT BY {}\n  MINVALUE {}\n  MAXVALUE {}\n  START WITH {}\n  CACHE {}{}",
                    d.display_name(object),
                    r.get::<_, String>(0),
                    r.get::<_, i64>(4),
                    r.get::<_, i64>(2),
                    r.get::<_, i64>(3),
                    r.get::<_, i64>(1),
                    r.get::<_, i64>(6),
                    if r.get::<_, bool>(5) { "\n  CYCLE" } else { "" },
                );
                if let Some(owner) = r.get::<_, Option<String>>(7) {
                    let _ = write!(out, "\n  OWNED BY {owner}");
                }
                out + ";"
            })
        }
        _ => return Err(DbError::invalid("This object has no definition.")),
    };
    text.map(|t| t.trim_end().to_owned() + "\n").ok_or_else(missing)
}

/// DDL reconstructed from the catalog.
pub async fn ddl(client: &Client, server: Server, object: &ObjectRef) -> DbResult<String> {
    if !object.kind.is_relation() {
        return object_ddl(client, server, object).await;
    }
    let d = PostgresDialect;
    let rel = relation(client, object).await?;
    let name = d.display_name(object);
    if rel.kind != NodeKind::Table {
        let def: String =
            client.query_one("SELECT pg_get_viewdef($1::oid, true)", &[&rel.oid]).await.map_err(pg)?.get(0);
        let keyword = if rel.kind == NodeKind::View { "VIEW" } else { "MATERIALIZED VIEW" };
        return Ok(format!("CREATE {keyword} {name} AS\n{}", def.trim_end()));
    }
    let structure = structure(client, server, object).await?;
    let attrs = client
        .query(
            &format!(
                "SELECT a.attname::text, {} FROM pg_attribute a \
                 WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
                server.identity_columns()
            ),
            &[&rel.oid],
        )
        .await
        .map_err(pg)?;
    let mut lines = Vec::new();
    for (column, attr) in structure.columns.iter().zip(&attrs) {
        let identity: String = attr.get(1);
        let generated: String = attr.get(2);
        let mut line = format!("  {} {}", d.display_ident(&column.name), column.data_type);
        match (identity.as_str(), generated.as_str(), &column.default) {
            ("a", _, _) => line.push_str(" GENERATED ALWAYS AS IDENTITY"),
            ("d", _, _) => line.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
            (_, "s", Some(expr)) => {
                let _ = write!(line, " GENERATED ALWAYS AS ({expr}) STORED");
            }
            _ => {}
        }
        if !column.nullable {
            line.push_str(" NOT NULL");
        }
        if let Some(default) = column.default.as_ref().filter(|_| identity.is_empty() && generated.is_empty()) {
            let _ = write!(line, " DEFAULT {default}");
        }
        lines.push(line);
    }
    for c in &structure.constraints {
        lines.push(format!("  CONSTRAINT {} {}", d.display_ident(&c.name), c.definition));
    }
    let mut out = format!("CREATE TABLE {name} (\n{}\n);\n", lines.join(",\n"));
    let constraint_names: Vec<&str> = structure.constraints.iter().map(|c| c.name.as_str()).collect();
    for index in structure.indexes.iter().filter(|i| !constraint_names.contains(&i.name.as_str())) {
        let _ = write!(out, "\n{};", index.definition);
    }
    if let Some(comment) = &structure.comment {
        let _ = write!(out, "\n\nCOMMENT ON TABLE {name} IS {};", d.quote_literal(comment));
    }
    for column in structure.columns.iter().filter(|c| c.comment.is_some()) {
        let comment = column.comment.as_deref().unwrap_or_default();
        let _ = write!(
            out,
            "\nCOMMENT ON COLUMN {name}.{} IS {};",
            d.display_ident(&column.name),
            d.quote_literal(comment)
        );
    }
    Ok(out.trim_end().to_owned() + "\n")
}

/// Puts `clauses` (one per line) after the column list of the first `CREATE TABLE`.
fn with_table_clauses(ddl: &str, clauses: &[String]) -> String {
    match ddl.split_once("\n);") {
        Some((head, tail)) if !clauses.is_empty() => format!("{head}\n)\n{};{tail}", clauses.join("\n")),
        _ => ddl.to_owned(),
    }
}

/// Greenplum: the table's DDL with what makes it an MPP table: access method
/// (append-optimized row or column storage), storage options, distribution
/// (`DISTRIBUTED BY / RANDOMLY / REPLICATED`) and partitioning, in the order
/// both Greenplum 6 and 7 accept. Greenplum 6 keeps storage in the options
/// and spells out every partition; Greenplum 7 names only the partition key.
pub async fn greenplum_ddl(client: &Client, server: Server, object: &ObjectRef) -> DbResult<String> {
    let text = ddl(client, server, object).await?;
    if object.kind != NodeKind::Table {
        return Ok(text);
    }
    let rel = relation(client, object).await?;
    let (partitions, method, join) = if server.legacy {
        (
            "CASE WHEN EXISTS (SELECT 1 FROM pg_partition p WHERE p.parrelid = c.oid) \
                  THEN pg_get_partition_def(c.oid, true, true) END",
            "NULL::text",
            "",
        )
    } else {
        (
            "CASE WHEN c.relkind = 'p' THEN 'PARTITION BY ' || pg_get_partkeydef(c.oid) END",
            "am.amname::text",
            "LEFT JOIN pg_am am ON am.oid = c.relam",
        )
    };
    let r = client
        .query_one(
            &format!(
                "SELECT {method}, array_to_string(c.reloptions, ', '), pg_get_table_distributedby(c.oid), {partitions} \
                 FROM pg_class c {join} WHERE c.oid = $1"
            ),
            &[&rel.oid],
        )
        .await
        .map_err(pg)?;
    let mut clauses = Vec::new();
    if let Some(method) = r.get::<_, Option<String>>(0).filter(|m| m != "heap") {
        clauses.push(format!("USING {method}"));
    }
    if let Some(options) = r.get::<_, Option<String>>(1).filter(|o| !o.is_empty()) {
        clauses.push(format!("WITH ({options})"));
    }
    for clause in [r.get::<_, Option<String>>(2), r.get(3)].into_iter().flatten() {
        if !clause.trim().is_empty() {
            clauses.push(clause.trim_end().to_owned());
        }
    }
    Ok(with_table_clauses(&text, &clauses))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greenplum_clauses_follow_the_column_list() {
        let ddl =
            "CREATE TABLE public.t (\n  id integer NOT NULL\n);\n\nCREATE INDEX i ON public.t USING btree (id);\n";
        let clauses = ["USING ao_column".to_owned(), "DISTRIBUTED BY (id)".to_owned()];
        assert_eq!(
            with_table_clauses(ddl, &clauses),
            "CREATE TABLE public.t (\n  id integer NOT NULL\n)\nUSING ao_column\nDISTRIBUTED BY (id);\n\nCREATE INDEX i ON public.t USING btree (id);\n"
        );
        assert_eq!(with_table_clauses(ddl, &[]), ddl);
    }
}
