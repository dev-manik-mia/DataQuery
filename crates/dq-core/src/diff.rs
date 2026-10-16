//! Schema diff between two schemas (or MySQL databases), possibly on
//! different connections, and the migration SQL that makes the target match
//! the source. The SQL is shown for review and only runs when the user runs it.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::error::{DbError, DbResult, ErrorKind};
use crate::node::{NodeKind, ObjectRef};
use crate::schema::{ColumnInfo, ConstraintKind, TableStructure};
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
}

/// Compares `source` with `target` and plans changes to the target.
pub fn diff(
    d: &dyn Dialect,
    source: &[TableStructure],
    source_container: &Container,
    target: &[TableStructure],
    target_container: &Container,
) -> DbResult<SchemaDiff> {
    let mut out = SchemaDiff::default();
    let mut plan = Plan { drops: Vec::new(), alters: Vec::new(), creates: Vec::new(), late: Vec::new() };
    let item = |out: &mut SchemaDiff, table: &str, kind: &str, name: &str, change, detail: String| {
        out.items.push(DiffItem { table: table.into(), kind: kind.into(), name: name.into(), change, detail });
    };
    let tables = |list: &[TableStructure]| -> Vec<String> {
        list.iter().filter(|t| t.object.kind == NodeKind::Table).map(|t| t.object.name.clone()).collect()
    };
    let (source_names, target_names) = (tables(source), tables(target));
    let find = |list: &'_ [TableStructure], name: &str| {
        list.iter().find(|t| t.object.name == name && t.object.kind == NodeKind::Table).cloned()
    };

    for name in &source_names {
        let s = find(source, name).ok_or_else(|| DbError::internal("source table vanished"))?;
        let table = target_container.table(name);
        let Some(t) = find(target, name) else {
            item(&mut out, name, "table", name, DiffChange::Added, format!("{} columns", s.columns.len()));
            plan.creates.push(SchemaChange::CreateTable {
                table: table.clone(),
                columns: s.columns.iter().map(column_def).collect(),
                primary_key: primary_key(&s),
            });
            for c in &s.constraints {
                if let Some((_, change)) = constraint_change(&s, &c.name, &table, source_container, target_container) {
                    plan.late.push(change);
                }
            }
            for i in plain_indexes(&s) {
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
                constraint_change(&s, &c.name, &table, source_container, target_container)
                    .map(|(k, ch)| (c.name.clone(), k, ch))
            })
            .collect();
        let target_keys: Vec<(String, String)> = t
            .constraints
            .iter()
            .filter_map(|c| {
                constraint_change(&t, &c.name, &table, target_container, target_container)
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
        for si in plain_indexes(&s) {
            let ti = plain_indexes(&t).find(|i| i.name == si.name);
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
        for ti in plain_indexes(&t).filter(|ti| !plain_indexes(&s).any(|si| si.name == ti.name)) {
            item(&mut out, name, "index", &ti.name, DiffChange::Removed, format!("({})", ti.columns.join(", ")));
            plan.drops.insert(0, SchemaChange::DropIndex { table: table.clone(), name: ti.name.clone() });
        }
        if primary_key(&s) != primary_key(&t) {
            out.warnings.push(format!(
                "{name}: the primary key differs ({:?} vs {:?}); change it manually.",
                primary_key(&s),
                primary_key(&t)
            ));
        }
    }
    for name in target_names.iter().filter(|n| !source_names.contains(n)) {
        item(&mut out, name, "table", name, DiffChange::Removed, String::new());
        plan.drops.push(SchemaChange::DropTable { table: target_container.table(name), cascade: false });
    }

    // A change the engine cannot make (SQLite altering a column, …) becomes a
    // warning; the rest of the plan still applies.
    let mut statements = Vec::new();
    let changes = plan
        .drops
        .iter()
        .chain(&plan.creates)
        .map(|c| (c, None))
        .chain(plan.alters.iter().map(|(c, s)| (c, s.as_ref())))
        .chain(plan.late.iter().map(|c| (c, None)));
    for (change, structure) in changes {
        match ddl::statements(d, change, structure) {
            Ok(sql) => statements.extend(sql),
            Err(e) if e.kind == ErrorKind::Unsupported => {
                out.warnings.push(format!(
                    "{}: {} Change it manually.",
                    change.table().name,
                    e.message.trim_end_matches('.')
                ));
            }
            Err(e) => return Err(e),
        }
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
        let source = vec![orders, table("public", "customers", vec![col("id", "bigint", false)])];
        let target = vec![
            table(
                "staging",
                "orders",
                vec![col("id", "bigint", false), col("total", "numeric(12,2)", true), col("legacy", "text", true)],
            ),
            table("staging", "old_stuff", vec![col("id", "int", false)]),
        ];
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
