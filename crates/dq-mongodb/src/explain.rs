//! Query plans from the `explain` command, as the common plan tree.
//!
//! The winning plan is `queryPlanner.winningPlan` (classic engine) or its
//! `queryPlan` (slot-based engine); with `executionStats` the measured
//! `executionStages` tree is shown instead. Aggregations that are not pushed
//! down entirely list their stages, the first holding the `$cursor` plan.

use dq_core::DbResult;
use dq_core::explain::{ExplainResult, PlanNode};
use mongodb::bson::{Bson, Document, doc};

use crate::commands::Plan;
use crate::literal::compact;
use crate::session::{MongoSession, command};

/// Fields shown as the node's own columns, not as properties.
const SKIP: &[&str] = &[
    "stage",
    "inputStage",
    "inputStages",
    "queryPlan",
    "nReturned",
    "executionTimeMillisEstimate",
    "executionTimeMillis",
    "indexName",
    "planNodeId",
    "slots",
    "shards",
];

fn number(v: Option<&Bson>) -> Option<f64> {
    match v? {
        Bson::Int32(n) => Some(f64::from(*n)),
        Bson::Int64(n) => Some(*n as f64),
        Bson::Double(n) => Some(*n),
        _ => None,
    }
}

fn text(v: &Bson) -> String {
    match v {
        Bson::String(s) => s.clone(),
        other => compact(other),
    }
}

/// One plan stage and its inputs.
fn stage(d: &Document, namespace: Option<&str>) -> PlanNode {
    if let Ok(inner) = d.get_document("queryPlan") {
        return stage(inner, namespace);
    }
    let label = d.get_str("stage").unwrap_or("STAGE").to_owned();
    let detail = match (d.get_str("indexName").ok(), label.as_str()) {
        (Some(index), _) => Some(match namespace {
            Some(ns) => format!("{ns} using {index}"),
            None => format!("using {index}"),
        }),
        (None, "COLLSCAN" | "collscan" | "scan") => namespace.map(str::to_owned),
        _ => None,
    };
    let mut children: Vec<PlanNode> = Vec::new();
    if let Ok(input) = d.get_document("inputStage") {
        children.push(stage(input, namespace));
    }
    if let Ok(inputs) = d.get_array("inputStages") {
        children.extend(inputs.iter().filter_map(Bson::as_document).map(|i| stage(i, namespace)));
    }
    if let Ok(shards) = d.get_array("shards") {
        for shard in shards.iter().filter_map(Bson::as_document) {
            let mut node = shard
                .get_document("winningPlan")
                .or_else(|_| shard.get_document("executionStages"))
                .map(|p| stage(p, namespace))
                .unwrap_or_default();
            node.detail = shard.get_str("shardName").ok().map(str::to_owned).or(node.detail);
            children.push(node);
        }
    }
    PlanNode {
        label,
        detail,
        cost: None,
        rows: None,
        actual_ms: number(d.get("executionTimeMillisEstimate")),
        actual_rows: number(d.get("nReturned")),
        loops: None,
        props: d.iter().filter(|(k, _)| !SKIP.contains(&k.as_str())).map(|(k, v)| (k.clone(), text(v))).collect(),
        children,
    }
}

/// The plan of one `queryPlanner` / `executionStats` reply.
fn planner_tree(reply: &Document, analyze: bool) -> Option<PlanNode> {
    let planner = reply.get_document("queryPlanner").ok()?;
    let namespace = planner.get_str("namespace").ok();
    let measured = reply.get_document("executionStats").ok().and_then(|s| s.get_document("executionStages").ok());
    match (analyze, measured) {
        (true, Some(stages)) => Some(stage(stages, namespace)),
        _ => planner.get_document("winningPlan").ok().map(|w| stage(w, namespace)),
    }
}

/// The plan tree of an explain reply.
pub fn tree(reply: &Document, analyze: bool) -> Option<PlanNode> {
    if let Some(plan) = planner_tree(reply, analyze) {
        return Some(plan);
    }
    let stages = reply.get_array("stages").ok()?;
    let children = stages
        .iter()
        .filter_map(Bson::as_document)
        .map(|s| {
            let Some((name, body)) = s.iter().next() else { return PlanNode::default() };
            match body.as_document() {
                Some(inner) if name == "$cursor" => planner_tree(inner, analyze).unwrap_or_default(),
                _ => PlanNode {
                    label: name.clone(),
                    detail: None,
                    actual_rows: number(s.get("nReturned")),
                    actual_ms: number(s.get("executionTimeMillisEstimate")),
                    props: vec![("spec".into(), text(body))],
                    ..PlanNode::default()
                },
            }
        })
        .collect();
    Some(PlanNode { label: "aggregate".into(), children, ..PlanNode::default() })
}

pub async fn run(session: &MongoSession, plan: &Plan, analyze: bool, timeout_secs: u32) -> DbResult<ExplainResult> {
    let mut cmd = doc! {
        "explain": plan.command.clone(),
        "verbosity": if analyze { "executionStats" } else { "queryPlanner" },
    };
    if timeout_secs > 0 {
        cmd.insert("maxTimeMS", i64::from(timeout_secs) * 1000);
    }
    let reply = command(&session.client, &plan.database, cmd, None).await?;
    let execution_ms = reply.get_document("executionStats").ok().and_then(|s| number(s.get("executionTimeMillis")));
    let raw = serde_json::to_string_pretty(&Bson::Document(reply.clone()).into_relaxed_extjson()).unwrap_or_default();
    Ok(ExplainResult { plan: tree(&reply, analyze), raw, analyzed: analyze, planning_ms: None, execution_ms })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_and_sbe_plans() {
        let reply = doc! { "queryPlanner": {
            "namespace": "shop.orders",
            "winningPlan": { "queryPlan": { "stage": "FETCH", "inputStage": {
                "stage": "IXSCAN", "indexName": "status_created", "keyPattern": { "status": 1 }, "direction": "forward" } } },
        } };
        let plan = tree(&reply, false).unwrap();
        assert_eq!(plan.label, "FETCH");
        assert_eq!(plan.children[0].label, "IXSCAN");
        assert_eq!(plan.children[0].detail.as_deref(), Some("shop.orders using status_created"));
        assert!(plan.children[0].props.iter().any(|(k, v)| k == "keyPattern" && v == "{ status: 1 }"));
    }

    #[test]
    fn measured_and_aggregate_plans() {
        let reply = doc! {
            "queryPlanner": { "namespace": "a.b", "winningPlan": { "stage": "COLLSCAN" } },
            "executionStats": { "executionTimeMillis": 3, "executionStages": { "stage": "COLLSCAN", "nReturned": 7, "executionTimeMillisEstimate": 1 } },
        };
        let plan = tree(&reply, true).unwrap();
        assert_eq!((plan.actual_rows, plan.detail.as_deref()), (Some(7.0), Some("a.b")));
        let agg = doc! { "stages": [
            { "$cursor": { "queryPlanner": { "namespace": "a.b", "winningPlan": { "stage": "COLLSCAN" } } } },
            { "$group": { "_id": "$x" }, "nReturned": 2_i64 },
        ] };
        let plan = tree(&agg, false).unwrap();
        assert_eq!(plan.children.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["COLLSCAN", "$group"]);
    }
}
