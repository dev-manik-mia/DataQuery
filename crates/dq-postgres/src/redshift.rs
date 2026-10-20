//! Amazon Redshift catalog. Redshift forked `PostgreSQL` 8.0: `pg_class` and
//! `pg_attribute` exist (with `reldiststyle`, `attisdistkey`, `attsortkeyord`
//! and `attencodingtype`), but there are no indexes, sequences, triggers,
//! `relispartition`, `prokind` or `pg_get_function_identity_arguments`.
//! Routines come from `pg_proc_info`; external (Spectrum) schemas from
//! `svv_external_schemas` / `svv_external_tables` / `svv_external_columns`,
//! and their tables are read-only. Key columns are read from
//! `pg_get_constraintdef`, as `sqlalchemy-redshift` does, since the catalog's
//! key arrays are not usable on the leader node; sizes come from
//! `svv_table_info`, which is queried on its own because compute-node views
//! cannot be joined with leader-node functions.
//!
//! Names travel as parameters. `SHOW TABLE` and `SHOW PROCEDURE` take no
//! parameters, so their names are quoted by [`RedshiftDialect`].

use std::fmt::Write;

use dq_core::node::{GROUP_FUNCTIONS, GROUP_PROCEDURES, Node, NodeKind, ObjectRef};
use dq_core::schema::{
    ColumnInfo, ConstraintInfo, ConstraintKind, Dependencies, Dependency, ForeignKey, ModelColumn, ModelTable,
    SchemaModel, TableSize, TableStructure,
};
use dq_core::sql::builder::like_contains;
use dq_core::sql::{Dialect, RedshiftDialect};
use dq_core::{DbError, DbResult};
use tokio_postgres::{Client, SimpleQueryMessage};

use crate::error::map_error;
use crate::values::is_native_oid;

const D: RedshiftDialect = RedshiftDialect;

fn pg(e: tokio_postgres::Error) -> DbError {
    map_error(&e, None)
}

/// System schemas, hidden from the explorer and the schema model.
const USER_SCHEMA: &str =
    "substr(n.nspname, 1, 3) <> 'pg_' AND n.nspname NOT IN ('information_schema', 'catalog_history')";

/// Types the grid cannot write back through a text cast (`SUPER` needs
/// `JSON_PARSE`, spatial types a constructor, `VARBYTE` hex input).
const NOT_EDITABLE: &[&str] = &["super", "geometry", "geography", "hllsketch", "varbyte", "binary varying"];

const NUMERIC: &[&str] = &["smallint", "integer", "bigint", "numeric", "real", "double precision"];

fn base_type(data_type: &str) -> &str {
    data_type.split('(').next().unwrap_or(data_type).trim()
}

fn schema_of(object: &ObjectRef) -> DbResult<&str> {
    object.schema.as_deref().ok_or_else(|| DbError::invalid("A schema is required."))
}

fn gone(object: &ObjectRef) -> DbError {
    DbError::not_found(format!("{} no longer exists.", D.display_name(object)))
}

pub async fn schemas(client: &Client, database: &str) -> DbResult<Vec<Node>> {
    let sql = format!(
        "SELECT n.nspname::text, \
                CASE WHEN e.esoid IS NULL \
                     THEN (SELECT count(*) FROM pg_class c WHERE c.relnamespace = n.oid AND c.relkind IN ('r', 'v')) \
                     ELSE (SELECT count(*) FROM svv_external_tables t WHERE t.schemaname = n.nspname) END::int8, \
                obj_description(n.oid, 'pg_namespace'), e.databasename::text \
         FROM pg_namespace n LEFT JOIN svv_external_schemas e ON e.esoid = n.oid \
         WHERE {USER_SCHEMA} ORDER BY n.nspname <> 'public', n.nspname"
    );
    let rows = client.query(&sql, &[]).await.map_err(pg)?;
    Ok(rows
        .iter()
        .map(|r| {
            let name: String = r.get(0);
            let mut node = Node::container(ObjectRef::new(NodeKind::Schema, database, Some(name.clone()), name));
            node.row_estimate = Some(r.get(1));
            let external: Option<String> = r.get(3);
            node.comment = r
                .get::<_, Option<String>>(2)
                .or_else(|| external.map(|db| format!("External schema (Spectrum) on {db}, read-only")));
            node
        })
        .collect())
}

fn relation_leaf(database: &str, schema: String, name: String, relkind: &str, comment: Option<String>) -> Node {
    let kind = if relkind == "v" { NodeKind::View } else { NodeKind::Table };
    // No indexes or triggers to expand to.
    let mut node = Node::leaf(ObjectRef::new(kind, database, Some(schema), name));
    node.comment = comment;
    node
}

/// Tables and views of a local schema (or the tables of an external one),
/// then the function and procedure groups.
pub async fn relations(client: &Client, database: &str, schema: &str) -> DbResult<Vec<Node>> {
    let rows = client
        .query(
            "SELECT n.nspname::text, c.relname::text, c.relkind::text, obj_description(c.oid, 'pg_class') \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('r', 'v') \
             UNION ALL \
             SELECT schemaname::text, tablename::text, 'r', NULL::text FROM svv_external_tables WHERE schemaname = $1 \
             ORDER BY 3, 2",
            &[&schema],
        )
        .await
        .map_err(pg)?;
    let mut nodes: Vec<Node> =
        rows.iter().map(|r| relation_leaf(database, r.get(0), r.get(1), &r.get::<_, String>(2), r.get(3))).collect();
    let counts = client
        .query_one(
            "SELECT COALESCE(SUM(CASE WHEN p.prokind = 'p' THEN 0 ELSE 1 END), 0)::int8, \
                    COALESCE(SUM(CASE WHEN p.prokind = 'p' THEN 1 ELSE 0 END), 0)::int8 \
             FROM pg_proc_info p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = $1",
            &[&schema],
        )
        .await
        .map_err(pg)?;
    for (i, group) in [GROUP_FUNCTIONS, GROUP_PROCEDURES].into_iter().enumerate() {
        let count: i64 = counts.get(i);
        if count > 0 {
            let mut node = Node::container(ObjectRef::new(NodeKind::Group, database, Some(schema.to_owned()), group));
            node.row_estimate = Some(count);
            nodes.push(node);
        }
    }
    Ok(nodes)
}

/// Functions or procedures of a schema; `parent` is the argument type list.
pub async fn group_children(client: &Client, group: &ObjectRef) -> DbResult<Vec<Node>> {
    let schema = schema_of(group)?;
    let (kind, procedures) = match group.name.as_str() {
        GROUP_FUNCTIONS => (NodeKind::Function, false),
        GROUP_PROCEDURES => (NodeKind::Procedure, true),
        other => return Err(DbError::invalid(format!("Unknown group `{other}`."))),
    };
    let rows = client
        .query(
            "SELECT p.proname::text, oidvectortypes(p.proargtypes)::text, obj_description(p.prooid, 'pg_proc') \
             FROM pg_proc_info p JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = $1 AND (COALESCE(p.prokind, 'f') = 'p') = $2 ORDER BY 1, 2",
            &[&schema, &procedures],
        )
        .await
        .map_err(pg)?;
    Ok(rows
        .iter()
        .map(|r| {
            let object = ObjectRef::new(kind, group.database.clone(), Some(schema.to_owned()), r.get::<_, String>(0))
                .with_parent(r.get::<_, String>(1));
            let mut node = Node::leaf(object);
            node.comment = r.get(2);
            node
        })
        .collect())
}

pub async fn search(client: &Client, database: &str, query: &str, limit: u32) -> DbResult<Vec<Node>> {
    let sql = format!(
        "SELECT n.nspname::text, c.relname::text, c.relkind::text, obj_description(c.oid, 'pg_class') \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relname ILIKE $1 AND c.relkind IN ('r', 'v') AND {USER_SCHEMA} \
         ORDER BY length(c.relname), c.relname LIMIT $2"
    );
    let rows = client.query(&sql, &[&like_contains(query), &i64::from(limit)]).await.map_err(pg)?;
    Ok(rows.iter().map(|r| relation_leaf(database, r.get(0), r.get(1), &r.get::<_, String>(2), r.get(3))).collect())
}

/// A table or view of a local schema, or a table of an external one.
enum Relation {
    Local {
        oid: u32,
        kind: NodeKind,
        estimate: Option<i64>,
        comment: Option<String>,
    },
    External {
        location: Option<String>,
        input_format: Option<String>,
        output_format: Option<String>,
        serde: Option<String>,
    },
}

async fn relation(client: &Client, object: &ObjectRef) -> DbResult<Relation> {
    let schema = schema_of(object)?;
    let local = client
        .query_opt(
            "SELECT c.oid, c.relkind::text, c.reltuples::int8, obj_description(c.oid, 'pg_class') \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ('r', 'v')",
            &[&schema, &object.name],
        )
        .await
        .map_err(pg)?;
    if let Some(r) = local {
        let kind = if r.get::<_, String>(1) == "v" { NodeKind::View } else { NodeKind::Table };
        return Ok(Relation::Local {
            oid: r.get(0),
            kind,
            estimate: Some(r.get::<_, i64>(2)).filter(|n| *n >= 0),
            comment: r.get(3),
        });
    }
    let external = client
        .query_opt(
            "SELECT location::text, input_format::text, output_format::text, serialization_lib::text \
             FROM svv_external_tables WHERE schemaname = $1 AND tablename = $2",
            &[&schema, &object.name],
        )
        .await
        .map_err(pg)?
        .ok_or_else(|| gone(object))?;
    Ok(Relation::External {
        location: external.get(0),
        input_format: external.get(1),
        output_format: external.get(2),
        serde: external.get(3),
    })
}

/// Rows and size from `svv_table_info`, which lists only tables with data
/// and only those the account may see.
async fn table_info(client: &Client, object: &ObjectRef) -> Option<(i64, i64)> {
    let row = client
        .query_opt(
            "SELECT size::int8, tbl_rows::int8 FROM svv_table_info WHERE \"schema\" = $1 AND \"table\" = $2",
            &[&object.schema, &object.name],
        )
        .await
        .ok()??;
    Some((row.get::<_, i64>(0) * 1024 * 1024, row.get(1)))
}

pub async fn row_estimate(client: &Client, object: &ObjectRef) -> DbResult<Option<i64>> {
    Ok(match relation(client, object).await? {
        Relation::Local { kind: NodeKind::Table, estimate, .. } => {
            table_info(client, object).await.map(|(_, rows)| rows).or(estimate)
        }
        _ => None,
    })
}

/// One column as the DDL needs it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RsColumn {
    pub name: String,
    pub data_type: String,
    pub not_null: bool,
    /// `pg_attrdef.adsrc`, which also carries identity columns as `"identity"(…)`.
    pub default: Option<String>,
    pub encoding: Option<String>,
    pub distkey: bool,
    /// Position in the sort key; negative for interleaved sort keys, 0 when not in it.
    pub sortkey: i32,
    pub comment: Option<String>,
    pub type_oid: u32,
}

/// `IDENTITY(seed, step)` (or `GENERATED BY DEFAULT AS IDENTITY(…)`) from an
/// identity column's default, `"identity"(108603, 0, '1,1'::text)`.
pub(crate) fn identity_clause(default: &str) -> Option<String> {
    let by_default = default.starts_with("\"default_identity\"");
    if !by_default && !default.starts_with("\"identity\"") {
        return None;
    }
    let quoted = default.split('\'').nth(1).unwrap_or("1,1");
    let (seed, step) = quoted.split_once(',').unwrap_or(("1", "1"));
    let clause = format!("IDENTITY({}, {})", seed.trim(), step.trim());
    Some(if by_default { format!("GENERATED BY DEFAULT AS {clause}") } else { clause })
}

async fn local_columns(client: &Client, oid: u32) -> DbResult<Vec<RsColumn>> {
    let rows = client
        .query(
            "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod)::text, a.attnotnull, d.adsrc::text, \
                    format_encoding(a.attencodingtype::integer)::text, a.attisdistkey, a.attsortkeyord::int4, \
                    col_description(a.attrelid, a.attnum), a.atttypid \
             FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
            &[&oid],
        )
        .await
        .map_err(pg)?;
    Ok(rows
        .iter()
        .map(|r| RsColumn {
            name: r.get(0),
            data_type: r.get(1),
            not_null: r.get(2),
            default: r.get(3),
            encoding: r.get::<_, Option<String>>(4).filter(|e| !e.is_empty() && e != "none"),
            distkey: r.get(5),
            sortkey: r.get(6),
            comment: r.get(7),
            type_oid: r.get(8),
        })
        .collect())
}

/// Columns of a late-binding view (`WITH NO SCHEMA BINDING`), which keeps none in `pg_attribute`.
async fn view_columns(client: &Client, object: &ObjectRef) -> DbResult<Vec<RsColumn>> {
    let rows = client
        .query(
            "SELECT column_name::text, data_type::text, is_nullable::text = 'NO' FROM svv_columns \
             WHERE table_schema = $1 AND table_name = $2 ORDER BY ordinal_position",
            &[&object.schema, &object.name],
        )
        .await
        .map_err(pg)?;
    Ok(rows
        .iter()
        .map(|r| RsColumn { name: r.get(0), data_type: r.get(1), not_null: r.get(2), ..RsColumn::default() })
        .collect())
}

/// Columns of an external table, partition columns last as `PARTITIONED BY` lists them.
async fn external_columns(client: &Client, object: &ObjectRef) -> DbResult<Vec<(RsColumn, bool)>> {
    let rows = client
        .query(
            "SELECT columnname::text, external_type::text, is_nullable::text = 'false', part_key::int4 \
             FROM svv_external_columns WHERE schemaname = $1 AND tablename = $2 \
             ORDER BY part_key > 0, part_key, columnnum",
            &[&object.schema, &object.name],
        )
        .await
        .map_err(pg)?;
    Ok(rows
        .iter()
        .map(|r| {
            let column = RsColumn { name: r.get(0), data_type: r.get(1), not_null: r.get(2), ..RsColumn::default() };
            (column, r.get::<_, i32>(3) > 0)
        })
        .collect())
}

/// Constraints with their definitions; Redshift keeps them informational (not enforced).
async fn constraints(client: &Client, oid: u32) -> DbResult<Vec<ConstraintInfo>> {
    let rows = client
        .query(
            "SELECT conname::text, contype::text, pg_get_constraintdef(oid)::text FROM pg_constraint \
             WHERE conrelid = $1 ORDER BY CASE contype WHEN 'p' THEN 0 WHEN 'u' THEN 1 WHEN 'f' THEN 2 ELSE 3 END, conname",
            &[&oid],
        )
        .await
        .map_err(pg)?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let kind = match r.get::<_, String>(1).as_str() {
                "p" => ConstraintKind::PrimaryKey,
                "u" => ConstraintKind::Unique,
                "f" => ConstraintKind::ForeignKey,
                "c" => ConstraintKind::Check,
                _ => return None,
            };
            Some(ConstraintInfo { kind, name: r.get(0), definition: r.get(2) })
        })
        .collect())
}

/// Identifiers of a parenthesised list such as `(id, "Line No")`, unquoted.
pub(crate) fn ident_list(list: &str) -> Vec<String> {
    let inner = list.trim().trim_start_matches('(').trim_end_matches(')');
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut current).trim().to_owned()),
            c => current.push(c),
        }
    }
    out.push(current.trim().to_owned());
    out.retain(|s| !s.is_empty());
    out
}

/// The key columns of `PRIMARY KEY (a, b)` or `UNIQUE (a)`.
pub(crate) fn key_columns(definition: &str) -> Vec<String> {
    definition.find('(').map(|i| ident_list(&definition[i..])).unwrap_or_default()
}

/// A foreign key as `pg_get_constraintdef` spells it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ForeignKeyDef {
    pub columns: Vec<String>,
    /// The target's schema, when the definition qualifies it.
    pub schema: Option<String>,
    pub table: String,
    pub target_columns: Vec<String>,
}

/// Reads `FOREIGN KEY (a) REFERENCES schema.table(b)`.
pub(crate) fn parse_foreign_key(definition: &str) -> Option<ForeignKeyDef> {
    let rest = definition.trim().strip_prefix("FOREIGN KEY")?;
    let (columns, target) = rest.split_once("REFERENCES")?;
    let target = target.trim();
    let open = target.find('(')?;
    let name = target[..open].trim();
    let target_columns = ident_list(&target[open..=target.find(')')?]);
    let mut parts = ident_list(&format!("({})", name.replace('.', ",")));
    let table = parts.pop()?;
    Some(ForeignKeyDef { columns: ident_list(columns), schema: parts.pop(), table, target_columns })
}

fn foreign_key(database: &str, source: &ObjectRef, name: String, definition: &str) -> Option<ForeignKey> {
    let def = parse_foreign_key(definition)?;
    // An unqualified target is on the search path; most often the same schema.
    let target = ObjectRef::new(NodeKind::Table, database, def.schema.or_else(|| source.schema.clone()), def.table);
    Some(ForeignKey {
        name,
        source: source.clone(),
        columns: def.columns,
        target,
        target_columns: def.target_columns,
        on_update: None,
        on_delete: None,
    })
}

fn column_info(c: &RsColumn, primary_key: &[String], external: bool) -> ColumnInfo {
    let base = base_type(&c.data_type);
    let identity = c.default.as_deref().is_some_and(|d| identity_clause(d).is_some());
    ColumnInfo {
        name: c.name.clone(),
        data_type: c.data_type.clone(),
        nullable: !c.not_null,
        default: c.default.clone().filter(|_| !identity),
        primary_key: primary_key.contains(&c.name),
        generated: identity,
        identity,
        editable: !external && !identity && !NOT_EDITABLE.contains(&base),
        numeric: NUMERIC.contains(&base) || base == "decimal",
        comment: c.comment.clone(),
        read_as_text: c.type_oid != 0 && !is_native_oid(c.type_oid),
    }
}

pub async fn structure(client: &Client, object: &ObjectRef) -> DbResult<TableStructure> {
    let rel = relation(client, object).await?;
    let mut s = TableStructure {
        object: object.clone(),
        columns: Vec::new(),
        constraints: Vec::new(),
        indexes: Vec::new(),
        row_identity: Vec::new(),
        full_row_key: Vec::new(),
        comment: None,
        row_estimate: None,
        foreign_keys: Vec::new(),
        size: None,
    };
    match rel {
        Relation::External { location, .. } => {
            s.object.kind = NodeKind::Table;
            s.columns =
                external_columns(client, object).await?.iter().map(|(c, _)| column_info(c, &[], true)).collect();
            s.comment = location.map(|l| format!("External table at {l}, read-only"));
        }
        Relation::Local { oid, kind, estimate, comment } => {
            s.object.kind = kind;
            let mut columns = local_columns(client, oid).await?;
            if columns.is_empty() && kind == NodeKind::View {
                columns = view_columns(client, object).await?;
            }
            s.constraints = constraints(client, oid).await?;
            let primary_key = s
                .constraints
                .iter()
                .find(|c| c.kind == ConstraintKind::PrimaryKey)
                .map(|c| key_columns(&c.definition))
                .unwrap_or_default();
            s.columns = columns.iter().map(|c| column_info(c, &primary_key, false)).collect();
            s.foreign_keys = s
                .constraints
                .iter()
                .filter(|c| c.kind == ConstraintKind::ForeignKey)
                .filter_map(|c| foreign_key(&object.database, object, c.name.clone(), &c.definition))
                .collect();
            // Keys are not enforced: an edit that matches several rows is rolled back (`check_affected`).
            if kind == NodeKind::Table {
                s.row_identity = primary_key;
                let info = table_info(client, object).await;
                s.size = info.map(|(bytes, _)| TableSize { total_bytes: bytes, table_bytes: bytes, index_bytes: 0 });
                s.row_estimate = info.map(|(_, rows)| rows).or(estimate);
            }
            s.comment = comment;
        }
    }
    Ok(s)
}

/// `DISTSTYLE` from `pg_class.reldiststyle`.
fn diststyle(code: i32) -> &'static str {
    match code {
        0 => "EVEN",
        1 => "KEY",
        8 => "ALL",
        _ => "AUTO",
    }
}

/// `CREATE TABLE` rebuilt from the catalog, with identity, encodings,
/// distribution and sort keys, for clusters without `SHOW TABLE`.
pub(crate) fn table_ddl(
    object: &ObjectRef,
    columns: &[RsColumn],
    constraints: &[ConstraintInfo],
    diststyle_code: i32,
    comment: Option<&str>,
) -> String {
    let name = D.display_name(object);
    let mut lines = Vec::new();
    for c in columns {
        let mut line = format!("  {} {}", D.display_ident(&c.name), c.data_type);
        match c.default.as_deref().map(|d| (d, identity_clause(d))) {
            Some((_, Some(identity))) => {
                let _ = write!(line, " {identity}");
            }
            Some((default, None)) => {
                let _ = write!(line, " DEFAULT {default}");
            }
            None => {}
        }
        if c.not_null {
            line.push_str(" NOT NULL");
        }
        if let Some(encoding) = &c.encoding {
            let _ = write!(line, " ENCODE {encoding}");
        }
        lines.push(line);
    }
    for c in constraints {
        lines.push(format!("  CONSTRAINT {} {}", D.display_ident(&c.name), c.definition));
    }
    let style = diststyle(diststyle_code);
    let mut out = format!("CREATE TABLE {name} (\n{}\n)\nDISTSTYLE {style}", lines.join(",\n"));
    if let Some(key) = columns.iter().find(|c| c.distkey).filter(|_| style == "KEY") {
        let _ = write!(out, "\nDISTKEY ({})", D.display_ident(&key.name));
    }
    let mut sort: Vec<&RsColumn> = columns.iter().filter(|c| c.sortkey != 0).collect();
    sort.sort_by_key(|c| c.sortkey.abs());
    if !sort.is_empty() {
        let interleaved = sort.iter().any(|c| c.sortkey < 0);
        let list = sort.iter().map(|c| D.display_ident(&c.name)).collect::<Vec<_>>().join(", ");
        let _ = write!(out, "\n{} SORTKEY ({list})", if interleaved { "INTERLEAVED" } else { "COMPOUND" });
    }
    out.push(';');
    if let Some(comment) = comment {
        let _ = write!(out, "\n\nCOMMENT ON TABLE {name} IS {};", D.quote_literal(comment));
    }
    for c in columns.iter().filter(|c| c.comment.is_some()) {
        let text = c.comment.as_deref().unwrap_or_default();
        let _ = write!(out, "\nCOMMENT ON COLUMN {name}.{} IS {};", D.display_ident(&c.name), D.quote_literal(text));
    }
    out + "\n"
}

/// `CREATE EXTERNAL TABLE` from `svv_external_tables` and `svv_external_columns`.
pub(crate) fn external_table_ddl(
    object: &ObjectRef,
    columns: &[(RsColumn, bool)],
    serde: Option<&str>,
    input_format: Option<&str>,
    output_format: Option<&str>,
    location: Option<&str>,
) -> String {
    let list = |partition: bool| {
        columns
            .iter()
            .filter(|(_, p)| *p == partition)
            .map(|(c, _)| format!("  {} {}", D.display_ident(&c.name), c.data_type))
            .collect::<Vec<_>>()
    };
    let mut out = format!("CREATE EXTERNAL TABLE {} (\n{}\n)", D.display_name(object), list(false).join(",\n"));
    let partitions = list(true);
    if !partitions.is_empty() {
        let _ = write!(out, "\nPARTITIONED BY (\n{}\n)", partitions.join(",\n"));
    }
    if let Some(serde) = serde.filter(|s| !s.is_empty()) {
        let _ = write!(out, "\nROW FORMAT SERDE {}", D.quote_literal(serde));
    }
    if let (Some(input), Some(output)) = (input_format, output_format) {
        let _ =
            write!(out, "\nSTORED AS INPUTFORMAT {} OUTPUTFORMAT {}", D.quote_literal(input), D.quote_literal(output));
    }
    if let Some(location) = location {
        let _ = write!(out, "\nLOCATION {}", D.quote_literal(location));
    }
    out + ";\n"
}

/// The text of a `SHOW TABLE` / `SHOW PROCEDURE` statement.
async fn show(client: &Client, sql: &str) -> Option<String> {
    let messages = client.simple_query(sql).await.ok()?;
    let text: Vec<&str> = messages
        .iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(r) => r.get(0),
            _ => None,
        })
        .collect();
    (!text.is_empty()).then(|| text.join("\n"))
}

/// Definition of a function or procedure. Procedures come from `SHOW
/// PROCEDURE`; functions (SQL, Python and Lambda UDFs) are rebuilt from `pg_proc`.
async fn routine_ddl(client: &Client, object: &ObjectRef) -> DbResult<String> {
    let schema = schema_of(object)?;
    let args = object.parent.as_deref().unwrap_or_default();
    let missing = || DbError::not_found(format!("{} no longer exists.", object.name));
    let row = client
        .query_opt(
            "SELECT p.proname::text, oidvectortypes(p.proargtypes)::text, array_to_string(p.proargnames, ',')::text, \
                    format_type(p.prorettype, NULL)::text, p.provolatile::text, l.lanname::text, p.prosrc::text \
             FROM pg_proc_info p JOIN pg_namespace n ON n.oid = p.pronamespace JOIN pg_language l ON l.oid = p.prolang \
             WHERE n.nspname = $1 AND p.proname = $2 AND oidvectortypes(p.proargtypes) = $3",
            &[&schema, &object.name, &args],
        )
        .await
        .map_err(pg)?
        .ok_or_else(missing)?;
    let name = D.display_name(object);
    if object.kind == NodeKind::Procedure
        && let Some(text) = show(client, &format!("SHOW PROCEDURE {}({args})", D.quoted_name(object))).await
    {
        return Ok(text.trim_end().to_owned() + "\n");
    }
    let types: Vec<&str> = args.split(", ").filter(|t| !t.is_empty()).collect();
    let names: Vec<String> =
        row.get::<_, Option<String>>(2).map(|n| n.split(',').map(str::to_owned).collect()).unwrap_or_default();
    let params = types
        .iter()
        .enumerate()
        .map(|(i, t)| match names.get(i).filter(|n| !n.is_empty()) {
            Some(n) => format!("{} {t}", D.display_ident(n)),
            None => (*t).to_owned(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let body: String = row.get(6);
    let language: String = row.get(5);
    if object.kind == NodeKind::Procedure {
        return Ok(format!(
            "CREATE OR REPLACE PROCEDURE {name}({params})\nAS $$\n{}\n$$ LANGUAGE {language};\n",
            body.trim()
        ));
    }
    let volatility = match row.get::<_, String>(4).as_str() {
        "i" => "IMMUTABLE",
        "s" => "STABLE",
        _ => "VOLATILE",
    };
    Ok(format!(
        "CREATE OR REPLACE FUNCTION {name}({params})\nRETURNS {} {volatility}\nAS $$\n{}\n$$ LANGUAGE {language};\n",
        row.get::<_, String>(3),
        body.trim()
    ))
}

/// DDL: `SHOW TABLE` where the cluster has it, else rebuilt from the catalog;
/// views from `pg_get_viewdef`, which returns a late-binding view's whole statement.
pub async fn ddl(client: &Client, object: &ObjectRef) -> DbResult<String> {
    if matches!(object.kind, NodeKind::Function | NodeKind::Procedure) {
        return routine_ddl(client, object).await;
    }
    if !object.kind.is_relation() {
        return Err(DbError::invalid("This object has no definition."));
    }
    match relation(client, object).await? {
        Relation::External { location, input_format, output_format, serde } => {
            let columns = external_columns(client, object).await?;
            Ok(external_table_ddl(
                object,
                &columns,
                serde.as_deref(),
                input_format.as_deref(),
                output_format.as_deref(),
                location.as_deref(),
            ))
        }
        Relation::Local { oid, kind: NodeKind::View, .. } => {
            let def: String =
                client.query_one("SELECT pg_get_viewdef($1::oid, true)::text", &[&oid]).await.map_err(pg)?.get(0);
            let def = def.trim_end();
            if def.get(..6).is_some_and(|w| w.eq_ignore_ascii_case("create")) {
                return Ok(def.to_owned() + "\n");
            }
            Ok(format!("CREATE VIEW {} AS\n{def}\n", D.display_name(object)))
        }
        Relation::Local { oid, comment, .. } => {
            if let Some(text) = show(client, &format!("SHOW TABLE {}", D.quoted_name(object))).await {
                return Ok(text.trim_end().to_owned() + "\n");
            }
            let columns = local_columns(client, oid).await?;
            let constraints = constraints(client, oid).await?;
            let style: i32 = client
                .query_one("SELECT reldiststyle::int4 FROM pg_class WHERE oid = $1", &[&oid])
                .await
                .map_err(pg)?
                .get(0);
            Ok(table_ddl(object, &columns, &constraints, style, comment.as_deref()))
        }
    }
}

/// Foreign keys both ways and the views that read the table. Late-binding
/// views record no dependencies.
pub async fn dependencies(client: &Client, object: &ObjectRef) -> DbResult<Dependencies> {
    let Relation::Local { oid, .. } = relation(client, object).await? else { return Ok(Dependencies::default()) };
    let db = object.database.as_str();
    let mut deps = Dependencies::default();
    let rows = client
        .query(
            "SELECT k.conname::text, pg_get_constraintdef(k.oid)::text, sn.nspname::text, s.relname::text, k.conrelid = $1 \
             FROM pg_constraint k JOIN pg_class s ON s.oid = k.conrelid JOIN pg_namespace sn ON sn.oid = s.relnamespace \
             WHERE k.contype = 'f' AND (k.conrelid = $1 OR k.confrelid = $1) ORDER BY 1",
            &[&oid],
        )
        .await
        .map_err(pg)?;
    for r in &rows {
        let source = ObjectRef::new(NodeKind::Table, db, Some(r.get(2)), r.get::<_, String>(3));
        let name: String = r.get(0);
        let Some(fk) = foreign_key(db, &source, name.clone(), &r.get::<_, String>(1)) else { continue };
        let relation = format!("foreign key {name}");
        if r.get::<_, bool>(4) {
            deps.uses.push(Dependency { object: fk.target, relation });
        } else {
            deps.used_by.push(Dependency { object: fk.source, relation });
        }
    }
    let views = client
        .query(
            "SELECT DISTINCT vn.nspname::text, v.relname::text FROM pg_depend d \
             JOIN pg_rewrite r ON r.oid = d.objid JOIN pg_class v ON v.oid = r.ev_class \
             JOIN pg_namespace vn ON vn.oid = v.relnamespace \
             WHERE d.classid = 'pg_rewrite'::regclass AND d.refobjid = $1 AND r.ev_class <> $1 ORDER BY 1, 2",
            &[&oid],
        )
        .await
        .map_err(pg)?;
    for r in &views {
        let view = ObjectRef::new(NodeKind::View, db, Some(r.get(0)), r.get::<_, String>(1));
        deps.used_by.push(Dependency { object: view, relation: "view".into() });
    }
    Ok(deps)
}

/// Tables, columns and foreign keys of one schema or every local user schema.
pub async fn schema_model(client: &Client, database: &str, schema: Option<&str>) -> DbResult<SchemaModel> {
    let filter = format!("(($1::text IS NULL AND {USER_SCHEMA}) OR n.nspname = $1)");
    let rows = client
        .query(
            &format!(
                "SELECT n.nspname::text, c.relname::text, c.relkind::text, a.attname::text, \
                        format_type(a.atttypid, a.atttypmod)::text, NOT a.attnotnull \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped \
                 WHERE c.relkind IN ('r', 'v') AND {filter} ORDER BY 1, 2, a.attnum LIMIT $2"
            ),
            &[&schema, &(SchemaModel::MAX_COLUMNS as i64)],
        )
        .await
        .map_err(pg)?;
    let keys = client
        .query(
            &format!(
                "SELECT n.nspname::text, c.relname::text, k.conname::text, k.contype::text, pg_get_constraintdef(k.oid)::text \
                 FROM pg_constraint k JOIN pg_class c ON c.oid = k.conrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE k.contype IN ('p', 'f') AND {filter} ORDER BY 1, 2, 3"
            ),
            &[&schema],
        )
        .await
        .map_err(pg)?;
    let mut model = SchemaModel::default();
    let mut primary: Vec<(String, String, Vec<String>)> = Vec::new();
    for r in &keys {
        let (schema, table, name, kind, definition): (String, String, String, String, String) =
            (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4));
        if kind == "p" {
            primary.push((schema, table, key_columns(&definition)));
        } else {
            let source = ObjectRef::new(NodeKind::Table, database, Some(schema), table);
            model.foreign_keys.extend(foreign_key(database, &source, name, &definition));
        }
    }
    for r in &rows {
        let (schema, name): (String, String) = (r.get(0), r.get(1));
        let same = model
            .tables
            .last()
            .is_some_and(|t: &ModelTable| t.object.schema.as_deref() == Some(&schema) && t.object.name == name);
        if !same {
            let kind = if r.get::<_, String>(2) == "v" { NodeKind::View } else { NodeKind::Table };
            model
                .tables
                .push(ModelTable { object: ObjectRef::new(kind, database, Some(schema), name), columns: Vec::new() });
        }
        if let Some(table) = model.tables.last_mut() {
            let column: String = r.get(3);
            let primary_key = primary.iter().any(|(s, t, cols)| {
                table.object.schema.as_deref() == Some(s) && &table.object.name == t && cols.contains(&column)
            });
            table.columns.push(ModelColumn { name: column, data_type: r.get(4), nullable: r.get(5), primary_key });
        }
    }
    model.truncated = model.tables.len() > SchemaModel::MAX_TABLES || rows.len() >= SchemaModel::MAX_COLUMNS;
    model.tables.truncate(SchemaModel::MAX_TABLES);
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> ObjectRef {
        ObjectRef::new(NodeKind::Table, "dev", Some("public".into()), "sales")
    }

    fn column(name: &str, data_type: &str) -> RsColumn {
        RsColumn { name: name.into(), data_type: data_type.into(), ..RsColumn::default() }
    }

    #[test]
    fn reads_identity_defaults() {
        assert_eq!(identity_clause(r#""identity"(108603, 0, '1,1'::text)"#).as_deref(), Some("IDENTITY(1, 1)"));
        assert_eq!(
            identity_clause(r#""default_identity"(108603, 1, '100,10'::text)"#).as_deref(),
            Some("GENERATED BY DEFAULT AS IDENTITY(100, 10)")
        );
        assert_eq!(identity_clause("'open'::character varying"), None);
    }

    #[test]
    fn reads_constraint_definitions() {
        assert_eq!(key_columns("PRIMARY KEY (salesid)"), ["salesid"]);
        assert_eq!(key_columns(r#"UNIQUE (a, "Line ""No""")"#), ["a", r#"Line "No""#]);
        assert_eq!(
            parse_foreign_key("FOREIGN KEY (listid, sellerid) REFERENCES listing(listid, sellerid)"),
            Some(ForeignKeyDef {
                columns: vec!["listid".into(), "sellerid".into()],
                schema: None,
                table: "listing".into(),
                target_columns: vec!["listid".into(), "sellerid".into()],
            })
        );
        let def = parse_foreign_key(r#"FOREIGN KEY (buyerid) REFERENCES crm."Users"(userid)"#).unwrap();
        assert_eq!(
            (def.schema.as_deref(), def.table.as_str(), def.target_columns),
            (Some("crm"), "Users", vec!["userid".to_owned()])
        );
        assert_eq!(parse_foreign_key("CHECK (qty > 0)"), None);
        let fk = foreign_key("dev", &table(), "fk".into(), "FOREIGN KEY (eventid) REFERENCES event(eventid)").unwrap();
        assert_eq!(fk.target, ObjectRef::new(NodeKind::Table, "dev", Some("public".into()), "event"));
    }

    #[test]
    fn rebuilds_table_ddl_with_keys_and_encodings() {
        let columns = [
            RsColumn {
                not_null: true,
                default: Some(r#""identity"(1, 0, '1,1'::text)"#.into()),
                encoding: Some("az64".into()),
                ..column("salesid", "integer")
            },
            RsColumn { distkey: true, sortkey: 2, encoding: Some("az64".into()), ..column("listid", "integer") },
            RsColumn { sortkey: 1, comment: Some("Sold on".into()), ..column("dateid", "smallint") },
            RsColumn {
                default: Some("'web'::character varying".into()),
                encoding: Some("lzo".into()),
                ..column("Channel", "character varying(20)")
            },
        ];
        let constraints = [ConstraintInfo {
            kind: ConstraintKind::PrimaryKey,
            name: "sales_pkey".into(),
            definition: "PRIMARY KEY (salesid)".into(),
        }];
        assert_eq!(
            table_ddl(&table(), &columns, &constraints, 1, Some("It's sales")),
            "CREATE TABLE public.sales (
  salesid integer IDENTITY(1, 1) NOT NULL ENCODE az64,
  listid integer ENCODE az64,
  dateid smallint,
  \"Channel\" character varying(20) DEFAULT 'web'::character varying ENCODE lzo,
  CONSTRAINT sales_pkey PRIMARY KEY (salesid)
)
DISTSTYLE KEY
DISTKEY (listid)
COMPOUND SORTKEY (dateid, listid);

COMMENT ON TABLE public.sales IS 'It''s sales';
COMMENT ON COLUMN public.sales.dateid IS 'Sold on';
"
        );
        let interleaved =
            [RsColumn { sortkey: -1, ..column("a", "int") }, RsColumn { sortkey: -2, ..column("b", "int") }];
        let ddl = table_ddl(&table(), &interleaved, &[], 9, None);
        assert!(ddl.ends_with(")\nDISTSTYLE AUTO\nINTERLEAVED SORTKEY (a, b);\n"), "{ddl}");
    }

    #[test]
    fn rebuilds_external_table_ddl() {
        let spectrum = ObjectRef::new(NodeKind::Table, "dev", Some("spectrum".into()), "sales");
        let columns = [
            (column("salesid", "int"), false),
            (column("price", "decimal(8,2)"), false),
            (column("saledate", "date"), true),
        ];
        assert_eq!(
            external_table_ddl(
                &spectrum,
                &columns,
                Some("org.apache.hadoop.hive.ql.io.parquet.serde.ParquetHiveSerDe"),
                Some("org.apache.hadoop.hive.ql.io.parquet.MapredParquetInputFormat"),
                Some("org.apache.hadoop.hive.ql.io.parquet.MapredParquetOutputFormat"),
                Some("s3://bucket/sales/"),
            ),
            "CREATE EXTERNAL TABLE spectrum.sales (
  salesid int,
  price decimal(8,2)
)
PARTITIONED BY (
  saledate date
)
ROW FORMAT SERDE 'org.apache.hadoop.hive.ql.io.parquet.serde.ParquetHiveSerDe'
STORED AS INPUTFORMAT 'org.apache.hadoop.hive.ql.io.parquet.MapredParquetInputFormat' OUTPUTFORMAT 'org.apache.hadoop.hive.ql.io.parquet.MapredParquetOutputFormat'
LOCATION 's3://bucket/sales/';
"
        );
    }

    #[test]
    fn external_and_special_columns_are_read_only() {
        let pk = ["id".to_owned()];
        let id = column_info(&RsColumn { not_null: true, ..column("id", "bigint") }, &pk, false);
        assert!(id.editable && id.primary_key && id.numeric && !id.nullable);
        let doc = column_info(&column("doc", "super"), &pk, false);
        assert!(!doc.editable && !doc.primary_key);
        let identity = column_info(
            &RsColumn { default: Some(r#""identity"(1, 0, '1,1'::text)"#.into()), ..column("n", "integer") },
            &pk,
            false,
        );
        assert!(identity.identity && identity.generated && !identity.editable && identity.default.is_none());
        assert!(!column_info(&column("name", "varchar(20)"), &pk, true).editable);
    }
}
