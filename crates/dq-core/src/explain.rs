//! Query plans: the `EXPLAIN` statement for each engine and a common tree the
//! UI renders. PostgreSQL's JSON plan maps to named fields; MySQL and MariaDB
//! JSON plans are walked generically (every object is a node); `CockroachDB`'s
//! text tree and Redshift's `PostgreSQL`-style text plan are read line by line. SQL Server and Oracle plans are read by
//! their adapters, which build the same tree.

use serde::Serialize;
use serde_json::Value;
use ts_rs::TS;

use crate::error::{DbError, DbResult};
use crate::sql::SqlFlavor;

#[derive(Debug, Clone, Default, PartialEq, Serialize, TS)]
#[ts(export)]
pub struct PlanNode {
    /// Operation, e.g. `Seq Scan` or `table`.
    pub label: String,
    /// Object or condition the operation works on, e.g. `public.orders`.
    pub detail: Option<String>,
    /// Estimated total cost in the engine's own units.
    pub cost: Option<f64>,
    /// Estimated rows.
    pub rows: Option<f64>,
    /// Measured time in milliseconds (EXPLAIN ANALYZE).
    pub actual_ms: Option<f64>,
    pub actual_rows: Option<f64>,
    pub loops: Option<f64>,
    /// Other properties, e.g. `Filter`, `Index Cond`, `access_type`.
    pub props: Vec<(String, String)>,
    pub children: Vec<PlanNode>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ExplainResult {
    /// `None` when the engine returned a text plan (MySQL `EXPLAIN ANALYZE`).
    pub plan: Option<PlanNode>,
    /// The plan as the server returned it.
    pub raw: String,
    pub analyzed: bool,
    pub planning_ms: Option<f64>,
    pub execution_ms: Option<f64>,
}

/// Which plan output the server supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplainDialect {
    Postgres,
    Cockroach,
    /// Amazon Redshift: `EXPLAIN` as text only, without `ANALYZE`.
    Redshift,
    Mysql,
    Mariadb,
    /// `EXPLAIN JSON`; measured plans come from `PROFILE` and then `SHOW PROFILE JSON`.
    Singlestore,
}

impl ExplainDialect {
    /// The plan statement family, or `None` when the adapter reads plans itself.
    pub fn new(flavor: SqlFlavor, version: &str) -> Option<Self> {
        match flavor {
            SqlFlavor::Postgres if version.starts_with("CockroachDB") => Some(Self::Cockroach),
            SqlFlavor::Postgres if version.starts_with("Amazon Redshift") => Some(Self::Redshift),
            SqlFlavor::Postgres => Some(Self::Postgres),
            SqlFlavor::Mysql if version.starts_with("MariaDB") => Some(Self::Mariadb),
            SqlFlavor::Mysql if version.starts_with("SingleStore") => Some(Self::Singlestore),
            SqlFlavor::Mysql => Some(Self::Mysql),
            SqlFlavor::Sqlserver
            | SqlFlavor::Oracle
            | SqlFlavor::Clickhouse
            | SqlFlavor::Duckdb
            | SqlFlavor::Sqlite
            | SqlFlavor::Snowflake
            | SqlFlavor::Bigquery
            | SqlFlavor::Vertica
            | SqlFlavor::Mongodb
            | SqlFlavor::Partiql
            | SqlFlavor::Redis
            | SqlFlavor::Cql
            | SqlFlavor::QueryDsl
            | SqlFlavor::Cypher => None,
        }
    }

    /// The engine can run a statement to measure its plan (`EXPLAIN ANALYZE`).
    pub fn analyzes(self) -> bool {
        self != Self::Redshift
    }

    /// The statement that explains `sql`. `analyze` executes it, so callers
    /// only allow it for read-only statements.
    pub fn statement(self, sql: &str, analyze: bool) -> String {
        let sql = sql.trim().trim_end_matches(';');
        match (self, analyze) {
            (Self::Postgres, false) => format!("EXPLAIN (FORMAT JSON, VERBOSE) {sql}"),
            (Self::Postgres, true) => format!("EXPLAIN (FORMAT JSON, VERBOSE, ANALYZE, BUFFERS) {sql}"),
            (Self::Cockroach, false) => format!("EXPLAIN (VERBOSE) {sql}"),
            (Self::Cockroach, true) => format!("EXPLAIN ANALYZE (VERBOSE) {sql}"),
            (Self::Redshift, _) => format!("EXPLAIN {sql}"),
            (Self::Mysql | Self::Mariadb, false) => format!("EXPLAIN FORMAT=JSON {sql}"),
            // MySQL measures only in its TREE format.
            (Self::Mysql, true) => format!("EXPLAIN ANALYZE {sql}"),
            (Self::Mariadb, true) => format!("ANALYZE FORMAT=JSON {sql}"),
            (Self::Singlestore, false) => format!("EXPLAIN JSON {sql}"),
            (Self::Singlestore, true) => format!("PROFILE {sql}"),
        }
    }

    /// A statement on the same connection that returns the plan, when
    /// [`Self::statement`] itself returns the query's rows instead.
    pub fn plan_statement(self, analyze: bool) -> Option<&'static str> {
        (self == Self::Singlestore && analyze).then_some("SHOW PROFILE JSON")
    }

    pub fn parse(self, raw: String, analyzed: bool) -> DbResult<ExplainResult> {
        if self == Self::Mysql && analyzed {
            return Ok(ExplainResult { plan: None, raw, analyzed, planning_ms: None, execution_ms: None });
        }
        if self == Self::Cockroach {
            return Ok(cockroach_plan(raw, analyzed));
        }
        if self == Self::Redshift {
            return Ok(text_plan(raw, analyzed));
        }
        let json: Value = serde_json::from_str(&raw).map_err(|e| DbError::internal(format!("unreadable plan: {e}")))?;
        Ok(match self {
            Self::Postgres => {
                let root = json.get(0).ok_or_else(|| DbError::internal("empty plan"))?;
                let plan = root.get("Plan").ok_or_else(|| DbError::internal("plan without a root node"))?;
                ExplainResult {
                    plan: Some(pg_node(plan)),
                    planning_ms: root.get("Planning Time").and_then(Value::as_f64),
                    execution_ms: root.get("Execution Time").and_then(Value::as_f64),
                    raw: serde_json::to_string_pretty(&json).unwrap_or(raw),
                    analyzed,
                }
            }
            Self::Cockroach | Self::Redshift | Self::Mysql | Self::Mariadb | Self::Singlestore => ExplainResult {
                plan: Some(generic_node("query", &json)),
                planning_ms: None,
                execution_ms: None,
                raw: serde_json::to_string_pretty(&json).unwrap_or(raw),
                analyzed,
            },
        })
    }
}

const PG_SKIP: &[&str] = &[
    "Node Type",
    "Plans",
    "Startup Cost",
    "Total Cost",
    "Plan Rows",
    "Plan Width",
    "Actual Startup Time",
    "Actual Total Time",
    "Actual Rows",
    "Actual Loops",
    "Relation Name",
    "Schema",
    "Alias",
    "Index Name",
    "Parallel Aware",
    "Async Capable",
    "Parent Relationship",
];

fn pg_node(node: &Value) -> PlanNode {
    let v = node;
    let num = |k: &str| v.get(k).and_then(Value::as_f64);
    let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let relation = text("Relation Name").map(|r| match text("Schema") {
        Some(schema) => format!("{schema}.{r}"),
        None => r,
    });
    let detail = match (relation, text("Index Name")) {
        (Some(r), Some(i)) => Some(format!("{r} using {i}")),
        (Some(r), None) => Some(r),
        (None, Some(i)) => Some(format!("using {i}")),
        (None, None) => text("CTE Name").or_else(|| text("Subplan Name")).or_else(|| text("Function Name")),
    };
    let props = v
        .as_object()
        .map(|o| {
            o.iter().filter(|(k, _)| !PG_SKIP.contains(&k.as_str())).map(|(k, v)| (k.clone(), scalar_text(v))).collect()
        })
        .unwrap_or_default();
    PlanNode {
        label: text("Node Type").unwrap_or_else(|| "Node".into()),
        detail,
        cost: num("Total Cost"),
        rows: num("Plan Rows"),
        actual_ms: num("Actual Total Time"),
        actual_rows: num("Actual Rows"),
        loops: num("Actual Loops"),
        props,
        children: v.get("Plans").and_then(Value::as_array).map(|a| a.iter().map(pg_node).collect()).unwrap_or_default(),
    }
}

fn scalar_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(scalar_text).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

fn number(v: Option<&Value>) -> Option<f64> {
    v.and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
}

/// MySQL / MariaDB JSON plans: objects become nodes, scalars become properties.
fn generic_node(label: &str, v: &Value) -> PlanNode {
    let mut node = PlanNode { label: label.to_owned(), ..PlanNode::default() };
    let Some(obj) = v.as_object() else {
        node.props.push(("value".into(), scalar_text(v)));
        return node;
    };
    node.detail = obj.get("table_name").and_then(Value::as_str).map(str::to_owned);
    let cost_info = obj.get("cost_info");
    node.cost = number(cost_info.and_then(|c| c.get("query_cost").or_else(|| c.get("prefix_cost"))))
        .or_else(|| number(obj.get("cost")));
    node.rows = number(obj.get("rows_produced_per_join").or_else(|| obj.get("rows")));
    node.actual_rows = number(obj.get("r_rows"));
    node.actual_ms = number(obj.get("r_total_time_ms"));
    node.loops = number(obj.get("r_loops"));
    for (key, value) in obj {
        match value {
            Value::Object(_) if key != "cost_info" => node.children.push(generic_node(key, value)),
            Value::Array(items) if items.iter().all(Value::is_object) && !items.is_empty() => {
                for (i, item) in items.iter().enumerate() {
                    let child_label = item.as_object().filter(|o| o.len() == 1).and_then(|o| o.keys().next().cloned());
                    match child_label {
                        // `nested_loop: [{"table": {...}}]` reads better as the table node itself.
                        Some(inner) => node.children.push(generic_node(&inner, &item[&inner])),
                        None => node.children.push(generic_node(&format!("{key}[{i}]"), item)),
                    }
                }
            }
            Value::Object(_) => {
                for (k, v) in value.as_object().into_iter().flatten() {
                    node.props.push((format!("cost.{k}"), scalar_text(v)));
                }
            }
            _ if key == "table_name" => {}
            _ => node.props.push((key.clone(), scalar_text(value))),
        }
    }
    node
}

/// Leading number of a `CockroachDB` value such as `3 (100% of the table; …)` or `1,024`.
fn leading_number(value: &str) -> Option<f64> {
    let digits: String = value.chars().take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '.').collect();
    digits.replace(',', "").parse().ok()
}

/// A `CockroachDB` duration such as `250µs`, `1ms` or `2.5s`, in milliseconds.
pub fn duration_ms(value: &str) -> Option<f64> {
    let value = value.trim();
    let split = value.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (number, unit) = value.split_at(split);
    let n: f64 = number.parse().ok()?;
    Some(match unit.trim() {
        "ns" => n / 1_000_000.0,
        "µs" | "us" => n / 1_000.0,
        "ms" => n,
        "s" => n * 1_000.0,
        "m" => n * 60_000.0,
        _ => return None,
    })
}

/// Closes the open plan nodes indented at `until` or deeper, attaching each to
/// the node above it (or to `roots`). `stack` holds (column, node) pairs from
/// the root down to the current node.
fn close_nodes(stack: &mut Vec<(usize, PlanNode)>, roots: &mut Vec<PlanNode>, until: usize) {
    while stack.last().is_some_and(|(col, _)| *col >= until) {
        let (_, node) = stack.pop().expect("non-empty stack");
        match stack.last_mut() {
            Some((_, parent)) => parent.children.push(node),
            None => roots.push(node),
        }
    }
}

/// `cost=0.00..1.72 rows=172 width=4` or `actual time=0.01..0.20 rows=3 loops=1`,
/// a parenthesised group after a text plan node.
fn read_estimates(node: &mut PlanNode, group: &str) {
    let actual = group.starts_with("actual");
    for (key, value) in group.split_whitespace().filter_map(|kv| kv.split_once('=')) {
        let upper = || value.rsplit("..").next().and_then(|v| v.parse().ok());
        match (key, actual) {
            ("cost", _) => node.cost = upper(),
            ("rows", false) => node.rows = value.parse().ok(),
            ("time", true) => node.actual_ms = upper(),
            ("rows", true) => node.actual_rows = value.parse().ok(),
            ("loops", _) => node.loops = value.parse().ok(),
            _ => {}
        }
    }
}

/// One text plan node: `XN Seq Scan on sales  (cost=0.00..1.72 rows=172 width=4)`.
fn text_plan_node(content: &str) -> PlanNode {
    let (head, groups) = content.split_once("  (").unwrap_or((content, ""));
    let head = head.trim();
    let (label, detail) = match head.split_once(" on ").or_else(|| head.split_once(" using ")) {
        Some((label, detail)) => (label.trim(), Some(detail.trim().to_owned())),
        None => (head, None),
    };
    let mut node = PlanNode { label: label.to_owned(), detail, ..PlanNode::default() };
    for group in groups.split('(') {
        read_estimates(&mut node, group.trim().trim_end_matches(')'));
    }
    node
}

/// Reads a `PostgreSQL`-style text plan (Redshift's only format): the first line
/// and `->` lines are nodes nested by indentation, `Key: value` lines below a
/// node are its properties, and Redshift's `----- … -----` notes go on the root.
fn text_plan(raw: String, analyzed: bool) -> ExplainResult {
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    let mut roots: Vec<PlanNode> = Vec::new();
    let mut notes = Vec::new();
    let mut planning_ms = None;
    let mut execution_ms = None;
    for line in raw.lines() {
        let content = line.trim_start();
        if content.is_empty() {
            continue;
        }
        let column = line.len() - content.len();
        if let Some(note) = content.strip_prefix("-----") {
            notes.push(note.trim_matches(|c: char| c == '-' || c.is_whitespace()).to_owned());
            continue;
        }
        if let Some(node) = content.strip_prefix("->") {
            close_nodes(&mut stack, &mut roots, column);
            stack.push((column, text_plan_node(node)));
            continue;
        }
        let key_value = content.split_once(": ").map(|(k, v)| (k.trim(), v.trim()));
        match (stack.last_mut(), key_value) {
            (_, Some(("Planning Time" | "Planning time", v))) => planning_ms = leading_number(v),
            (_, Some(("Execution Time" | "Execution time", v))) => execution_ms = leading_number(v),
            (Some((_, node)), Some((key, value))) => node.props.push((key.to_owned(), value.to_owned())),
            (Some((_, node)), None) => node.props.push(("note".to_owned(), content.to_owned())),
            (None, _) if content.contains("(cost=") => stack.push((column, text_plan_node(content))),
            _ => notes.push(content.to_owned()),
        }
    }
    close_nodes(&mut stack, &mut roots, 0);
    let mut plan = match roots.len() {
        0 => None,
        1 => roots.pop(),
        _ => Some(PlanNode { label: "plan".into(), children: roots, ..PlanNode::default() }),
    };
    if let Some(root) = plan.as_mut() {
        root.props.extend(notes.into_iter().filter(|n| !n.is_empty()).map(|n| ("note".to_owned(), n)));
    }
    ExplainResult { plan, raw, analyzed, planning_ms, execution_ms }
}

/// Reads `CockroachDB`'s `EXPLAIN` text tree: `• op` lines are nodes, indented
/// `key: value` lines are properties of the node above them.
fn cockroach_plan(raw: String, analyzed: bool) -> ExplainResult {
    let mut header = PlanNode { label: "plan".into(), ..PlanNode::default() };
    let mut planning_ms = None;
    let mut execution_ms = None;
    // (bullet column, node) from the root down to the current node.
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    let mut roots: Vec<PlanNode> = Vec::new();
    for line in raw.lines() {
        let content_start =
            line.char_indices().find(|(_, c)| !matches!(c, ' ' | '│' | '├' | '└' | '─')).map(|(i, _)| i);
        let Some(start) = content_start else { continue };
        let content = &line[start..];
        let column = line[..start].chars().count();
        if let Some(label) = content.strip_prefix('•') {
            close_nodes(&mut stack, &mut roots, column);
            stack.push((column, PlanNode { label: label.trim().to_owned(), ..PlanNode::default() }));
            continue;
        }
        let Some((key, value)) = content.split_once(':') else { continue };
        let (key, value) = (key.trim(), value.trim());
        let Some((_, node)) = stack.last_mut() else {
            match key {
                "planning time" => planning_ms = duration_ms(value),
                "execution time" => execution_ms = duration_ms(value),
                _ => header.props.push((key.to_owned(), value.to_owned())),
            }
            continue;
        };
        match key {
            "table" | "index" if node.detail.is_none() => node.detail = Some(value.to_owned()),
            "estimated row count" => node.rows = leading_number(value),
            "actual row count" => node.actual_rows = leading_number(value),
            "execution time" => node.actual_ms = duration_ms(value),
            _ => node.props.push((key.to_owned(), value.to_owned())),
        }
    }
    close_nodes(&mut stack, &mut roots, 0);
    let plan = match roots.len() {
        0 => None,
        1 if header.props.is_empty() => roots.pop(),
        _ => {
            header.children = roots;
            Some(header)
        }
    };
    ExplainResult { plan, raw, analyzed, planning_ms, execution_ms }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_per_engine() {
        assert_eq!(ExplainDialect::Postgres.statement("select 1;", false), "EXPLAIN (FORMAT JSON, VERBOSE) select 1");
        assert!(ExplainDialect::Postgres.statement("select 1", true).contains("ANALYZE"));
        assert_eq!(ExplainDialect::Mysql.statement("select 1", true), "EXPLAIN ANALYZE select 1");
        assert_eq!(ExplainDialect::Mariadb.statement("select 1", true), "ANALYZE FORMAT=JSON select 1");
        assert_eq!(ExplainDialect::new(SqlFlavor::Mysql, "MariaDB 11.4.2"), Some(ExplainDialect::Mariadb));
        let s2 = ExplainDialect::new(SqlFlavor::Mysql, "SingleStore 8.9.3").unwrap();
        assert_eq!((s2.statement("select 1", false), s2.plan_statement(false)), ("EXPLAIN JSON select 1".into(), None));
        assert_eq!(
            (s2.statement("select 1", true), s2.plan_statement(true)),
            ("PROFILE select 1".into(), Some("SHOW PROFILE JSON"))
        );
        assert_eq!(ExplainDialect::Mysql.plan_statement(true), None);
        assert_eq!(ExplainDialect::new(SqlFlavor::Postgres, "CockroachDB 25.2.1"), Some(ExplainDialect::Cockroach));
        assert_eq!(ExplainDialect::new(SqlFlavor::Sqlserver, "SQL Server 2022"), None);
        let redshift = ExplainDialect::new(SqlFlavor::Postgres, "Amazon Redshift 1.0.12103").unwrap();
        assert_eq!((redshift, redshift.analyzes()), (ExplainDialect::Redshift, false));
        assert_eq!(redshift.statement("select 1;", false), "EXPLAIN select 1");
        assert_eq!(ExplainDialect::new(SqlFlavor::Postgres, "Greenplum 7.1.0"), Some(ExplainDialect::Postgres));
    }

    #[test]
    fn parses_redshift_text_plans() {
        let raw = "XN Merge  (cost=1000000000136.38..1000000000137.82 rows=576 width=33)
  Merge Key: sum(sales.pricepaid)
  ->  XN Network  (cost=1000000000136.38..1000000000137.82 rows=576 width=33)
        Send to leader
        ->  XN Hash Join DS_DIST_ALL_NONE  (cost=0.00..21.05 rows=576 width=33)
              Hash Cond: (\"outer\".eventid = \"inner\".eventid)
              ->  XN Seq Scan on sales  (cost=0.00..1.72 rows=172 width=12)
              ->  XN Hash  (cost=0.80..0.80 rows=80 width=25)
                    ->  XN Seq Scan on event e  (cost=0.00..0.80 rows=80 width=25)
----- Tables missing statistics: event -----
----- Update statistics by running the ANALYZE command on these tables -----";
        let r = ExplainDialect::Redshift.parse(raw.into(), false).unwrap();
        let root = r.plan.unwrap();
        assert_eq!((root.label.as_str(), root.cost, root.rows), ("XN Merge", Some(1_000_000_000_137.82), Some(576.0)));
        assert_eq!(root.props[0], ("Merge Key".to_owned(), "sum(sales.pricepaid)".to_owned()));
        assert!(root.props.iter().any(|(k, v)| k == "note" && v == "Tables missing statistics: event"));
        let network = &root.children[0];
        assert_eq!(network.props, [("note".to_owned(), "Send to leader".to_owned())]);
        let join = &network.children[0];
        assert_eq!(join.label, "XN Hash Join DS_DIST_ALL_NONE");
        assert_eq!(join.props, [("Hash Cond".to_owned(), "(\"outer\".eventid = \"inner\".eventid)".to_owned())]);
        assert_eq!(join.children.len(), 2);
        assert_eq!(
            (join.children[0].label.as_str(), join.children[0].detail.as_deref()),
            ("XN Seq Scan", Some("sales"))
        );
        let scan = &join.children[1].children[0];
        assert_eq!((scan.detail.as_deref(), scan.rows, scan.cost), (Some("event e"), Some(80.0), Some(0.8)));
    }

    #[test]
    fn parses_postgres_text_plans_with_timings() {
        let raw = "Seq Scan on orders o  (cost=0.00..1.05 rows=5 width=8) (actual time=0.010..0.020 rows=3 loops=1)
  Filter: (id > 1)
Planning Time: 0.050 ms
Execution Time: 0.100 ms";
        let r = text_plan(raw.into(), true);
        let root = r.plan.unwrap();
        assert_eq!(
            (root.label.as_str(), root.detail.as_deref(), root.actual_ms, root.actual_rows, root.loops),
            ("Seq Scan", Some("orders o"), Some(0.02), Some(3.0), Some(1.0))
        );
        assert_eq!(root.props, [("Filter".to_owned(), "(id > 1)".to_owned())]);
        assert_eq!((r.planning_ms, r.execution_ms), (Some(0.05), Some(0.1)));
    }

    #[test]
    fn parses_cockroach_text_plans() {
        let raw = "planning time: 250µs\nexecution time: 2ms\ndistribution: local\nvectorized: true\n\n\
• filter\n│ columns: (id, name)\n│ estimated row count: 1\n│ filter: id > 1\n│\n\
└── • scan\n      columns: (id, name)\n      actual row count: 3\n      estimated row count: 3 (100% of the table)\n      table: t@t_pkey\n      spans: FULL SCAN";
        let r = ExplainDialect::Cockroach.parse(raw.into(), true).unwrap();
        assert_eq!((r.planning_ms, r.execution_ms), (Some(0.25), Some(2.0)));
        let root = r.plan.unwrap();
        assert_eq!(root.label, "plan");
        let filter = &root.children[0];
        assert_eq!((filter.label.as_str(), filter.rows), ("filter", Some(1.0)));
        let scan = &filter.children[0];
        assert_eq!(
            (scan.label.as_str(), scan.detail.as_deref(), scan.actual_rows),
            ("scan", Some("t@t_pkey"), Some(3.0))
        );
    }

    #[test]
    fn parses_postgres_plans() {
        let raw = r#"[{"Plan":{"Node Type":"Sort","Total Cost":12.5,"Plan Rows":10,"Sort Key":["total DESC"],
            "Plans":[{"Node Type":"Seq Scan","Relation Name":"orders","Schema":"public","Total Cost":10.0,
            "Plan Rows":10,"Filter":"(status = 'paid'::text)","Actual Total Time":0.12,"Actual Rows":7,"Actual Loops":1}]},
            "Planning Time":0.2,"Execution Time":0.4}]"#;
        let r = ExplainDialect::Postgres.parse(raw.into(), true).unwrap();
        let plan = r.plan.unwrap();
        assert_eq!(plan.label, "Sort");
        assert!(plan.props.contains(&("Sort Key".into(), "total DESC".into())));
        let scan = &plan.children[0];
        assert_eq!((scan.label.as_str(), scan.detail.as_deref()), ("Seq Scan", Some("public.orders")));
        assert_eq!((scan.actual_rows, scan.loops), (Some(7.0), Some(1.0)));
        assert_eq!(r.execution_ms, Some(0.4));
    }

    #[test]
    fn parses_mysql_plans_generically() {
        let raw = r#"{"query_block":{"select_id":1,"cost_info":{"query_cost":"41.00"},
            "nested_loop":[{"table":{"table_name":"o","access_type":"ALL","rows_examined_per_scan":400,
            "rows_produced_per_join":40,"cost_info":{"read_cost":"36.00","prefix_cost":"41.00"}}}]}}"#;
        let r = ExplainDialect::Mysql.parse(raw.into(), false).unwrap();
        let block = &r.plan.unwrap().children[0];
        assert_eq!((block.label.as_str(), block.cost), ("query_block", Some(41.0)));
        let table = &block.children[0];
        assert_eq!((table.label.as_str(), table.detail.as_deref(), table.rows), ("table", Some("o"), Some(40.0)));
        assert!(table.props.contains(&("access_type".into(), "ALL".into())));
        let text = ExplainDialect::Mysql.parse("-> Table scan on o".into(), true).unwrap();
        assert!(text.plan.is_none() && text.raw.contains("Table scan"));
    }
}
