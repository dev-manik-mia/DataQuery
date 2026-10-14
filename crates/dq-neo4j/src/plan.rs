//! `EXPLAIN` / `PROFILE` plans (`queryPlan`, `profiledQueryPlan`) as the common plan tree.

use dq_core::explain::{ExplainResult, PlanNode};
use serde_json::Value;

/// A typed argument's value as text.
fn arg_text(args: &Value, name: &str) -> Option<String> {
    let v = args.get(name)?;
    let inner = v.get("_value").unwrap_or(v);
    match inner {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

fn arg_number(args: &Value, name: &str) -> Option<f64> {
    arg_text(args, name)?.parse().ok()
}

/// Arguments shown as properties; the rest are planner bookkeeping.
const SHOWN: &[&str] = &["Memory", "GlobalMemory", "Order", "PipelineInfo", "batch-size"];

pub fn node(plan: &Value, profiled: bool) -> PlanNode {
    let args = plan.get("arguments").cloned().unwrap_or(Value::Null);
    let label = plan
        .get("operatorType")
        .and_then(Value::as_str)
        .unwrap_or("Operator")
        .split('@')
        .next()
        .unwrap_or("Operator")
        .to_owned();
    let mut props: Vec<(String, String)> = Vec::new();
    let number = |k: &str| plan.get(k).and_then(Value::as_f64);
    if profiled {
        for (key, name) in
            [("dbHits", "DB hits"), ("pageCacheHits", "Page cache hits"), ("pageCacheMisses", "Page cache misses")]
        {
            if let Some(n) = number(key) {
                props.push((name.to_owned(), format!("{n}")));
            }
        }
    }
    for name in SHOWN {
        if let Some(v) = arg_text(&args, name) {
            props.push(((*name).to_owned(), v));
        }
    }
    if let Some(ids) = plan.get("identifiers").and_then(Value::as_array).filter(|i| !i.is_empty()) {
        let ids: Vec<&str> = ids.iter().filter_map(Value::as_str).collect();
        props.push(("Identifiers".to_owned(), ids.join(", ")));
    }
    // `time` is reported in nanoseconds, and only when the runtime measures it.
    let actual_ms = profiled.then(|| number("time")).flatten().filter(|t| *t > 0.0).map(|ns| ns / 1_000_000.0);
    PlanNode {
        label,
        detail: arg_text(&args, "Details"),
        cost: profiled.then(|| number("dbHits")).flatten(),
        rows: arg_number(&args, "EstimatedRows"),
        actual_ms,
        actual_rows: profiled.then(|| number("records")).flatten(),
        loops: None,
        props,
        children: plan
            .get("children")
            .and_then(Value::as_array)
            .map(|c| c.iter().map(|child| node(child, profiled)).collect())
            .unwrap_or_default(),
    }
}

/// The plan tree; `raw` is the server's own table rendering when it sends one.
pub fn result(plan: &Value, profiled: bool, execution_ms: Option<f64>) -> ExplainResult {
    let args = plan.get("arguments").cloned().unwrap_or(Value::Null);
    let raw = arg_text(&args, "string-representation")
        .unwrap_or_else(|| serde_json::to_string_pretty(plan).unwrap_or_default());
    ExplainResult { plan: Some(node(plan, profiled)), raw, analyzed: profiled, planning_ms: None, execution_ms }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_profiled_plans() {
        let plan = json!({
            "operatorType": "ProduceResults@neo4j", "dbHits": 0, "records": 2, "time": 1_500_000,
            "arguments": { "Details": { "$type": "String", "_value": "n" }, "EstimatedRows": { "$type": "Float", "_value": "2.0" },
                           "string-representation": { "$type": "String", "_value": "+ProduceResults" } },
            "identifiers": ["n"],
            "children": [{ "operatorType": "NodeByLabelScan@neo4j", "dbHits": 3, "records": 2,
                           "arguments": { "Details": { "$type": "String", "_value": "n:Person" } }, "identifiers": ["n"], "children": [] }]
        });
        let r = result(&plan, true, Some(4.0));
        let root = r.plan.unwrap();
        assert_eq!((root.label.as_str(), root.detail.as_deref(), root.rows), ("ProduceResults", Some("n"), Some(2.0)));
        assert_eq!((root.actual_rows, root.actual_ms), (Some(2.0), Some(1.5)));
        assert_eq!(root.children[0].label, "NodeByLabelScan");
        assert_eq!(root.children[0].cost, Some(3.0));
        assert_eq!(r.raw, "+ProduceResults");
        assert!(r.analyzed);
        let explain = node(&plan, false);
        assert_eq!((explain.actual_rows, explain.cost), (None, None));
    }
}
