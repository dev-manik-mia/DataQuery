//! Reads SQL Server showplan XML into the common plan tree. Each `RelOp`
//! element is an operator; `RunTimeCountersPerThread` carries what
//! `SET STATISTICS XML` measured.

use dq_core::explain::{ExplainResult, PlanNode};
use dq_core::{DbError, DbResult};
use quick_xml::{Reader, XmlVersion};
use quick_xml::events::{BytesStart, Event};

fn attributes(e: &BytesStart<'_>) -> Vec<(String, String)> {
    e.attributes()
        .flatten()
        .map(|a| {
            let key = a.key.as_ref().to_owned();
            let value = a.normalized_value(XmlVersion::Implicit1_0).map(|v| v.into_owned()).unwrap_or_default();
            (key, value)
        })
        .collect()
}

fn get<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn num(attrs: &[(String, String)], key: &str) -> Option<f64> {
    get(attrs, key).and_then(|v| v.parse().ok())
}

/// Properties shown for an operator; the rest are in the raw plan.
const OPERATOR_PROPS: &[&str] = &["EstimateIO", "EstimateCPU", "Parallel", "EstimatedExecutionMode", "AvgRowSize"];

pub fn parse(raw: String, analyzed: bool) -> DbResult<ExplainResult> {
    let mut reader = Reader::from_str(&raw);
    reader.config_mut().trim_text(true);
    let mut elements: Vec<String> = Vec::new();
    let mut stack: Vec<PlanNode> = Vec::new();
    let mut roots: Vec<PlanNode> = Vec::new();
    let mut statements: Vec<PlanNode> = Vec::new();
    let mut elapsed_ms: Option<f64> = None;
    loop {
        let event = reader.read_event().map_err(|e| DbError::internal(format!("unreadable plan: {e}")))?;
        let (start, empty) = match &event {
            Event::Start(e) => (Some(e), false),
            Event::Empty(e) => (Some(e), true),
            Event::End(_) => {
                let name = elements.pop().unwrap_or_default();
                if name == "RelOp" {
                    let node = stack.pop().expect("RelOp start pushed a node");
                    match stack.last_mut() {
                        Some(parent) => parent.children.push(node),
                        None => roots.push(node),
                    }
                } else if name == "StmtSimple" {
                    if let Some(statement) = statements.last_mut() {
                        statement.children = std::mem::take(&mut roots);
                    }
                }
                continue;
            }
            Event::Eof => break,
            _ => continue,
        };
        let Some(e) = start else { continue };
        let name = e.local_name().as_ref().to_owned();
        let attrs = attributes(e);
        match name.as_str() {
            "StmtSimple" => statements.push(PlanNode {
                label: get(&attrs, "StatementType").unwrap_or("statement").to_owned(),
                detail: get(&attrs, "StatementText").map(|t| t.trim().to_owned()),
                cost: num(&attrs, "StatementSubTreeCost"),
                rows: num(&attrs, "StatementEstRows"),
                ..PlanNode::default()
            }),
            "RelOp" => stack.push(PlanNode {
                label: get(&attrs, "PhysicalOp").unwrap_or("Operator").to_owned(),
                cost: num(&attrs, "EstimatedTotalSubtreeCost"),
                rows: num(&attrs, "EstimateRows"),
                props: get(&attrs, "LogicalOp")
                    .filter(|l| Some(*l) != get(&attrs, "PhysicalOp"))
                    .map(|l| ("Logical Op".to_owned(), l.to_owned()))
                    .into_iter()
                    .chain(OPERATOR_PROPS.iter().filter_map(|k| get(&attrs, k).map(|v| ((*k).to_owned(), v.to_owned()))))
                    .collect(),
                ..PlanNode::default()
            }),
            "Object" => {
                if let Some(node) = stack.last_mut().filter(|n| n.detail.is_none()) {
                    let clean = |k: &str| get(&attrs, k).map(|v| v.trim_matches(['[', ']']).to_owned());
                    let table = [clean("Schema"), clean("Table")].into_iter().flatten().collect::<Vec<_>>().join(".");
                    node.detail = Some(match clean("Index") {
                        Some(index) if !table.is_empty() => format!("{table} using {index}"),
                        Some(index) => index,
                        None => table,
                    })
                    .filter(|d| !d.is_empty());
                }
            }
            "RunTimeCountersPerThread" => {
                if let Some(node) = stack.last_mut() {
                    let add = |slot: &mut Option<f64>, v: Option<f64>| {
                        if let Some(v) = v {
                            *slot = Some(slot.unwrap_or(0.0) + v);
                        }
                    };
                    add(&mut node.actual_rows, num(&attrs, "ActualRows"));
                    add(&mut node.loops, num(&attrs, "ActualExecutions"));
                    if let Some(ms) = num(&attrs, "ActualElapsedms") {
                        node.actual_ms = Some(node.actual_ms.map_or(ms, |m: f64| m.max(ms)));
                    }
                }
            }
            "QueryTimeStats" => elapsed_ms = num(&attrs, "ElapsedTime"),
            "ScalarOperator" if elements.last().is_some_and(|p| p.ends_with("Predicate")) => {
                if let (Some(node), Some(text)) = (stack.last_mut(), get(&attrs, "ScalarString")) {
                    let key = elements.last().map_or("Predicate", String::as_str);
                    node.props.push((key.to_owned(), text.to_owned()));
                }
            }
            _ => {}
        }
        if !empty {
            elements.push(name);
        }
    }
    let plan = match statements.len() {
        0 => roots.pop(),
        1 => {
            let mut statement = statements.pop().expect("one statement");
            // A single statement reads best as its top operator.
            if statement.children.len() == 1 {
                let mut top = statement.children.pop().expect("one child");
                top.props.insert(0, ("Statement".into(), statement.detail.unwrap_or_default()));
                Some(top)
            } else {
                Some(statement)
            }
        }
        _ => Some(PlanNode { label: "batch".into(), children: statements, ..PlanNode::default() }),
    };
    Ok(ExplainResult { plan, raw, analyzed, planning_ms: None, execution_ms: elapsed_ms })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = r#"<?xml version="1.0" encoding="utf-16"?>
<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan" Version="1.6">
 <BatchSequence><Batch><Statements>
  <StmtSimple StatementText="SELECT * FROM dbo.orders WHERE total &gt; 5" StatementType="SELECT" StatementSubTreeCost="0.02" StatementEstRows="400">
   <QueryPlan>
    <QueryTimeStats ElapsedTime="3" CpuTime="2"/>
    <RelOp NodeId="0" PhysicalOp="Clustered Index Scan" LogicalOp="Clustered Index Scan" EstimateRows="400" EstimatedTotalSubtreeCost="0.02">
     <RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="390" ActualExecutions="1" ActualElapsedms="1"/></RunTimeInformation>
     <IndexScan>
      <Object Database="[app]" Schema="[dbo]" Table="[orders]" Index="[orders_pkey]"/>
      <Predicate><ScalarOperator ScalarString="[app].[dbo].[orders].[total]&gt;(5.00)"/></Predicate>
     </IndexScan>
    </RelOp>
   </QueryPlan>
  </StmtSimple>
 </Statements></Batch></BatchSequence>
</ShowPlanXML>"#;

    #[test]
    fn reads_operators_and_runtime_counters() {
        let r = parse(PLAN.into(), true).unwrap();
        let top = r.plan.unwrap();
        assert_eq!(top.label, "Clustered Index Scan");
        assert_eq!(top.detail.as_deref(), Some("dbo.orders using orders_pkey"));
        assert_eq!((top.rows, top.actual_rows, top.loops), (Some(400.0), Some(390.0), Some(1.0)));
        assert!(top.props.iter().any(|(k, v)| k == "Predicate" && v.contains("total")));
        assert!(top.props.iter().any(|(k, v)| k == "Statement" && v.starts_with("SELECT")));
        assert_eq!(r.execution_ms, Some(3.0));
    }
}
