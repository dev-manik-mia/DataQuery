//! Builds a plan tree from `EXPLAIN QUERY PLAN` rows: `(id, parent, notused, detail)`.

use dq_core::DbError;
use dq_core::explain::{ExplainResult, PlanNode};

/// `EXPLAIN QUERY PLAN` for one statement.
pub fn statement(sql: &str) -> String {
    format!("EXPLAIN QUERY PLAN {}", sql.trim().trim_end_matches(';'))
}

/// `SQLite` only plans: there is no `EXPLAIN ANALYZE`.
pub fn no_analyze() -> DbError {
    DbError::unsupported("SQLite has no EXPLAIN ANALYZE. Run the query to see its time, or use Explain for the plan.")
}

/// `detail` such as `SEARCH orders USING INDEX by_status (status=?)` becomes
/// the label `SEARCH` and the detail `orders USING INDEX by_status (status=?)`.
fn node(detail: &str) -> PlanNode {
    let (label, rest) = detail.split_once(' ').unwrap_or((detail, ""));
    let upper = |s: &str| s.chars().all(|c| c.is_ascii_uppercase() || c == '-');
    let (label, rest) = if upper(label) { (label.to_owned(), rest.trim()) } else { (detail.to_owned(), "") };
    PlanNode { label, detail: (!rest.is_empty()).then(|| rest.to_owned()), ..PlanNode::default() }
}

fn attach(nodes: &mut Vec<(i64, i64, PlanNode)>, parent: i64) -> Vec<PlanNode> {
    let mine: Vec<(i64, PlanNode)> = {
        let mut out = Vec::new();
        let mut i = 0;
        while i < nodes.len() {
            if nodes[i].1 == parent {
                let (id, _, n) = nodes.remove(i);
                out.push((id, n));
            } else {
                i += 1;
            }
        }
        out
    };
    mine.into_iter()
        .map(|(id, mut n)| {
            n.children = attach(nodes, id);
            n
        })
        .collect()
}

/// `rows` are `(id, parent, detail)` in the order `SQLite` returned them.
pub fn plan(rows: Vec<(i64, i64, String)>) -> ExplainResult {
    let raw = rows
        .iter()
        .map(|(_, parent, detail)| {
            let depth = std::iter::successors(Some(*parent), |p| rows.iter().find(|r| r.0 == *p).map(|r| r.1))
                .take_while(|p| *p != 0)
                .count();
            format!("{}{detail}", "  ".repeat(depth))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut nodes: Vec<(i64, i64, PlanNode)> = rows.into_iter().map(|(id, parent, d)| (id, parent, node(&d))).collect();
    let mut roots = attach(&mut nodes, 0);
    let root = if roots.len() == 1 {
        roots.remove(0)
    } else {
        PlanNode { label: "QUERY PLAN".into(), children: roots, ..PlanNode::default() }
    };
    ExplainResult { plan: Some(root), raw, analyzed: false, planning_ms: None, execution_ms: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_tree() {
        let r = plan(vec![
            (2, 0, "SCAN o".into()),
            (5, 0, "SEARCH c USING INTEGER PRIMARY KEY (rowid=?)".into()),
            (9, 0, "USE TEMP B-TREE FOR ORDER BY".into()),
            (12, 5, "CORRELATED SCALAR SUBQUERY 1".into()),
        ]);
        let root = r.plan.unwrap();
        assert_eq!(root.label, "QUERY PLAN");
        assert_eq!(root.children.len(), 3);
        assert_eq!(
            (root.children[1].label.as_str(), root.children[1].detail.as_deref()),
            ("SEARCH", Some("c USING INTEGER PRIMARY KEY (rowid=?)"))
        );
        assert_eq!(root.children[1].children[0].label, "CORRELATED");
        assert!(r.raw.contains("\n  CORRELATED SCALAR SUBQUERY 1"));
    }
}
