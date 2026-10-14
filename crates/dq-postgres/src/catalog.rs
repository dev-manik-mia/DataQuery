//! Catalog queries for the explorer, structure tab and generated DDL.
//! Every name travels as a parameter; nothing is interpolated.

use std::fmt::Write;

use dq_core::node::{Node, NodeKind, ObjectRef};
use dq_core::schema::{ColumnInfo, ConstraintInfo, ConstraintKind, IndexInfo, TableStructure};
use dq_core::sql::{Dialect, PostgresDialect};
use dq_core::{DbError, DbResult};
use tokio_postgres::Client;

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

pub async fn databases(client: &Client, current: &str) -> DbResult<Vec<Node>> {
    let rows = client
        .query("SELECT datname::text FROM pg_database WHERE datallowconn AND NOT datistemplate ORDER BY datname", &[])
        .await
        .map_err(pg)?;
    Ok(rows
        .iter()
        .map(|r| {
            let name: String = r.get(0);
            let mut node = Node::container(ObjectRef {
                kind: NodeKind::Database,
                database: name.clone(),
                schema: None,
                name: name.clone(),
            });
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
         WHERE n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema' \
         ORDER BY n.nspname <> 'public', n.nspname"
    );
    let rows = client.query(&sql, &[]).await.map_err(pg)?;
    Ok(rows
        .iter()
        .map(|r| {
            let name: String = r.get(0);
            let mut node = Node::container(ObjectRef {
                kind: NodeKind::Schema,
                database: database.to_owned(),
                schema: Some(name.clone()),
                name,
            });
            node.row_estimate = Some(r.get(1));
            node.comment = r.get(2);
            node
        })
        .collect())
}

fn relation_node(database: &str, row: &tokio_postgres::Row) -> Node {
    let mut node = Node::leaf(ObjectRef {
        kind: kind_of(&row.get::<_, String>(2)),
        database: database.to_owned(),
        schema: Some(row.get(0)),
        name: row.get(1),
    });
    node.row_estimate = estimate(row.get(3));
    node.comment = row.get(4);
    node
}

const RELATION_COLUMNS: &str =
    "n.nspname::text, c.relname::text, c.relkind::text, c.reltuples::int8, obj_description(c.oid, 'pg_class')";

pub async fn relations(client: &Client, database: &str, schema: &str) -> DbResult<Vec<Node>> {
    let sql = format!(
        "SELECT {RELATION_COLUMNS} FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relkind IN {RELKINDS} AND NOT c.relispartition \
         ORDER BY {}, c.relname",
        relkind_order()
    );
    let rows = client.query(&sql, &[&schema]).await.map_err(pg)?;
    Ok(rows.iter().map(|r| relation_node(database, r)).collect())
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
                "SELECT c.oid, c.relkind::text, c.reltuples::int8, obj_description(c.oid, 'pg_class') \
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

pub async fn structure(client: &Client, object: &ObjectRef) -> DbResult<TableStructure> {
    let rel = relation(client, object).await?;
    let (columns, constraints, indexes, identity) = tokio::try_join!(
        columns(client, rel.oid),
        constraints(client, rel.oid),
        indexes(client, rel.oid),
        row_identity(client, rel.oid),
    )?;
    Ok(TableStructure {
        object: ObjectRef { kind: rel.kind, ..object.clone() },
        columns,
        constraints,
        indexes,
        row_identity: if rel.kind == NodeKind::Table { identity } else { Vec::new() },
        comment: rel.comment,
        row_estimate: rel.estimate,
    })
}

async fn columns(client: &Client, oid: u32) -> DbResult<Vec<ColumnInfo>> {
    let rows = client
        .query(
            "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull, \
                    pg_get_expr(d.adbin, d.adrelid), a.attidentity::text, a.attgenerated::text, \
                    a.atttypid, t.typtype::text, CASE WHEN t.typcategory = 'A' THEN t.typelem ELSE 0::oid END, t.typbasetype, col_description(a.attrelid, a.attnum), \
                    EXISTS (SELECT 1 FROM pg_index x WHERE x.indrelid = a.attrelid AND x.indisprimary AND a.attnum = ANY(x.indkey)) \
             FROM pg_attribute a \
             JOIN pg_type t ON t.oid = a.atttypid \
             LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
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
            let always_generated = identity == "a" || !generated_expr.is_empty();
            ColumnInfo {
                name: r.get(0),
                numeric: [20, 21, 23, 26, 700, 701, 790, 1700].contains(&type_oid),
                editable: data_type != "bytea" && !always_generated,
                generated: !identity.is_empty() || !generated_expr.is_empty(),
                data_type,
                nullable: r.get(2),
                default: r.get(3),
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
             WHERE conrelid = $1 ORDER BY array_position(ARRAY['p','u','f','c','x']::text[], contype::text), conname",
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

async fn indexes(client: &Client, oid: u32) -> DbResult<Vec<IndexInfo>> {
    let rows = client
        .query(
            "SELECT i.relname::text, pg_get_indexdef(x.indexrelid), x.indisunique, x.indisprimary \
             FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid WHERE x.indrelid = $1 \
             ORDER BY x.indisprimary DESC, i.relname",
            &[&oid],
        )
        .await
        .map_err(pg)?;
    Ok(rows
        .iter()
        .map(|r| IndexInfo { name: r.get(0), definition: r.get(1), unique: r.get(2), primary: r.get(3) })
        .collect())
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

/// DDL reconstructed from the catalog.
pub async fn ddl(client: &Client, object: &ObjectRef) -> DbResult<String> {
    let d = PostgresDialect;
    let rel = relation(client, object).await?;
    let name = d.display_name(object);
    if rel.kind != NodeKind::Table {
        let def: String =
            client.query_one("SELECT pg_get_viewdef($1::oid, true)", &[&rel.oid]).await.map_err(pg)?.get(0);
        let keyword = if rel.kind == NodeKind::View { "VIEW" } else { "MATERIALIZED VIEW" };
        return Ok(format!("CREATE {keyword} {name} AS\n{}", def.trim_end()));
    }
    let structure = structure(client, object).await?;
    let attrs = client
        .query(
            "SELECT a.attname::text, a.attidentity::text, a.attgenerated::text FROM pg_attribute a \
             WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
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
