//! Plans: the search Profile API as a tree (shards → queries, collectors,
//! aggregations), and `_validate/query?explain` for a plan without running.

use dq_core::explain::{ExplainResult, PlanNode};
use dq_core::{DbError, DbResult};
use serde_json::Value;

fn ms(nanos: &Value) -> Option<f64> {
    nanos.as_f64().map(|n| n / 1_000_000.0)
}

/// One profiled query, collector or aggregation, with its children.
fn node(v: &Value, label_key: &str, prefix: &str) -> PlanNode {
    let label = v[label_key].as_str().unwrap_or("?");
    let mut props: Vec<(String, String)> = v["breakdown"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, n)| !k.ends_with("_count") && n.as_u64().is_some_and(|n| n > 0))
        .map(|(k, n)| (k.clone(), format!("{:.3} ms", n.as_f64().unwrap_or(0.0) / 1_000_000.0)))
        .collect();
    if let Some(reason) = v["reason"].as_str() {
        props.push(("reason".into(), reason.into()));
    }
    PlanNode {
        label: format!("{prefix}{label}"),
        detail: v["description"].as_str().map(str::to_owned),
        actual_ms: ms(&v["time_in_nanos"]),
        props,
        children: v["children"].as_array().into_iter().flatten().map(|c| node(c, label_key, prefix)).collect(),
        ..PlanNode::default()
    }
}

/// The `profile` section of a search response.
pub fn profile_plan(response: &Value, analyzed: bool) -> DbResult<ExplainResult> {
    let profile = &response["profile"];
    let shards = profile["shards"].as_array().ok_or_else(|| DbError::internal("the response has no profile"))?;
    let children: Vec<PlanNode> = shards
        .iter()
        .map(|shard| {
            let mut kids = Vec::new();
            for search in shard["searches"].as_array().into_iter().flatten() {
                kids.extend(search["query"].as_array().into_iter().flatten().map(|q| node(q, "type", "")));
                kids.extend(search["collector"].as_array().into_iter().flatten().map(|c| node(c, "name", "Collector: ")));
            }
            kids.extend(shard["aggregations"].as_array().into_iter().flatten().map(|a| node(a, "type", "Aggregation: ")));
            let total: f64 = kids.iter().filter_map(|k| k.actual_ms).sum();
            PlanNode {
                label: "Shard".into(),
                detail: shard["id"].as_str().map(str::to_owned),
                actual_ms: Some(total),
                children: kids,
                ..PlanNode::default()
            }
        })
        .collect();
    let took = response["took"].as_f64();
    let hits = response["hits"]["total"]["value"].as_f64();
    Ok(ExplainResult {
        plan: Some(PlanNode {
            label: "Search".into(),
            detail: Some(format!("{} shard(s)", children.len())),
            actual_ms: took,
            actual_rows: hits,
            children,
            ..PlanNode::default()
        }),
        raw: serde_json::to_string_pretty(profile).unwrap_or_default(),
        analyzed,
        planning_ms: None,
        execution_ms: took,
    })
}

/// `_validate/query?explain=true`: the Lucene query each index would run.
pub fn validate_plan(response: &Value) -> DbResult<ExplainResult> {
    let explanations = response["explanations"].as_array().cloned().unwrap_or_default();
    if response["valid"] == Value::Bool(false) {
        let reason = explanations.iter().find_map(|e| e["error"].as_str()).unwrap_or("The query is not valid.");
        return Err(DbError::invalid(reason));
    }
    let children = explanations
        .iter()
        .map(|e| PlanNode {
            label: "Index".into(),
            detail: e["index"].as_str().map(str::to_owned),
            props: e["explanation"].as_str().map(|x| vec![("Lucene query".to_owned(), x.to_owned())]).unwrap_or_default(),
            ..PlanNode::default()
        })
        .collect();
    Ok(ExplainResult {
        plan: Some(PlanNode { label: "Query".into(), detail: Some("not run".into()), children, ..PlanNode::default() }),
        raw: serde_json::to_string_pretty(response).unwrap_or_default(),
        analyzed: false,
        planning_ms: None,
        execution_ms: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn profile_becomes_a_tree() {
        let response = json!({
            "took": 4,
            "hits": {"total": {"value": 3}},
            "profile": {"shards": [{"id": "[n][orders][0]", "searches": [{
                "query": [{"type": "BooleanQuery", "description": "+status:paid", "time_in_nanos": 2_000_000,
                    "breakdown": {"score": 1_000_000, "score_count": 3, "next_doc": 0},
                    "children": [{"type": "TermQuery", "description": "status:paid", "time_in_nanos": 500_000}]}],
                "collector": [{"name": "SimpleTopScoreDocCollector", "reason": "search_top_hits", "time_in_nanos": 100_000}]}],
                "aggregations": []}]}
        });
        let r = profile_plan(&response, true).unwrap();
        let root = r.plan.unwrap();
        assert_eq!((root.label.as_str(), root.actual_rows), ("Search", Some(3.0)));
        let shard = &root.children[0];
        assert_eq!(shard.children.len(), 2);
        assert_eq!(shard.children[0].children[0].label, "TermQuery");
        assert_eq!(shard.children[0].props, vec![("score".to_owned(), "1.000 ms".to_owned())]);
        assert_eq!(shard.children[1].label, "Collector: SimpleTopScoreDocCollector");
        assert!(r.analyzed);
    }

    #[test]
    fn validate_explains_or_fails() {
        let ok = validate_plan(&json!({"valid": true, "explanations": [{"index": "o", "valid": true, "explanation": "status:paid"}]}))
            .unwrap();
        assert_eq!(ok.plan.unwrap().children[0].props[0].1, "status:paid");
        assert!(validate_plan(&json!({"valid": false, "explanations": [{"error": "bad"}]})).is_err());
    }
}
