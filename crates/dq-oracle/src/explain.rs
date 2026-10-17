//! Query plans. `EXPLAIN PLAN` fills `PLAN_TABLE`, read back into the common
//! tree. Analyze runs the statement with `STATISTICS_LEVEL = ALL` and shows
//! `DBMS_XPLAN.DISPLAY_CURSOR` (needs access to the `V$SQL_PLAN` views).

use std::time::{SystemTime, UNIX_EPOCH};

use dq_core::explain::{ExplainResult, PlanNode};
use dq_core::{DbError, DbResult};
use oracle::Connection;

use crate::error::map_error;
use crate::rows::{TextRow, prepare, text_rows};

/// Builds the operator tree from `PLAN_TABLE` rows ordered by id.
fn tree(rows: &[TextRow]) -> Option<PlanNode> {
    // (id, parent id, node)
    let mut nodes: Vec<(i64, Option<i64>, PlanNode)> = rows
        .iter()
        .map(|r| {
            let label = match r.text(3) {
                Some(options) => format!("{} {}", r.string(2), options),
                None => r.string(2),
            };
            let detail = r.text(5).map(|name| match r.text(4) {
                Some(owner) => format!("{owner}.{name}"),
                None => name,
            });
            let mut props = Vec::new();
            for (key, i) in [("Access", 9), ("Filter", 10), ("Bytes", 8)] {
                if let Some(v) = r.text(i) {
                    props.push((key.to_owned(), v));
                }
            }
            let node = PlanNode {
                label,
                detail,
                cost: r.text(6).and_then(|c| c.parse().ok()),
                rows: r.text(7).and_then(|c| c.parse().ok()),
                props,
                ..PlanNode::default()
            };
            (r.number(0).unwrap_or(0), r.number(1), node)
        })
        .collect();
    // Children have larger ids than their parent: attach from the last node up.
    while nodes.len() > 1 {
        let (_, parent, node) = nodes.pop().expect("more than one node");
        match parent.and_then(|p| nodes.iter_mut().find(|(id, ..)| *id == p)) {
            Some((.., parent_node)) => parent_node.children.insert(0, node),
            None => break,
        }
    }
    nodes.pop().map(|(.., node)| node)
}

pub fn explain(
    conn: &Connection,
    sql: &str,
    params: &[Option<String>],
    analyze: bool,
    secret: Option<&str>,
) -> DbResult<ExplainResult> {
    let err = |e: oracle::Error| map_error(&e, secret);
    if analyze {
        conn.execute("ALTER SESSION SET STATISTICS_LEVEL = ALL", &[]).map_err(err)?;
        let run = (|| {
            let started = std::time::Instant::now();
            let rows =
                prepare(conn, sql, params, None).and_then(|s| s.into_result_set::<oracle::Row>(&[])).map_err(err)?;
            for row in rows {
                row.map_err(err)?;
            }
            let execution_ms = started.elapsed().as_secs_f64() * 1000.0;
            let lines = text_rows(
                conn,
                "SELECT plan_table_output FROM TABLE(DBMS_XPLAN.DISPLAY_CURSOR(NULL, NULL, 'ALLSTATS LAST'))",
                &[],
            )?;
            let raw = lines.iter().map(|l| l.string(0)).collect::<Vec<_>>().join("\n");
            Ok(ExplainResult { plan: None, raw, analyzed: true, planning_ms: None, execution_ms: Some(execution_ms) })
        })();
        let _ = conn.execute("ALTER SESSION SET STATISTICS_LEVEL = TYPICAL", &[]);
        let _ = conn.rollback();
        return run;
    }
    let id =
        format!("DQ{}", SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos()) % 1_000_000_000_000);
    // Bind variables stay unbound: EXPLAIN PLAN plans the statement without values.
    conn.execute(&format!("EXPLAIN PLAN SET STATEMENT_ID = '{id}' FOR {sql}"), &[]).map_err(err)?;
    let rows = text_rows(
        conn,
        "SELECT id, parent_id, operation, options, object_owner, object_name, cost, cardinality, bytes, \
                access_predicates, filter_predicates FROM plan_table WHERE statement_id = :1 ORDER BY id",
        &[&id],
    );
    let lines =
        text_rows(conn, "SELECT plan_table_output FROM TABLE(DBMS_XPLAN.DISPLAY('PLAN_TABLE', :1, 'TYPICAL'))", &[&id]);
    let _ = conn.execute("DELETE FROM plan_table WHERE statement_id = :1", &[&id]);
    let _ = conn.commit();
    let rows = rows?;
    let raw = lines.map(|l| l.iter().map(|r| r.string(0)).collect::<Vec<_>>().join("\n")).unwrap_or_default();
    let plan = tree(&rows).ok_or_else(|| DbError::internal("the server returned no plan"))?;
    Ok(ExplainResult { plan: Some(plan), raw, analyzed: false, planning_ms: None, execution_ms: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cols: &[Option<&str>]) -> TextRow {
        TextRow(cols.iter().map(|c| c.map(str::to_owned)).collect())
    }

    #[test]
    fn builds_the_operator_tree() {
        let rows = [
            row(&[
                Some("0"),
                None,
                Some("SELECT STATEMENT"),
                None,
                None,
                None,
                Some("5"),
                Some("400"),
                None,
                None,
                None,
            ]),
            row(&[
                Some("1"),
                Some("0"),
                Some("SORT"),
                Some("ORDER BY"),
                None,
                None,
                Some("5"),
                Some("400"),
                None,
                None,
                None,
            ]),
            row(&[
                Some("2"),
                Some("1"),
                Some("TABLE ACCESS"),
                Some("FULL"),
                Some("APP"),
                Some("ORDERS"),
                Some("4"),
                Some("400"),
                None,
                None,
                Some("\"STATUS\"='paid'"),
            ]),
        ];
        let root = tree(&rows).unwrap();
        assert_eq!(root.label, "SELECT STATEMENT");
        let scan = &root.children[0].children[0];
        assert_eq!(
            (scan.label.as_str(), scan.detail.as_deref(), scan.rows),
            ("TABLE ACCESS FULL", Some("APP.ORDERS"), Some(400.0))
        );
        assert!(scan.props.iter().any(|(k, _)| k == "Filter"));
    }
}
