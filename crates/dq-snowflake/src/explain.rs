//! Reads `EXPLAIN USING JSON`: a flat list of operations linked by `parentOperators`.

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

fn node_of(op: &Value) -> PlanNode {
    PlanNode {
        label: op.get("operation").map_or_else(|| "operation".into(), scalar),
        detail: op.get("objects").map(scalar),
        rows: op.get("partitionsAssigned").and_then(Value::as_f64),
        props: op
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(k, _)| !matches!(k.as_str(), "id" | "operation" | "objects" | "parentOperators"))
            .map(|(k, v)| (k.clone(), scalar(v)))
            .collect(),
        ..PlanNode::default()
    }
}

/// The operation `id` with its children (operations that name it as a parent).
fn build(id: i64, operations: &[&Value], depth: usize) -> Option<PlanNode> {
    let op = operations.iter().find(|o| o.get("id").and_then(Value::as_i64) == Some(id))?;
    let mut node = node_of(op);
    if depth < 200 {
        for child in operations.iter().filter(|o| {
            o.get("parentOperators").and_then(Value::as_array).is_some_and(|p| p.iter().any(|x| x.as_i64() == Some(id)))
        }) {
            if let Some(c) = child.get("id").and_then(Value::as_i64).and_then(|cid| build(cid, operations, depth + 1)) {
                node.children.push(c);
            }
        }
    }
    Some(node)
}

pub fn parse(raw: &str) -> DbResult<ExplainResult> {
    let json: Value = serde_json::from_str(raw).map_err(|e| DbError::internal(format!("unreadable plan: {e}")))?;
    let operations: Vec<&Value> = json
        .get("Operations")
        .and_then(Value::as_array)
        .map(|steps| steps.iter().filter_map(Value::as_array).flatten().collect())
        .unwrap_or_default();
    let roots: Vec<PlanNode> = operations
        .iter()
        .filter(|o| o.get("parentOperators").and_then(Value::as_array).is_none_or(Vec::is_empty))
        .filter_map(|o| o.get("id").and_then(Value::as_i64))
        .filter_map(|id| build(id, &operations, 0))
        .collect();
    let mut plan = PlanNode { label: "query".into(), children: roots, ..PlanNode::default() };
    if let Some(stats) = json.get("GlobalStats").and_then(Value::as_object) {
        plan.props = stats.iter().map(|(k, v)| (k.clone(), scalar(v))).collect();
    }
    let plan = if plan.children.len() == 1 && plan.props.is_empty() { plan.children.remove(0) } else { plan };
    Ok(ExplainResult {
        plan: Some(plan),
        raw: serde_json::to_string_pretty(&json).unwrap_or_else(|_| raw.to_owned()),
        analyzed: false,
        planning_ms: None,
        execution_ms: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_operations_to_parents() {
        let raw = r#"{"GlobalStats": {"partitionsTotal": 1}, "Operations": [[
            {"id": 0, "operation": "Result", "expressions": ["T.ID"]},
            {"id": 1, "parentOperators": [0], "operation": "Filter", "expressions": ["T.STATUS = 'paid'"]},
            {"id": 2, "parentOperators": [1], "operation": "TableScan", "objects": ["DB.PUBLIC.T"]}]]}"#;
        let plan = parse(raw).unwrap().plan.unwrap();
        let result = &plan.children[0];
        assert_eq!(result.label, "Result");
        let scan = &result.children[0].children[0];
        assert_eq!((scan.label.as_str(), scan.detail.as_deref()), ("TableScan", Some("DB.PUBLIC.T")));
    }
}
