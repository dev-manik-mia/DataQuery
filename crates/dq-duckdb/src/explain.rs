//! Reads `DuckDB`'s JSON plans (`EXPLAIN (FORMAT JSON)` and its `ANALYZE` form).

use dq_core::explain::{ExplainResult, PlanNode};
use dq_core::{DbError, DbResult};
use serde_json::Value;

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(scalar).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

fn plan_node(v: &Value) -> PlanNode {
    let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim).map(str::to_owned);
    let mut out = PlanNode {
        label: text("name").or_else(|| text("operator_name")).or_else(|| text("operator_type")).unwrap_or_else(|| "operator".into()),
        actual_rows: v.get("operator_cardinality").and_then(Value::as_f64),
        actual_ms: v.get("operator_timing").and_then(Value::as_f64).map(|s| s * 1000.0),
        ..PlanNode::default()
    };
    if let Some(extra) = v.get("extra_info").and_then(Value::as_object) {
        for (key, value) in extra {
            match key.as_str() {
                "Estimated Cardinality" => out.rows = scalar(value).trim_start_matches('~').parse().ok(),
                "Table" => out.detail = Some(scalar(value)),
                _ => out.props.push((key.clone(), scalar(value))),
            }
        }
    }
    out.children = v.get("children").and_then(Value::as_array).map(|c| c.iter().map(plan_node).collect()).unwrap_or_default();
    out
}

/// `raw` is the JSON value the plan statement returned.
pub fn parse(raw: &str, analyzed: bool) -> DbResult<ExplainResult> {
    let json: Value = serde_json::from_str(raw).map_err(|e| DbError::internal(format!("unreadable plan: {e}")))?;
    let pretty = serde_json::to_string_pretty(&json).unwrap_or_else(|_| raw.to_owned());
    let (plan, execution_ms) = if analyzed {
        // `{ "latency": s, "children": [...] }`
        let children: Vec<PlanNode> = json.get("children").and_then(Value::as_array).map(|c| c.iter().map(plan_node).collect()).unwrap_or_default();
        let mut root = PlanNode { label: "query".into(), children, ..PlanNode::default() };
        if root.children.len() == 1 {
            root = root.children.remove(0);
        }
        (root, json.get("latency").and_then(Value::as_f64).map(|s| s * 1000.0))
    } else {
        // `[ {root operator} ]`
        let root = json.as_array().and_then(|a| a.first()).map(plan_node).ok_or_else(|| DbError::internal("empty plan"))?;
        (root, None)
    };
    Ok(ExplainResult { plan: Some(plan), raw: pretty, analyzed, planning_ms: None, execution_ms })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_plan_forms() {
        let plain = r#"[{"name": "PROJECTION", "children": [{"name": "SEQ_SCAN ", "children": [],
            "extra_info": {"Table": "orders", "Filters": "status='paid'", "Estimated Cardinality": "400"}}],
            "extra_info": {"Projections": ["id"], "Estimated Cardinality": "400"}}]"#;
        let r = parse(plain, false).unwrap();
        let scan = &r.plan.unwrap().children[0];
        assert_eq!((scan.label.as_str(), scan.detail.as_deref(), scan.rows), ("SEQ_SCAN", Some("orders"), Some(400.0)));
        let analyzed = r#"{"latency": 0.002, "children": [{"operator_name": "UNGROUPED_AGGREGATE", "operator_timing": 0.001,
            "operator_cardinality": 1, "children": []}]}"#;
        let r = parse(analyzed, true).unwrap();
        assert_eq!(r.execution_ms, Some(2.0));
        assert_eq!(r.plan.unwrap().actual_rows, Some(1.0));
    }
}
