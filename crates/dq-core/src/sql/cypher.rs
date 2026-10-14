//! Cypher (Neo4j): what a statement does, from its clauses.

use super::analyze::{DestructiveReason, StatementKind};
use super::lexer::{Token, TokenKind};

/// Procedures that only read (`CALL db.labels()` and friends).
const READ_PROCEDURES: &[&str] = &[
    "db.labels",
    "db.relationshiptypes",
    "db.propertykeys",
    "db.schema.visualization",
    "db.schema.nodetypeproperties",
    "db.schema.reltypeproperties",
    "db.indexes",
    "db.constraints",
    "dbms.components",
    "db.info",
    "db.ping",
    "dbms.listconfig",
    "dbms.showcurrentuser",
    "db.stats.retrieve",
];

pub(super) fn classify(src: &str, sig: &[Token]) -> (StatementKind, bool, Option<DestructiveReason>) {
    let words: Vec<String> =
        sig.iter().filter(|t| t.kind == TokenKind::Word).map(|t| t.text(src).to_ascii_uppercase()).collect();
    let head = words.first().map_or("", String::as_str);
    let has = |w: &str| words.iter().any(|x| x == w);
    match head {
        "BEGIN" | "COMMIT" | "ROLLBACK" => return (StatementKind::Transaction, false, None),
        "SHOW" => return (StatementKind::Show, false, None),
        "EXPLAIN" => return (StatementKind::Explain, false, None),
        "USE" if words.len() <= 2 => return (StatementKind::Use, false, None),
        "DROP" => return (StatementKind::Drop, true, Some(DestructiveReason::Drop)),
        "STOP" | "TERMINATE" | "DEALLOCATE" | "REALLOCATE" => {
            return (StatementKind::Other, true, Some(DestructiveReason::ServerCommand));
        }
        "CREATE" | "ALTER" | "GRANT" | "DENY" | "REVOKE" | "RENAME" | "START" | "ENABLE"
            if words.get(1).is_some_and(|w| {
                matches!(
                    w.as_str(),
                    "DATABASE"
                        | "COMPOSITE"
                        | "ALIAS"
                        | "USER"
                        | "ROLE"
                        | "INDEX"
                        | "CONSTRAINT"
                        | "OR"
                        | "SERVER"
                        | "RANGE"
                        | "TEXT"
                        | "POINT"
                        | "LOOKUP"
                        | "FULLTEXT"
                        | "VECTOR"
                        | "CURRENT"
                )
            }) || head == "GRANT"
                || head == "DENY"
                || head == "REVOKE" =>
        {
            return (StatementKind::Create, true, None);
        }
        _ => {}
    }
    let writes_data = ["CREATE", "MERGE", "SET", "DELETE", "REMOVE", "FOREACH"].iter().any(|w| has(w));
    // `CALL proc()` writes unless it is a known read procedure; `CALL { … }` is judged by its clauses.
    let calls_writer = sig.iter().enumerate().any(|(i, t)| {
        t.is_keyword(src, "CALL")
            && sig.get(i + 1).is_some_and(|n| n.kind == TokenKind::Word && !is_read_procedure(src, &sig[i + 1..]))
    });
    let writes = writes_data || calls_writer;
    let kind = if has("DELETE") {
        StatementKind::Delete
    } else if has("CREATE") || has("MERGE") {
        StatementKind::Insert
    } else if has("SET") || has("REMOVE") {
        StatementKind::Update
    } else if writes {
        StatementKind::Call
    } else {
        StatementKind::Select
    };
    // `MATCH (n) DETACH DELETE n` with no WHERE and no property filter removes everything it matched.
    let unbounded_delete = has("DELETE") && !has("WHERE") && !sig.iter().any(|t| t.text(src) == "{");
    (kind, writes, unbounded_delete.then_some(DestructiveReason::DeleteAllDocuments))
}

/// `db.labels`: the dotted procedure name starting at `name` is a known read-only procedure.
fn is_read_procedure(src: &str, name_tokens: &[Token]) -> bool {
    let mut name = String::new();
    for t in name_tokens {
        match t.kind {
            TokenKind::Word => name.push_str(&t.text(src).to_ascii_lowercase()),
            TokenKind::Punct if t.text(src) == "." => name.push('.'),
            _ => break,
        }
    }
    READ_PROCEDURES.contains(&name.as_str())
}

#[cfg(test)]
mod tests {
    use crate::sql::{DestructiveReason as D, SqlFlavor, StatementKind as K, split_statements};

    fn one(s: &str) -> (K, bool, Option<D>) {
        let i = split_statements(s, SqlFlavor::Cypher).remove(0);
        (i.kind, i.writes, i.destructive)
    }

    #[test]
    fn classifies_cypher() {
        assert_eq!(one("MATCH (p:Person {name: 'Ann'})-[:KNOWS]->(f) RETURN f.name"), (K::Select, false, None));
        assert!(one("CREATE (n:Person {name: $name})").1);
        assert_eq!(one("MATCH (n) DETACH DELETE n"), (K::Delete, true, Some(D::DeleteAllDocuments)));
        assert_eq!(one("MATCH (n:Tmp) WHERE n.age > 3 DELETE n").2, None);
        assert_eq!(one("DROP INDEX person_name").2, Some(D::Drop));
        assert!(!one("SHOW TRANSACTIONS").1);
        assert!(!one("CALL db.labels()").1);
        assert!(one("CALL apoc.periodic.iterate('a', 'b', {})").1);
        assert_eq!(one("CREATE INDEX FOR (p:Person) ON (p.name)").0, K::Create);
        assert_eq!(one("TERMINATE TRANSACTIONS 'neo4j-transaction-3'").2, Some(D::ServerCommand));
        assert!(!one("EXPLAIN MATCH (n) RETURN n").1);
        assert_eq!(one("BEGIN").0, K::Transaction);
        let s = split_statements("MATCH (n) RETURN n; // c;\nRETURN 'a;b';", SqlFlavor::Cypher);
        assert_eq!(s.len(), 2);
        assert_eq!(split_statements("MATCH (n {id: $id}) RETURN n", SqlFlavor::Cypher)[0].param_count, 1);
    }
}
