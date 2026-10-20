//! Plans from job statistics. Explain is a dry run: `BigQuery` validates the
//! statement and estimates the bytes it would process, without running it.
//! Explain Analyze runs the query and reads its stages (`queryPlan`), each
//! linked to the stages it reads from.

use dq_core::explain::{ExplainResult, PlanNode};
use serde_json::Value;

/// `1536` → `1.5 KiB`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

fn number(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_u64()))
}

/// The result notice of a finished job: statement type, bytes processed and billed, cache use.
pub fn notice(statistics: &Value) -> Option<String> {
    let query = statistics.get("query")?;
    let mut parts = Vec::new();
    if let Some(processed) = number(query, "totalBytesProcessed").or_else(|| number(statistics, "totalBytesProcessed"))
    {
        parts.push(format!("{} processed", human_bytes(processed)));
    }
    if let Some(billed) = number(query, "totalBytesBilled") {
        parts.push(format!("{} billed", human_bytes(billed)));
    }
    if query.get("cacheHit").and_then(Value::as_bool) == Some(true) {
        parts.push("from cache".into());
    }
    if let Some(slot_ms) = number(query, "totalSlotMs") {
        parts.push(format!("{:.1} slot seconds", slot_ms as f64 / 1000.0));
    }
    (!parts.is_empty()).then(|| format!("BigQuery: {}.", parts.join(" · ")))
}

fn scalar(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn table_name(t: &Value) -> String {
    ["projectId", "datasetId", "tableId"].iter().filter_map(|k| t[*k].as_str()).collect::<Vec<_>>().join(".")
}

/// A dry run's estimate: the statement type, bytes it would process, tables it reads.
pub fn dry_run(job: &Value) -> ExplainResult {
    let statistics = &job["statistics"];
    let query = &statistics["query"];
    let mut plan = PlanNode {
        label: format!("{} (dry run)", query["statementType"].as_str().unwrap_or("QUERY")),
        detail: notice(statistics),
        ..PlanNode::default()
    };
    if let Some(bytes) = number(query, "totalBytesProcessed") {
        plan.props.push(("Bytes processed (estimate)".into(), format!("{bytes} ({})", human_bytes(bytes))));
    }
    for key in ["totalBytesProcessedAccuracy", "statementType"] {
        if let Some(v) = query.get(key) {
            plan.props.push((key.into(), scalar(v)));
        }
    }
    let tables = query["referencedTables"].as_array().map(Vec::as_slice).unwrap_or_default();
    plan.children = tables
        .iter()
        .map(|t| PlanNode { label: "Read table".into(), detail: Some(table_name(t)), ..PlanNode::default() })
        .collect();
    ExplainResult {
        raw: serde_json::to_string_pretty(statistics).unwrap_or_default(),
        plan: Some(plan),
        analyzed: false,
        planning_ms: None,
        execution_ms: None,
    }
}

fn stage_node(stage: &Value) -> PlanNode {
    let steps = stage["steps"].as_array().map(Vec::as_slice).unwrap_or_default();
    let detail = steps
        .iter()
        .map(|s| {
            let substeps = s["substeps"].as_array().map(Vec::as_slice).unwrap_or_default();
            let first = substeps.first().map(scalar).unwrap_or_default();
            format!("{} {first}", s["kind"].as_str().unwrap_or_default()).trim().to_owned()
        })
        .collect::<Vec<_>>()
        .join(" → ");
    let props = [
        "status",
        "recordsRead",
        "recordsWritten",
        "shuffleOutputBytes",
        "completedParallelInputs",
        "slotMs",
        "waitMsAvg",
        "readMsAvg",
        "computeMsAvg",
        "writeMsAvg",
    ]
    .iter()
    .filter_map(|k| stage.get(*k).map(|v| ((*k).to_owned(), scalar(v))))
    .collect();
    PlanNode {
        label: stage["name"].as_str().unwrap_or("stage").to_owned(),
        detail: (!detail.is_empty()).then_some(detail),
        actual_rows: number(stage, "recordsWritten").map(|n| n as f64),
        actual_ms: match (number(stage, "startMs"), number(stage, "endMs")) {
            (Some(start), Some(end)) => Some(end.saturating_sub(start) as f64),
            _ => None,
        },
        props,
        ..PlanNode::default()
    }
}

/// The stage `id` with the stages it reads from.
fn build(id: &str, stages: &[Value], depth: usize) -> Option<PlanNode> {
    let stage = stages.iter().find(|s| scalar(&s["id"]) == id)?;
    let mut node = stage_node(stage);
    if depth < 100 {
        for input in stage["inputStages"].as_array().map(Vec::as_slice).unwrap_or_default() {
            node.children.extend(build(&scalar(input), stages, depth + 1));
        }
    }
    Some(node)
}

/// The executed plan of a finished job: the output stage with its inputs.
pub fn analyzed(job: &Value) -> ExplainResult {
    let statistics = &job["statistics"];
    let stages = statistics["query"]["queryPlan"].as_array().map(Vec::as_slice).unwrap_or_default();
    // The last stage writes the output; every other stage feeds it.
    let plan = stages.last().map(|last| build(&scalar(&last["id"]), stages, 0)).unwrap_or_default();
    let elapsed = match (number(statistics, "startTime"), number(statistics, "endTime")) {
        (Some(start), Some(end)) => Some(end.saturating_sub(start) as f64),
        _ => None,
    };
    let mut plan = plan.unwrap_or_else(|| PlanNode {
        label: "Query".into(),
        detail: Some("BigQuery returned no stages for this job.".into()),
        ..PlanNode::default()
    });
    if let Some(n) = notice(statistics) {
        plan.props.insert(0, ("job".into(), n));
    }
    ExplainResult {
        raw: serde_json::to_string_pretty(statistics).unwrap_or_default(),
        plan: Some(plan),
        analyzed: true,
        planning_ms: None,
        execution_ms: elapsed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_dry_runs_and_stage_plans() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(10 * 1024 * 1024), "10.0 MiB");
        let dry = json!({ "statistics": { "query": { "statementType": "SELECT", "totalBytesProcessed": "2048",
            "referencedTables": [{ "projectId": "p", "datasetId": "d", "tableId": "t" }] } } });
        let plan = dry_run(&dry).plan.unwrap();
        assert_eq!(plan.label, "SELECT (dry run)");
        assert_eq!(plan.children[0].detail.as_deref(), Some("p.d.t"));
        assert_eq!(plan.detail.as_deref(), Some("BigQuery: 2.0 KiB processed."));
        let job = json!({ "statistics": { "startTime": "1000", "endTime": "1600", "query": {
            "totalBytesProcessed": "100", "totalBytesBilled": "10485760", "cacheHit": false,
            "queryPlan": [
                { "name": "S00: Input", "id": "0", "recordsRead": "50", "recordsWritten": "5",
                  "steps": [{ "kind": "READ", "substeps": ["$1:id", "FROM d.t"] }] },
                { "name": "S01: Output", "id": "1", "inputStages": ["0"], "recordsRead": "5", "recordsWritten": "5" } ] } } });
        let result = analyzed(&job);
        let output = result.plan.unwrap();
        assert_eq!(output.label, "S01: Output");
        assert_eq!(output.children[0].label, "S00: Input");
        assert_eq!(output.children[0].detail.as_deref(), Some("READ $1:id"));
        assert_eq!(result.execution_ms, Some(600.0));
        assert_eq!(notice(&job["statistics"]).as_deref(), Some("BigQuery: 100 B processed · 10.0 MiB billed."));
    }
}
