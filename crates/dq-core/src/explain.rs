//! Query plans: the `EXPLAIN` statement for each engine and a common tree the
//! UI renders. PostgreSQL's JSON plan maps to named fields; MySQL and MariaDB
//! JSON plans are walked generically (every object is a node).

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
    Mysql,
    Mariadb,
}

impl ExplainDialect {
    pub fn new(flavor: SqlFlavor, version: &str) -> Self {
        match flavor {
            SqlFlavor::Postgres => Self::Postgres,
            SqlFlavor::Mysql if version.starts_with("MariaDB") => Self::Mariadb,
            SqlFlavor::Mysql => Self::Mysql,
        }
    }

    /// The statement that explains `sql`. `analyze` executes it, so callers
    /// only allow it for read-only statements.
    pub fn statement(self, sql: &str, analyze: bool) -> String {
        let sql = sql.trim().trim_end_matches(';');
        match (self, analyze) {
            (Self::Postgres, false) => format!("EXPLAIN (FORMAT JSON, VERBOSE) {sql}"),
            (Self::Postgres, true) => format!("EXPLAIN (FORMAT JSON, VERBOSE, ANALYZE, BUFFERS) {sql}"),
            (Self::Mysql | Self::Mariadb, false) => format!("EXPLAIN FORMAT=JSON {sql}"),
            // MySQL measures only in its TREE format.
            (Self::Mysql, true) => format!("EXPLAIN ANALYZE {sql}"),
            (Self::Mariadb, true) => format!("ANALYZE FORMAT=JSON {sql}"),
        }
    }

    pub fn parse(self, raw: String, analyzed: bool) -> DbResult<ExplainResult> {
        if self == Self::Mysql && analyzed {
            return Ok(ExplainResult { plan: None, raw, analyzed, planning_ms: None, execution_ms: None });
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
            Self::Mysql | Self::Mariadb => ExplainResult {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_per_engine() {
        assert_eq!(ExplainDialect::Postgres.statement("select 1;", false), "EXPLAIN (FORMAT JSON, VERBOSE) select 1");
        assert!(ExplainDialect::Postgres.statement("select 1", true).contains("ANALYZE"));
        assert_eq!(ExplainDialect::Mysql.statement("select 1", true), "EXPLAIN ANALYZE select 1");
        assert_eq!(ExplainDialect::Mariadb.statement("select 1", true), "ANALYZE FORMAT=JSON select 1");
        assert_eq!(ExplainDialect::new(SqlFlavor::Mysql, "MariaDB 11.4.2"), ExplainDialect::Mariadb);
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
