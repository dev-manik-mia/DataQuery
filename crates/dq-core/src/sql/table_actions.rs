//! Whole-table actions from the explorer: truncate, empty (delete every row)
//! and duplicate. Each becomes reviewed SQL through [`super::ddl::statements`];
//! [`TableActions`] says per engine what is possible and what the UI offers.

use serde::Serialize;
use ts_rs::TS;

use super::analyze::split_statements;
use super::ddl::SchemaChange;
use super::dialect::{Dialect, SqlFlavor};
use super::lexer::{TokenKind, tokenize};
use crate::engine::Engine;
use crate::error::{DbError, DbResult};
use crate::node::{NodeKind, ObjectRef};
use crate::schema::TableStructure;

/// Whether an action is offered, and a note: the reason when it is not, a
/// caveat when it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
pub struct ActionSupport {
    pub available: bool,
    pub note: Option<String>,
}

impl ActionSupport {
    fn yes(note: Option<&str>) -> Self {
        Self { available: true, note: note.map(str::to_owned) }
    }

    fn no(reason: &str) -> Self {
        Self { available: false, note: Some(reason.to_owned()) }
    }
}

/// What the table actions can do on one engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[ts(export)]
pub struct TableActions {
    pub truncate: ActionSupport,
    /// `TRUNCATE … RESTART IDENTITY` (`PostgreSQL`); `SQLite` resets `sqlite_sequence`.
    pub truncate_restart_identity: bool,
    /// `TRUNCATE … CASCADE` (`PostgreSQL`, `CockroachDB`, Oracle).
    pub truncate_cascade: bool,
    /// Foreign key checks off around `TRUNCATE` (`MySQL`, `MariaDB`).
    pub truncate_disable_foreign_keys: bool,
    pub empty: ActionSupport,
    pub duplicate: ActionSupport,
}

const DOCUMENTS: &str = "Document collections have no SQL table actions.";
const CQL_EMPTY: &str = "Cassandra deletes rows by partition key only; use Truncate to remove every row.";
const CQL_DUPLICATE: &str = "CQL cannot copy rows from one table into another.";

/// The table actions of an engine. `engine` refines the flavor where engines
/// sharing it differ (`CockroachDB` has no `RESTART IDENTITY`).
pub fn table_actions(flavor: SqlFlavor, engine: Option<Engine>) -> TableActions {
    if flavor.is_document() {
        return TableActions {
            truncate: ActionSupport::no(DOCUMENTS),
            truncate_restart_identity: false,
            truncate_cascade: false,
            truncate_disable_foreign_keys: false,
            empty: ActionSupport::no(DOCUMENTS),
            duplicate: ActionSupport::no(DOCUMENTS),
        };
    }
    let cockroach = engine == Some(Engine::Cockroachdb);
    let truncate_note = match flavor {
        SqlFlavor::Mysql => Some(
            "TRUNCATE commits at once, resets AUTO_INCREMENT and fails while other tables' foreign keys reference the table, unless foreign key checks are turned off.",
        ),
        SqlFlavor::Sqlserver => {
            Some("TRUNCATE resets IDENTITY and is refused while a foreign key references the table; use Empty instead.")
        }
        SqlFlavor::Sqlite => Some("SQLite has no TRUNCATE: every row is deleted with DELETE."),
        SqlFlavor::Cql => Some("Every node of the cluster must be up."),
        SqlFlavor::Vertica => Some("TRUNCATE commits the open transaction at once."),
        SqlFlavor::Clickhouse | SqlFlavor::Snowflake | SqlFlavor::Duckdb | SqlFlavor::Oracle | SqlFlavor::Postgres => {
            None
        }
        SqlFlavor::Mongodb | SqlFlavor::Partiql | SqlFlavor::Redis | SqlFlavor::QueryDsl | SqlFlavor::Cypher => None,
    };
    let empty = match flavor {
        SqlFlavor::Cql => ActionSupport::no(CQL_EMPTY),
        SqlFlavor::Clickhouse => ActionSupport::yes(Some("Runs a lightweight DELETE (MergeTree tables).")),
        _ => ActionSupport::yes(None),
    };
    let duplicate = match flavor {
        SqlFlavor::Cql => ActionSupport::no(CQL_DUPLICATE),
        SqlFlavor::Postgres => ActionSupport::yes(Some(
            "Copies columns, defaults, constraints, indexes and comments (LIKE … INCLUDING ALL). Foreign keys and triggers are not copied; serial columns keep using the original sequence.",
        )),
        SqlFlavor::Mysql => ActionSupport::yes(Some(
            "Copies columns and indexes (CREATE TABLE … LIKE). Foreign keys and triggers are not copied.",
        )),
        SqlFlavor::Sqlserver => ActionSupport::yes(Some(
            "SELECT … INTO copies columns and IDENTITY, then the primary key is added. Other indexes, defaults, foreign keys, checks and triggers are not copied.",
        )),
        SqlFlavor::Oracle => ActionSupport::yes(Some(
            "CREATE TABLE … AS SELECT copies columns and NOT NULL, then the primary key is added. Other indexes, defaults, constraints and identity are not copied.",
        )),
        SqlFlavor::Clickhouse => {
            ActionSupport::yes(Some("Copies the columns and the table engine with its keys (CREATE TABLE … AS)."))
        }
        SqlFlavor::Snowflake => {
            ActionSupport::yes(Some("With rows: a zero-copy CLONE. Structure only: CREATE TABLE … LIKE."))
        }
        SqlFlavor::Vertica => ActionSupport::yes(Some(
            "Copies columns, defaults, constraints and projections (CREATE TABLE … LIKE … INCLUDING PROJECTIONS).",
        )),
        SqlFlavor::Sqlite | SqlFlavor::Duckdb => ActionSupport::yes(Some(
            "Copies the table definition (columns, keys, constraints, defaults). Indexes and triggers are not copied.",
        )),
        SqlFlavor::Mongodb | SqlFlavor::Partiql | SqlFlavor::Redis | SqlFlavor::QueryDsl | SqlFlavor::Cypher => {
            ActionSupport::no(DOCUMENTS)
        }
    };
    TableActions {
        truncate: ActionSupport::yes(truncate_note),
        truncate_restart_identity: (flavor == SqlFlavor::Postgres && !cockroach) || flavor == SqlFlavor::Sqlite,
        truncate_cascade: matches!(flavor, SqlFlavor::Postgres | SqlFlavor::Oracle),
        truncate_disable_foreign_keys: flavor == SqlFlavor::Mysql,
        empty,
        duplicate,
    }
}

impl ActionSupport {
    fn check(&self) -> DbResult<()> {
        if self.available { Ok(()) } else { Err(DbError::unsupported(self.note.clone().unwrap_or_default())) }
    }
}

impl TableActions {
    /// Refuses a table action, or one of its options, that the engine does not
    /// offer, with the reason. Other changes pass.
    pub fn check(&self, change: &SchemaChange) -> DbResult<()> {
        let option = |on: bool, offered: bool, what: &str| {
            if on && !offered {
                Err(DbError::unsupported(format!("This engine has no {what} for TRUNCATE.")))
            } else {
                Ok(())
            }
        };
        match change {
            SchemaChange::TruncateTable { restart_identity, cascade, disable_foreign_keys, .. } => {
                self.truncate.check()?;
                option(*restart_identity, self.truncate_restart_identity, "RESTART IDENTITY")?;
                option(*cascade, self.truncate_cascade, "CASCADE")?;
                option(*disable_foreign_keys, self.truncate_disable_foreign_keys, "foreign key switch")
            }
            SchemaChange::EmptyTable { .. } => self.empty.check(),
            SchemaChange::DuplicateTable { .. } => self.duplicate.check(),
            _ => Ok(()),
        }
    }
}

fn only_tables(table: &ObjectRef) -> DbResult<()> {
    if table.kind == NodeKind::Table {
        Ok(())
    } else {
        Err(DbError::invalid(format!("{} is not a table.", table.name)))
    }
}

/// Options of [`truncate`].
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct TruncateOptions {
    pub restart_identity: bool,
    pub cascade: bool,
    pub disable_foreign_keys: bool,
}

/// `TRUNCATE` in the engine's spelling, for options [`TableActions::check`] allowed. `SQLite` deletes the rows and, for
/// `AUTOINCREMENT` tables (seen in `definition`, the `CREATE TABLE` text),
/// its `sqlite_sequence` entry.
pub(super) fn truncate(
    d: &dyn Dialect,
    table: &ObjectRef,
    options: TruncateOptions,
    definition: Option<&str>,
) -> DbResult<Vec<String>> {
    let flavor = d.flavor();
    only_tables(table)?;
    let name = d.quoted_name(table);
    Ok(match flavor {
        SqlFlavor::Sqlite => {
            let mut out = vec![format!("DELETE FROM {name}")];
            if options.restart_identity {
                let definition = definition
                    .ok_or_else(|| DbError::internal("Resetting the row ids needs the table's definition."))?;
                if has_word(definition, flavor, "AUTOINCREMENT") {
                    let sequence = ObjectRef { name: "sqlite_sequence".into(), ..table.clone() };
                    out.push(format!(
                        "DELETE FROM {} WHERE name = {}",
                        d.quoted_name(&sequence),
                        d.quote_literal(&table.name)
                    ));
                }
            }
            out
        }
        SqlFlavor::Duckdb | SqlFlavor::Cql => vec![format!("TRUNCATE {name}")],
        SqlFlavor::Mysql if options.disable_foreign_keys => vec![
            "SET FOREIGN_KEY_CHECKS = 0".into(),
            format!("TRUNCATE TABLE {name}"),
            "SET FOREIGN_KEY_CHECKS = 1".into(),
        ],
        _ => {
            let mut sql = format!("TRUNCATE TABLE {name}");
            if options.restart_identity {
                sql.push_str(" RESTART IDENTITY");
            }
            if options.cascade {
                sql.push_str(" CASCADE");
            }
            vec![sql]
        }
    })
}

/// `DELETE` of every row: fires triggers and works where `TRUNCATE` is blocked.
pub(super) fn empty(d: &dyn Dialect, table: &ObjectRef) -> DbResult<Vec<String>> {
    only_tables(table)?;
    let name = d.quoted_name(table);
    // ClickHouse's lightweight DELETE needs a WHERE clause.
    Ok(vec![if d.flavor() == SqlFlavor::Clickhouse {
        format!("DELETE FROM {name} WHERE 1")
    } else {
        format!("DELETE FROM {name}")
    }])
}

/// A copy of `table` named `new_name` in the same schema, with or without its
/// rows. `structure` is the table's; `definition` its `CREATE TABLE` text
/// (needed by `SQLite` and `DuckDB`, see [`needs_definition`]).
pub(super) fn duplicate(
    d: &dyn Dialect,
    table: &ObjectRef,
    new_name: &str,
    with_data: bool,
    structure: Option<&TableStructure>,
    definition: Option<&str>,
) -> DbResult<Vec<String>> {
    let flavor = d.flavor();
    only_tables(table)?;
    if new_name.trim().is_empty() || new_name.len() > 128 || new_name.chars().any(char::is_control) {
        return Err(DbError::invalid("The new name must be 1–128 characters."));
    }
    if new_name == table.name {
        return Err(DbError::invalid("Choose a name other than the original's."));
    }
    let target = ObjectRef { name: new_name.to_owned(), kind: NodeKind::Table, parent: None, ..table.clone() };
    let (source, copy) = (d.quoted_name(table), d.quoted_name(&target));
    let structure = || structure.ok_or_else(|| DbError::internal("Duplicating a table needs its structure."));
    // Columns a plain INSERT may fill: identity yes, computed no.
    let insert = |s: &TableStructure, overriding: &str| {
        let columns = s
            .columns
            .iter()
            .filter(|c| !c.generated || c.identity)
            .map(|c| d.quote_ident(&c.name))
            .collect::<Vec<_>>()
            .join(", ");
        format!("INSERT INTO {copy} ({columns}){overriding} SELECT {columns} FROM {source}")
    };
    let primary_key = |s: &TableStructure| {
        let from_index = s.indexes.iter().find(|i| i.primary).map(|i| i.columns.clone()).unwrap_or_default();
        let columns = if from_index.is_empty() {
            s.columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect()
        } else {
            from_index
        };
        (!columns.is_empty()).then(|| {
            let list = columns.iter().map(|c| d.quote_ident(c)).collect::<Vec<_>>().join(", ");
            format!("ALTER TABLE {copy} ADD PRIMARY KEY ({list})")
        })
    };
    let no_rows = if with_data { "" } else { " WHERE 1 = 0" };
    Ok(match flavor {
        SqlFlavor::Postgres => {
            let s = structure()?;
            let mut out = vec![format!("CREATE TABLE {copy} (LIKE {source} INCLUDING ALL)")];
            if with_data {
                let identity: Vec<_> = s.columns.iter().filter(|c| c.identity).collect();
                out.push(insert(s, if identity.is_empty() { "" } else { " OVERRIDING SYSTEM VALUE" }));
                // The copy's identity sequences are new: continue after the copied rows.
                for c in identity {
                    let column = d.quote_ident(&c.name);
                    out.push(format!(
                        "SELECT setval(pg_get_serial_sequence({}, {}), MAX({column})) FROM {copy} HAVING MAX({column}) IS NOT NULL",
                        d.quote_literal(&copy),
                        d.quote_literal(&c.name),
                    ));
                }
            }
            out
        }
        SqlFlavor::Mysql => {
            let mut out = vec![format!("CREATE TABLE {copy} LIKE {source}")];
            if with_data {
                out.push(insert(structure()?, ""));
            }
            out
        }
        SqlFlavor::Sqlserver => {
            let mut out = vec![format!("SELECT * INTO {copy} FROM {source}{no_rows}")];
            out.extend(primary_key(structure()?));
            out
        }
        SqlFlavor::Oracle => {
            let mut out = vec![format!("CREATE TABLE {copy} AS SELECT * FROM {source}{no_rows}")];
            out.extend(primary_key(structure()?));
            out
        }
        SqlFlavor::Clickhouse => {
            let mut out = vec![format!("CREATE TABLE {copy} AS {source}")];
            if with_data {
                out.push(format!("INSERT INTO {copy} SELECT * FROM {source}"));
            }
            out
        }
        SqlFlavor::Snowflake => {
            vec![format!("CREATE TABLE {copy} {} {source}", if with_data { "CLONE" } else { "LIKE" })]
        }
        SqlFlavor::Vertica => {
            let mut out = vec![format!("CREATE TABLE {copy} LIKE {source} INCLUDING PROJECTIONS")];
            if with_data {
                out.push(format!("INSERT INTO {copy} SELECT * FROM {source}"));
            }
            out
        }
        SqlFlavor::Sqlite | SqlFlavor::Duckdb => {
            let definition =
                definition.ok_or_else(|| DbError::internal("Duplicating a table needs its definition."))?;
            let mut out = vec![rename_created_table(definition, flavor, &copy)?];
            if with_data {
                out.push(insert(structure()?, ""));
            }
            out
        }
        // Refused by `TableActions::check`.
        SqlFlavor::Cql
        | SqlFlavor::Mongodb
        | SqlFlavor::Partiql
        | SqlFlavor::Redis
        | SqlFlavor::QueryDsl
        | SqlFlavor::Cypher => return Err(DbError::unsupported(CQL_DUPLICATE)),
    })
}

/// `SQLite` and `DuckDB` duplicate from the catalog's `CREATE TABLE` text.
pub fn needs_definition(flavor: SqlFlavor) -> bool {
    matches!(flavor, SqlFlavor::Sqlite | SqlFlavor::Duckdb)
}

fn has_word(sql: &str, flavor: SqlFlavor, word: &str) -> bool {
    tokenize(sql, flavor).iter().any(|t| t.kind == TokenKind::Word && t.text(sql).eq_ignore_ascii_case(word))
}

/// The first statement of `definition`, a `CREATE TABLE`, with the table's
/// name replaced by `quoted_name`.
pub fn rename_created_table(definition: &str, flavor: SqlFlavor, quoted_name: &str) -> DbResult<String> {
    let unexpected = || DbError::invalid("The table's definition is not a CREATE TABLE statement.");
    let first = split_statements(definition, flavor).into_iter().next().ok_or_else(unexpected)?;
    let sql = first.text(definition).trim().trim_end_matches(';').trim_end();
    let tokens: Vec<_> = tokenize(sql, flavor).into_iter().filter(|t| !t.is_trivia()).collect();
    let word = |i: usize, w: &str| {
        tokens.get(i).is_some_and(|t| t.kind == TokenKind::Word && t.text(sql).eq_ignore_ascii_case(w))
    };
    if !word(0, "CREATE") {
        return Err(unexpected());
    }
    // CREATE [TEMP | TEMPORARY | OR REPLACE …] TABLE [IF NOT EXISTS] name
    let mut i = (1..tokens.len().min(6)).find(|&i| word(i, "TABLE")).ok_or_else(unexpected)? + 1;
    if word(i, "IF") && word(i + 1, "NOT") && word(i + 2, "EXISTS") {
        i += 3;
    }
    let is_part = |i: usize| tokens.get(i).is_some_and(|t| matches!(t.kind, TokenKind::Word | TokenKind::QuotedIdent));
    let is_dot = |i: usize| tokens.get(i).is_some_and(|t| t.kind == TokenKind::Punct && t.text(sql) == ".");
    if !is_part(i) {
        return Err(unexpected());
    }
    let start = tokens[i].start;
    while is_dot(i + 1) && is_part(i + 2) {
        i += 2;
    }
    let end = tokens[i].end;
    Ok(format!("{}{quoted_name}{}", &sql[..start], &sql[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ColumnInfo, IndexInfo};
    use crate::sql::ddl::statements_with;
    use crate::sql::dialect::{
        ClickhouseDialect, CqlDialect, DuckdbDialect, MongoDialect, MysqlDialect, OracleDialect, PostgresDialect,
        SnowflakeDialect, SqlServerDialect, SqliteDialect,
    };

    fn column(name: &str, identity: bool, generated: bool, primary_key: bool) -> ColumnInfo {
        ColumnInfo { primary_key, generated, identity, ..ColumnInfo::new_field(name) }
    }

    fn structure(object: ObjectRef) -> TableStructure {
        TableStructure {
            object,
            columns: vec![
                column("id", true, true, true),
                column("name", false, false, false),
                column("total", false, true, false),
            ],
            constraints: vec![],
            indexes: vec![IndexInfo {
                name: "pk".into(),
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

    fn t(database: &str, schema: Option<&str>, name: &str) -> ObjectRef {
        ObjectRef::new(NodeKind::Table, database, schema.map(str::to_owned), name)
    }

    #[test]
    fn truncate_per_dialect() {
        let pg = t("app", Some("public"), "orders");
        let all = TruncateOptions { restart_identity: true, cascade: true, disable_foreign_keys: false };
        assert_eq!(
            truncate(&PostgresDialect, &pg, all, None).unwrap(),
            [r#"TRUNCATE TABLE "public"."orders" RESTART IDENTITY CASCADE"#]
        );
        assert_eq!(
            truncate(&PostgresDialect, &pg, TruncateOptions::default(), None).unwrap(),
            [r#"TRUNCATE TABLE "public"."orders""#]
        );
        let my = t("shop", None, "orders");
        assert_eq!(
            truncate(&MysqlDialect, &my, TruncateOptions::default(), None).unwrap(),
            ["TRUNCATE TABLE `shop`.`orders`"]
        );
        let off = TruncateOptions { disable_foreign_keys: true, ..TruncateOptions::default() };
        assert_eq!(
            truncate(&MysqlDialect, &my, off, None).unwrap(),
            ["SET FOREIGN_KEY_CHECKS = 0", "TRUNCATE TABLE `shop`.`orders`", "SET FOREIGN_KEY_CHECKS = 1"]
        );
        let change = |table: &ObjectRef| SchemaChange::TruncateTable {
            table: table.clone(),
            restart_identity: true,
            cascade: true,
            disable_foreign_keys: false,
        };
        let refused = statements_with(&MysqlDialect, &change(&my), None, None).unwrap_err();
        assert_eq!(refused.kind, crate::ErrorKind::Unsupported, "MySQL has no RESTART IDENTITY / CASCADE");
        let ms = t("shop", Some("dbo"), "orders");
        assert_eq!(
            truncate(&SqlServerDialect, &ms, TruncateOptions::default(), None).unwrap(),
            ["TRUNCATE TABLE [dbo].[orders]"]
        );
        let ora = t("FREEPDB1", Some("APP"), "ORDERS");
        let cascade = TruncateOptions { cascade: true, ..TruncateOptions::default() };
        assert_eq!(
            truncate(&OracleDialect, &ora, cascade, None).unwrap(),
            [r#"TRUNCATE TABLE "APP"."ORDERS" CASCADE"#]
        );
        let ch = t("logs", None, "events");
        assert_eq!(
            truncate(&ClickhouseDialect, &ch, TruncateOptions::default(), None).unwrap(),
            ["TRUNCATE TABLE `logs`.`events`"]
        );
        let sf = t("DB", Some("PUBLIC"), "ORDERS");
        assert_eq!(
            truncate(&SnowflakeDialect, &sf, TruncateOptions::default(), None).unwrap(),
            [r#"TRUNCATE TABLE "DB"."PUBLIC"."ORDERS""#]
        );
        let duck = t("sales", Some("main"), "orders");
        assert_eq!(
            truncate(&DuckdbDialect, &duck, TruncateOptions::default(), None).unwrap(),
            [r#"TRUNCATE "sales"."main"."orders""#]
        );
        let cql = t("telemetry", None, "readings");
        assert_eq!(
            truncate(&CqlDialect, &cql, TruncateOptions::default(), None).unwrap(),
            [r#"TRUNCATE "telemetry"."readings""#]
        );
        assert!(statements_with(&MongoDialect, &change(&cql), None, None).is_err());
        let crdb = table_actions(SqlFlavor::Postgres, Some(Engine::Cockroachdb));
        assert!(
            crdb.check(&change(&pg)).is_err() && table_actions(SqlFlavor::Postgres, None).check(&change(&pg)).is_ok()
        );
        let view = ObjectRef { kind: NodeKind::View, ..pg };
        assert!(truncate(&PostgresDialect, &view, TruncateOptions::default(), None).is_err());
    }

    #[test]
    fn sqlite_truncate_deletes_and_resets_autoincrement() {
        let lite = t("main", None, "it's");
        let reset = TruncateOptions { restart_identity: true, ..TruncateOptions::default() };
        assert_eq!(
            truncate(&SqliteDialect, &lite, TruncateOptions::default(), None).unwrap(),
            [r#"DELETE FROM "main"."it's""#]
        );
        let auto =
            "CREATE TABLE \"it's\" (id INTEGER PRIMARY KEY AUTOINCREMENT, note TEXT);\nCREATE INDEX i ON x (note);";
        assert_eq!(
            truncate(&SqliteDialect, &lite, reset, Some(auto)).unwrap(),
            [r#"DELETE FROM "main"."it's""#, r#"DELETE FROM "main"."sqlite_sequence" WHERE name = 'it''s'"#]
        );
        // Without AUTOINCREMENT the next rowid restarts by itself once the table is empty.
        let plain = "CREATE TABLE t (id INTEGER PRIMARY KEY, note TEXT DEFAULT 'autoincrement')";
        assert_eq!(truncate(&SqliteDialect, &lite, reset, Some(plain)).unwrap().len(), 1);
        assert!(truncate(&SqliteDialect, &lite, reset, None).is_err());
    }

    #[test]
    fn empty_per_dialect() {
        assert_eq!(
            empty(&PostgresDialect, &t("app", Some("public"), "orders")).unwrap(),
            [r#"DELETE FROM "public"."orders""#]
        );
        assert_eq!(empty(&MysqlDialect, &t("shop", None, "orders")).unwrap(), ["DELETE FROM `shop`.`orders`"]);
        assert_eq!(empty(&SqliteDialect, &t("main", None, "orders")).unwrap(), [r#"DELETE FROM "main"."orders""#]);
        assert_eq!(
            empty(&SqlServerDialect, &t("shop", Some("dbo"), "orders")).unwrap(),
            ["DELETE FROM [dbo].[orders]"]
        );
        assert_eq!(
            empty(&ClickhouseDialect, &t("logs", None, "events")).unwrap(),
            ["DELETE FROM `logs`.`events` WHERE 1"]
        );
        let cql = SchemaChange::EmptyTable { table: t("ks", None, "r") };
        assert_eq!(statements_with(&CqlDialect, &cql, None, None).unwrap_err().kind, crate::ErrorKind::Unsupported);
    }

    #[test]
    fn duplicate_postgres_and_mysql() {
        let pg = t("app", Some("public"), "orders");
        let s = structure(pg.clone());
        assert_eq!(
            duplicate(&PostgresDialect, &pg, "orders_copy", false, Some(&s), None).unwrap(),
            [r#"CREATE TABLE "public"."orders_copy" (LIKE "public"."orders" INCLUDING ALL)"#]
        );
        assert_eq!(
            duplicate(&PostgresDialect, &pg, "orders_copy", true, Some(&s), None).unwrap(),
            [
                r#"CREATE TABLE "public"."orders_copy" (LIKE "public"."orders" INCLUDING ALL)"#,
                r#"INSERT INTO "public"."orders_copy" ("id", "name") OVERRIDING SYSTEM VALUE SELECT "id", "name" FROM "public"."orders""#,
                r#"SELECT setval(pg_get_serial_sequence('"public"."orders_copy"', 'id'), MAX("id")) FROM "public"."orders_copy" HAVING MAX("id") IS NOT NULL"#,
            ]
        );
        let my = t("shop", None, "orders");
        let s = structure(my.clone());
        assert_eq!(
            duplicate(&MysqlDialect, &my, "orders_copy", true, Some(&s), None).unwrap(),
            [
                "CREATE TABLE `shop`.`orders_copy` LIKE `shop`.`orders`",
                "INSERT INTO `shop`.`orders_copy` (`id`, `name`) SELECT `id`, `name` FROM `shop`.`orders`",
            ]
        );
        assert!(duplicate(&MysqlDialect, &my, "orders", true, Some(&s), None).is_err(), "same name");
        assert!(duplicate(&MysqlDialect, &my, " ", true, Some(&s), None).is_err());
    }

    #[test]
    fn duplicate_sqlserver_oracle_clickhouse_snowflake() {
        let ms = t("shop", Some("dbo"), "orders");
        let s = structure(ms.clone());
        assert_eq!(
            duplicate(&SqlServerDialect, &ms, "orders_copy", false, Some(&s), None).unwrap(),
            [
                "SELECT * INTO [dbo].[orders_copy] FROM [dbo].[orders] WHERE 1 = 0",
                "ALTER TABLE [dbo].[orders_copy] ADD PRIMARY KEY ([id])"
            ]
        );
        let ora = t("FREEPDB1", Some("APP"), "ORDERS");
        let s = structure(ora.clone());
        assert_eq!(
            duplicate(&OracleDialect, &ora, "ORDERS_COPY", true, Some(&s), None).unwrap(),
            [
                r#"CREATE TABLE "APP"."ORDERS_COPY" AS SELECT * FROM "APP"."ORDERS""#,
                r#"ALTER TABLE "APP"."ORDERS_COPY" ADD PRIMARY KEY ("id")"#
            ]
        );
        let ch = t("logs", None, "events");
        assert_eq!(
            duplicate(&ClickhouseDialect, &ch, "events_copy", true, None, None).unwrap(),
            [
                "CREATE TABLE `logs`.`events_copy` AS `logs`.`events`",
                "INSERT INTO `logs`.`events_copy` SELECT * FROM `logs`.`events`"
            ]
        );
        let sf = t("DB", Some("PUBLIC"), "ORDERS");
        assert_eq!(
            duplicate(&SnowflakeDialect, &sf, "COPY", true, None, None).unwrap(),
            [r#"CREATE TABLE "DB"."PUBLIC"."COPY" CLONE "DB"."PUBLIC"."ORDERS""#]
        );
        assert_eq!(
            duplicate(&SnowflakeDialect, &sf, "COPY", false, None, None).unwrap(),
            [r#"CREATE TABLE "DB"."PUBLIC"."COPY" LIKE "DB"."PUBLIC"."ORDERS""#]
        );
        let cql = SchemaChange::DuplicateTable { table: t("ks", None, "r"), new_name: "r2".into(), with_data: false };
        assert_eq!(statements_with(&CqlDialect, &cql, None, None).unwrap_err().kind, crate::ErrorKind::Unsupported);
    }

    #[test]
    fn duplicate_sqlite_and_duckdb_from_the_definition() {
        let lite = t("main", None, "orders");
        let s = structure(lite.clone());
        let definition = "CREATE TABLE IF NOT EXISTS \"orders\" (id INTEGER PRIMARY KEY, name TEXT, total AS (id * 2));\nCREATE INDEX x ON orders (name);\n";
        assert_eq!(
            duplicate(&SqliteDialect, &lite, "orders 2", true, Some(&s), Some(definition)).unwrap(),
            [
                r#"CREATE TABLE IF NOT EXISTS "main"."orders 2" (id INTEGER PRIMARY KEY, name TEXT, total AS (id * 2))"#,
                r#"INSERT INTO "main"."orders 2" ("id", "name") SELECT "id", "name" FROM "main"."orders""#,
            ]
        );
        let duck = t("sales", Some("main"), "orders");
        assert_eq!(
            duplicate(
                &DuckdbDialect,
                &duck,
                "copy",
                false,
                Some(&s),
                Some("CREATE TABLE main.orders(id INTEGER PRIMARY KEY);")
            )
            .unwrap(),
            [r#"CREATE TABLE "sales"."main"."copy"(id INTEGER PRIMARY KEY)"#]
        );
        assert!(duplicate(&DuckdbDialect, &duck, "copy", false, Some(&s), None).is_err());
        assert!(rename_created_table("CREATE VIEW v AS SELECT 1", SqlFlavor::Sqlite, "x").is_err());
        assert_eq!(
            rename_created_table("create temp table [a b](x)", SqlFlavor::Sqlite, "\"n\"").unwrap(),
            "create temp table \"n\"(x)"
        );
    }

    #[test]
    fn support_per_engine() {
        let pg = table_actions(SqlFlavor::Postgres, Some(Engine::Postgres));
        assert!(pg.truncate_restart_identity && pg.truncate_cascade && !pg.truncate_disable_foreign_keys);
        let crdb = table_actions(SqlFlavor::Postgres, Some(Engine::Cockroachdb));
        assert!(!crdb.truncate_restart_identity && crdb.truncate_cascade);
        let my = table_actions(SqlFlavor::Mysql, Some(Engine::Mariadb));
        assert!(my.truncate_disable_foreign_keys && my.truncate.note.is_some());
        let cql = table_actions(SqlFlavor::Cql, Some(Engine::Cassandra));
        assert!(cql.truncate.available && !cql.empty.available && !cql.duplicate.available);
        assert!(cql.empty.note.is_some());
        let mongo = table_actions(SqlFlavor::Mongodb, Some(Engine::Mongodb));
        assert!(!mongo.truncate.available);
    }
}
