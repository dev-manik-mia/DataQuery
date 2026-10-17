//! Schema diff between two schemas (or MySQL databases), possibly on
//! different connections, and the migration SQL that makes the target match
//! the source. The SQL is shown for review and only runs when the user runs it.
//!
//! Wide-column tables (Cassandra, `ScyllaDB`) carry their partition and
//! clustering keys: new tables are created with that layout, and a key that
//! differs is reported, since CQL cannot change a primary key.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::error::{DbError, DbResult, ErrorKind};
use crate::node::{NodeKind, ObjectRef};
use crate::schema::{ColumnInfo, ConstraintKind, KeyRole, KeyRoleKind, TableStructure};
use crate::sql::Dialect;
use crate::sql::ddl::{self, ColumnDef, DefaultChange, IndexColumn, SchemaChange};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum DiffChange {
    /// Only in the source: created in the target.
    Added,
    /// Only in the target: dropped from it.
    Removed,
    Changed,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct DiffItem {
    pub table: String,
    /// `table`, `column`, `index` or `constraint`.
    pub kind: String,
    pub name: String,
    pub change: DiffChange,
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize, TS)]
#[ts(export)]
pub struct SchemaDiff {
    pub items: Vec<DiffItem>,
    /// Statements that make the target match the source, for review.
    pub sql: String,
    /// Differences that need manual SQL (e.g. expression indexes).
    pub warnings: Vec<String>,
}

/// One compared table, with its wide-column key layout (empty for other engines).
#[derive(Debug, Clone)]
pub struct DiffTable {
    pub structure: TableStructure,
    pub keys: Vec<KeyRole>,
}

impl From<TableStructure> for DiffTable {
    fn from(structure: TableStructure) -> Self {
        Self { structure, keys: Vec::new() }
    }
}

impl DiffTable {
    /// The primary key as compared between sides, e.g. `(a, b), c desc`.
    fn key_layout(&self) -> String {
        if self.keys.is_empty() {
            return primary_key(&self.structure).join(", ");
        }
        let partition: Vec<&str> =
            self.keys.iter().filter(|k| k.role == KeyRoleKind::Partition).map(|k| k.column.as_str()).collect();
        let clustering = self
            .keys
            .iter()
            .filter(|k| k.role == KeyRoleKind::Clustering)
            .map(|k| format!(", {}{}", k.column, if k.descending { " desc" } else { "" }));
        format!("({}){}", partition.join(", "), clustering.collect::<String>())
    }
}

/// Where the compared tables live on the target side.
#[derive(Debug, Clone)]
pub struct Container {
    pub database: String,
    pub schema: Option<String>,
}

impl Container {
    fn table(&self, name: &str) -> ObjectRef {
        ObjectRef::new(NodeKind::Table, self.database.clone(), self.schema.clone(), name)
    }

    fn holds(&self, o: &ObjectRef) -> bool {
        o.database == self.database && o.schema == self.schema
    }
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_lowercase()
}

fn column_def(c: &ColumnInfo) -> ColumnDef {
    ColumnDef {
        name: c.name.clone(),
        data_type: c.data_type.clone(),
        nullable: c.nullable,
        default: if c.identity { None } else { c.default.clone() },
        identity: c.identity,
    }
}

fn primary_key(t: &TableStructure) -> Vec<String> {
    t.indexes.iter().find(|i| i.primary).map(|i| i.columns.clone()).unwrap_or_default()
}

/// Indexes that are not the primary key and not backing a constraint.
fn plain_indexes(t: &TableStructure) -> impl Iterator<Item = &crate::schema::IndexInfo> {
    t.indexes.iter().filter(|i| !i.primary && !t.constraints.iter().any(|c| c.name == i.name))
}

fn index_columns(i: &crate::schema::IndexInfo) -> Vec<IndexColumn> {
    i.columns.iter().map(|c| IndexColumn { name: c.clone(), descending: false }).collect()
}

/// A comparable description of a constraint, plus the change that creates it.
fn constraint_change(
    t: &TableStructure,
    name: &str,
    table: &ObjectRef,
    source: &Container,
    target: &Container,
) -> Option<(String, SchemaChange)> {
    let c = t.constraints.iter().find(|c| c.name == name)?;
    match c.kind {
        ConstraintKind::ForeignKey => {
            let fk = t.foreign_keys.iter().find(|f| f.name == name)?;
            // References inside the compared schema point to the same table on the target side.
            let fk_target = if source.holds(&fk.target) { target.table(&fk.target.name) } else { fk.target.clone() };
            let key = format!(
                "fk ({}) -> {} ({}) {:?} {:?}",
                fk.columns.join(","),
                fk.target.name,
                fk.target_columns.join(","),
                fk.on_delete,
                fk.on_update
            );
            Some((
                key,
                SchemaChange::AddForeignKey {
                    table: table.clone(),
                    name: name.to_owned(),
                    columns: fk.columns.clone(),
                    target: fk_target,
                    target_columns: fk.target_columns.clone(),
                    on_delete: fk.on_delete.clone(),
                    on_update: fk.on_update.clone(),
                },
            ))
        }
        ConstraintKind::Unique => {
            let columns = t.indexes.iter().find(|i| i.name == name).map(|i| i.columns.clone())?;
            Some((
                format!("unique ({})", columns.join(",")),
                SchemaChange::AddUnique { table: table.clone(), name: name.to_owned(), columns },
            ))
        }
        ConstraintKind::Check => {
            let expression = c.definition.trim().trim_start_matches("CHECK").trim().to_owned();
            Some((
                format!("check {}", norm(&expression)),
                SchemaChange::AddCheck { table: table.clone(), name: name.to_owned(), expression },
            ))
        }
        ConstraintKind::PrimaryKey | ConstraintKind::Exclusion => None,
    }
}

struct Plan {
    drops: Vec<SchemaChange>,
    alters: Vec<(SchemaChange, Option<TableStructure>)>,
    creates: Vec<SchemaChange>,
    late: Vec<SchemaChange>,
    /// Ready statements, run with the creates (wide-column tables).
    create_sql: Vec<String>,
}

/// Adds the statements of one change; a change the engine cannot make (SQLite
/// altering a column, …) becomes a warning so the rest of the plan still applies.
fn push_change(
    d: &dyn Dialect,
    change: &SchemaChange,
    structure: Option<&TableStructure>,
    statements: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> DbResult<()> {
    match ddl::statements(d, change, structure) {
        Ok(sql) => statements.extend(sql),
        Err(e) if e.kind == ErrorKind::Unsupported => {
            warnings.push(format!("{}: {} Change it manually.", change.table().name, e.message.trim_end_matches('.')));
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

/// Compares `source` with `target` and plans changes to the target.
pub fn diff(
    d: &dyn Dialect,
    source: &[DiffTable],
    source_container: &Container,
    target: &[DiffTable],
    target_container: &Container,
) -> DbResult<SchemaDiff> {
    let mut out = SchemaDiff::default();
    let mut plan =
        Plan { drops: Vec::new(), alters: Vec::new(), creates: Vec::new(), late: Vec::new(), create_sql: Vec::new() };
    let item = |out: &mut SchemaDiff, table: &str, kind: &str, name: &str, change, detail: String| {
        out.items.push(DiffItem { table: table.into(), kind: kind.into(), name: name.into(), change, detail });
    };
    let tables = |list: &[DiffTable]| -> Vec<String> {
        list.iter().map(|t| &t.structure.object).filter(|o| o.kind == NodeKind::Table).map(|o| o.name.clone()).collect()
    };
    let (source_names, target_names) = (tables(source), tables(target));
    let find = |list: &'_ [DiffTable], name: &str| {
        list.iter().find(|t| t.structure.object.name == name && t.structure.object.kind == NodeKind::Table).cloned()
    };

    for name in &source_names {
        let source_table = find(source, name).ok_or_else(|| DbError::internal("source table vanished"))?;
        let s = &source_table.structure;
        let table = target_container.table(name);
        let Some(target_table) = find(target, name) else {
            item(&mut out, name, "table", name, DiffChange::Added, format!("{} columns", s.columns.len()));
            let columns = s.columns.iter().map(column_def).collect::<Vec<_>>();
            if source_table.keys.is_empty() {
                plan.creates.push(SchemaChange::CreateTable {
                    table: table.clone(),
                    columns,
                    primary_key: primary_key(s),
                });
            } else {
                // Static columns are marked by the CQL catalog's column comment.
                let statics: Vec<String> = s
                    .columns
                    .iter()
                    .filter(|c| c.comment.as_deref() == Some("static"))
                    .map(|c| c.name.clone())
                    .collect();
                plan.create_sql.push(ddl::cql_create_table(d, &table, &columns, &statics, &source_table.keys)?);
            }
            for c in &s.constraints {
                if let Some((_, change)) = constraint_change(s, &c.name, &table, source_container, target_container) {
                    plan.late.push(change);
                }
            }
            for i in plain_indexes(s) {
                if i.columns.is_empty() {
                    out.warnings.push(format!("{name}: expression index {} must be created manually.", i.name));
                } else {
                    plan.late.push(SchemaChange::CreateIndex {
                        table: table.clone(),
                        name: i.name.clone(),
                        columns: index_columns(i),
                        unique: i.unique,
                    });
                }
            }
            continue;
        };
        let t = &target_table.structure;
        let (source_key, target_key) = (source_table.key_layout(), target_table.key_layout());
        if source_key != target_key {
            if !target_table.keys.is_empty() {
                // Columns of a key cannot be added or dropped on their own: compare nothing else.
                out.warnings.push(format!(
                    "{name}: the primary key differs ({source_key} vs {target_key}). \
                     CQL cannot change a primary key: create a new table and copy the rows."
                ));
                continue;
            }
            out.warnings
                .push(format!("{name}: the primary key differs ({source_key} vs {target_key}); change it manually."));
        }
        // Columns
        for sc in &s.columns {
            match t.column(&sc.name) {
                None => {
                    item(&mut out, name, "column", &sc.name, DiffChange::Added, sc.data_type.clone());
                    plan.creates.push(SchemaChange::AddColumn { table: table.clone(), column: column_def(sc) });
                }
                Some(tc) => {
                    let type_changed = norm(&sc.data_type) != norm(&tc.data_type);
                    let null_changed = sc.nullable != tc.nullable;
                    let default_changed =
                        !sc.identity && sc.default.as_deref().map(norm) != tc.default.as_deref().map(norm);
                    if type_changed || null_changed || default_changed {
                        let mut details = Vec::new();
                        if type_changed {
                            details.push(format!("type {} → {}", tc.data_type, sc.data_type));
                        }
                        if null_changed {
                            details.push(if sc.nullable { "nullable".into() } else { "not null".to_owned() });
                        }
                        if default_changed {
                            details.push(format!(
                                "default {} → {}",
                                tc.default.as_deref().unwrap_or("none"),
                                sc.default.as_deref().unwrap_or("none")
                            ));
                        }
                        item(&mut out, name, "column", &sc.name, DiffChange::Changed, details.join(", "));
                        plan.alters.push((
                            SchemaChange::AlterColumn {
                                table: table.clone(),
                                column: sc.name.clone(),
                                data_type: type_changed.then(|| sc.data_type.clone()),
                                nullable: null_changed.then_some(sc.nullable),
                                default: default_changed.then(|| match &sc.default {
                                    Some(e) => DefaultChange::Set { expression: e.clone() },
                                    None => DefaultChange::Drop,
                                }),
                            },
                            Some(t.clone()),
                        ));
                    }
                }
            }
        }
        for tc in t.columns.iter().filter(|tc| s.column(&tc.name).is_none()) {
            item(&mut out, name, "column", &tc.name, DiffChange::Removed, tc.data_type.clone());
            plan.drops.push(SchemaChange::DropColumn { table: table.clone(), column: tc.name.clone() });
        }
        // Constraints (FK, unique, check), matched by name.
        let source_keys: Vec<(String, String, SchemaChange)> = s
            .constraints
            .iter()
            .filter_map(|c| {
                constraint_change(s, &c.name, &table, source_container, target_container)
                    .map(|(k, ch)| (c.name.clone(), k, ch))
            })
            .collect();
        let target_keys: Vec<(String, String)> = t
            .constraints
            .iter()
            .filter_map(|c| {
                constraint_change(t, &c.name, &table, target_container, target_container)
                    .map(|(k, _)| (c.name.clone(), k))
            })
            .collect();
        for (cname, key, change) in &source_keys {
            match target_keys.iter().find(|(n, _)| n == cname) {
                None => {
                    item(&mut out, name, "constraint", cname, DiffChange::Added, key.clone());
                    plan.late.push(change.clone());
                }
                Some((_, tkey)) if tkey != key => {
                    item(&mut out, name, "constraint", cname, DiffChange::Changed, key.clone());
                    let kind =
                        s.constraints.iter().find(|c| &c.name == cname).map_or(ConstraintKind::Check, |c| c.kind);
                    plan.drops.push(SchemaChange::DropConstraint { table: table.clone(), name: cname.clone(), kind });
                    plan.late.push(change.clone());
                }
                Some(_) => {}
            }
        }
        for (cname, key) in target_keys.iter().filter(|(n, _)| !source_keys.iter().any(|(sn, _, _)| sn == n)) {
            item(&mut out, name, "constraint", cname, DiffChange::Removed, key.clone());
            let kind = t.constraints.iter().find(|c| &c.name == cname).map_or(ConstraintKind::Check, |c| c.kind);
            plan.drops.insert(0, SchemaChange::DropConstraint { table: table.clone(), name: cname.clone(), kind });
        }
        // Plain indexes, matched by name.
        for si in plain_indexes(s) {
            let ti = plain_indexes(t).find(|i| i.name == si.name);
            let same = ti.is_some_and(|ti| ti.unique == si.unique && ti.columns == si.columns);
            if same {
                continue;
            }
            if si.columns.is_empty() {
                out.warnings.push(format!("{name}: expression index {} must be compared manually.", si.name));
                continue;
            }
            let change = if ti.is_some() { DiffChange::Changed } else { DiffChange::Added };
            item(&mut out, name, "index", &si.name, change, format!("({})", si.columns.join(", ")));
            if ti.is_some() {
                plan.drops.insert(0, SchemaChange::DropIndex { table: table.clone(), name: si.name.clone() });
            }
            plan.late.push(SchemaChange::CreateIndex {
                table: table.clone(),
                name: si.name.clone(),
                columns: index_columns(si),
                unique: si.unique,
            });
        }
        for ti in plain_indexes(t).filter(|ti| !plain_indexes(s).any(|si| si.name == ti.name)) {
            item(&mut out, name, "index", &ti.name, DiffChange::Removed, format!("({})", ti.columns.join(", ")));
            plan.drops.insert(0, SchemaChange::DropIndex { table: table.clone(), name: ti.name.clone() });
        }
    }
    for name in target_names.iter().filter(|n| !source_names.contains(n)) {
        item(&mut out, name, "table", name, DiffChange::Removed, String::new());
        plan.drops.push(SchemaChange::DropTable { table: target_container.table(name), cascade: false });
    }

    // A change the engine cannot make (SQLite altering a column, …) becomes a
    // warning; the rest of the plan still applies.
    let mut statements = Vec::new();
    let early = plan
        .drops
        .iter()
        .chain(&plan.creates)
        .map(|c| (c, None))
        .chain(plan.alters.iter().map(|(c, s)| (c, s.as_ref())));
    for (change, structure) in early {
        push_change(d, change, structure, &mut statements, &mut out.warnings)?;
    }
    // Ready-made creates go before the indexes and constraints that need their tables.
    statements.append(&mut plan.create_sql);
    for change in &plan.late {
        push_change(d, change, None, &mut statements, &mut out.warnings)?;
    }
    out.sql = if statements.is_empty() { String::new() } else { ddl::preview(&statements) };
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ConstraintInfo, IndexInfo};
    use crate::sql::PostgresDialect;

    fn col(name: &str, ty: &str, nullable: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: ty.into(),
            nullable,
            default: None,
            primary_key: name == "id",
            generated: false,
            identity: false,
            editable: true,
            numeric: false,
            comment: None,
            read_as_text: false,
        }
    }

    fn table(schema: &str, name: &str, columns: Vec<ColumnInfo>) -> TableStructure {
        TableStructure {
            object: ObjectRef::new(NodeKind::Table, "app", Some(schema.into()), name),
            columns,
            constraints: vec![ConstraintInfo {
                kind: ConstraintKind::PrimaryKey,
                name: format!("{name}_pkey"),
                definition: "PRIMARY KEY (id)".into(),
            }],
            indexes: vec![IndexInfo {
                name: format!("{name}_pkey"),
                definition: String::new(),
                unique: true,
                primary: true,
                columns: vec!["id".into()],
                size_bytes: None,
            }],
            row_identity: vec!["id".into()],
            full_row_key: vec![],
            comment: None,
            row_estimate: None,
            foreign_keys: vec![],
            size: None,
        }
    }

    fn tables(list: Vec<TableStructure>) -> Vec<DiffTable> {
        list.into_iter().map(DiffTable::from).collect()
    }

    /// A CQL table keyed by `keys` (partition first), plus its secondary indexes.
    fn cql_table(keyspace: &str, name: &str, columns: &[(&str, &str)], keys: Vec<KeyRole>) -> DiffTable {
        let is_key = |c: &str| keys.iter().any(|k| k.column == c);
        let columns = columns.iter().map(|(n, ty)| col(n, ty, !is_key(n))).collect();
        let mut structure = table(keyspace, name, columns);
        structure.object.schema = None;
        structure.object.database = keyspace.into();
        structure.indexes.clear();
        structure.constraints.clear();
        DiffTable { structure, keys }
    }

    #[test]
    fn plans_cql_tables_with_their_keys() {
        use crate::sql::dialect::CqlDialect;
        let key = |column: &str, role, descending| KeyRole { column: column.into(), role, descending };
        let src_c = Container { database: "prod".into(), schema: None };
        let dst_c = Container { database: "dev".into(), schema: None };
        let readings_keys = || {
            vec![
                key("sensor", KeyRoleKind::Partition, false),
                key("day", KeyRoleKind::Partition, false),
                key("at", KeyRoleKind::Clustering, true),
            ]
        };
        let mut readings = cql_table(
            "prod",
            "readings",
            &[("sensor", "uuid"), ("day", "date"), ("at", "timestamp"), ("value", "double"), ("unit", "text")],
            readings_keys(),
        );
        readings.structure.columns[4].comment = Some("static".into());
        let mut users = cql_table(
            "prod",
            "users",
            &[("id", "uuid"), ("email", "text"), ("age", "int")],
            vec![key("id", KeyRoleKind::Partition, false)],
        );
        users.structure.indexes.push(IndexInfo {
            name: "users_email_idx".into(),
            definition: String::new(),
            unique: false,
            primary: false,
            columns: vec!["email".into()],
            size_bytes: None,
        });
        let events = cql_table(
            "prod",
            "events",
            &[("id", "uuid"), ("at", "timestamp")],
            vec![key("id", KeyRoleKind::Partition, false)],
        );
        let source = vec![readings, users, events];
        let target = vec![
            cql_table(
                "dev",
                "users",
                &[("id", "uuid"), ("email", "text"), ("age", "bigint"), ("legacy", "text")],
                vec![key("id", KeyRoleKind::Partition, false)],
            ),
            cql_table(
                "dev",
                "events",
                &[("id", "uuid"), ("at", "timestamp")],
                vec![key("id", KeyRoleKind::Partition, false), key("at", KeyRoleKind::Clustering, false)],
            ),
        ];
        let diff = diff(&CqlDialect, &source, &src_c, &target, &dst_c).unwrap();
        assert!(diff.sql.contains(
            "CREATE TABLE \"dev\".\"readings\" (\n  \"sensor\" uuid,\n  \"day\" date,\n  \"at\" timestamp,\n  \"value\" double,\n  \"unit\" text STATIC,\n  PRIMARY KEY ((\"sensor\", \"day\"), \"at\")\n) WITH CLUSTERING ORDER BY (\"at\" DESC)"
        ), "{}", diff.sql);
        assert!(diff.sql.contains(r#"ALTER TABLE "dev"."users" DROP "legacy""#), "{}", diff.sql);
        assert!(diff.sql.contains(r#"CREATE INDEX "users_email_idx" ON "dev"."users" ("email")"#), "{}", diff.sql);
        // A type change and a different key cannot be applied in CQL: warnings, no SQL.
        assert!(diff.warnings.iter().any(|w| w.starts_with("users:") && w.contains("type")), "{:?}", diff.warnings);
        assert!(
            diff.warnings.iter().any(|w| w.starts_with("events: the primary key differs ((id) vs (id), at)")),
            "{:?}",
            diff.warnings
        );
        assert!(!diff.sql.contains("events"), "{}", diff.sql);
        let same = super::diff(&CqlDialect, &target, &dst_c, &target, &dst_c).unwrap();
        assert!(same.items.is_empty() && same.sql.is_empty() && same.warnings.is_empty());
    }

    #[test]
    fn plans_creates_alters_and_drops() {
        let src_c = Container { database: "app".into(), schema: Some("public".into()) };
        let dst_c = Container { database: "app".into(), schema: Some("staging".into()) };
        let mut orders = table(
            "public",
            "orders",
            vec![col("id", "bigint", false), col("total", "numeric(14,2)", false), col("note", "text", true)],
        );
        orders.indexes.push(IndexInfo {
            name: "orders_total_idx".into(),
            definition: String::new(),
            unique: false,
            primary: false,
            columns: vec!["total".into()],
            size_bytes: None,
        });
        let source = tables(vec![orders, table("public", "customers", vec![col("id", "bigint", false)])]);
        let target = tables(vec![
            table(
                "staging",
                "orders",
                vec![col("id", "bigint", false), col("total", "numeric(12,2)", true), col("legacy", "text", true)],
            ),
            table("staging", "old_stuff", vec![col("id", "int", false)]),
        ]);
        let diff = diff(&PostgresDialect, &source, &src_c, &target, &dst_c).unwrap();
        let kinds: Vec<_> = diff.items.iter().map(|i| (i.kind.as_str(), i.name.as_str(), i.change)).collect();
        assert!(kinds.contains(&("column", "total", DiffChange::Changed)));
        assert!(kinds.contains(&("column", "note", DiffChange::Added)));
        assert!(kinds.contains(&("column", "legacy", DiffChange::Removed)));
        assert!(kinds.contains(&("index", "orders_total_idx", DiffChange::Added)));
        assert!(kinds.contains(&("table", "customers", DiffChange::Added)));
        assert!(kinds.contains(&("table", "old_stuff", DiffChange::Removed)));
        assert!(
            diff.sql.contains(r#"ALTER TABLE "staging"."orders" ALTER COLUMN "total" TYPE numeric(14,2)"#),
            "{}",
            diff.sql
        );
        assert!(diff.sql.contains(r#"CREATE TABLE "staging"."customers""#));
        assert!(diff.sql.contains(r#"DROP TABLE "staging"."old_stuff""#));
        // Drops come before creates; indexes after columns exist.
        assert!(diff.sql.find("DROP COLUMN").unwrap() < diff.sql.find("ADD COLUMN").unwrap());
        assert!(diff.sql.find("ADD COLUMN \"note\"").unwrap() < diff.sql.find("CREATE INDEX").unwrap());
        let same = super::diff(&PostgresDialect, &target, &dst_c, &target, &dst_c).unwrap();
        assert!(same.items.is_empty() && same.sql.is_empty());
    }
}
