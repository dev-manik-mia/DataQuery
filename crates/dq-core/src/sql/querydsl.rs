//! Elasticsearch / OpenSearch requests in Kibana Dev Tools form:
//!
//! ```text
//! GET /orders/_search
//! { "query": { "match": { "status": "paid" } } }
//! ```
//!
//! A statement starts at a line beginning with an HTTP method and runs until the
//! next one. Read-only requests are `GET`/`HEAD` and `POST` to search-style
//! endpoints; deleting indices or every document is destructive.

use super::analyze::{DestructiveReason, StatementKind};
use crate::error::{DbError, DbResult};

pub const METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "HEAD", "PATCH"];

/// One parsed request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    /// Path with a leading `/`, query string included.
    pub path: String,
    /// JSON body (or NDJSON for `_bulk` / `_msearch`), trimmed; empty when none.
    pub body: String,
}

impl Request {
    /// Path segments without the query string.
    pub fn segments(&self) -> Vec<&str> {
        self.path.split('?').next().unwrap_or("").split('/').filter(|s| !s.is_empty()).collect()
    }
}

/// True when `line` starts a request (`GET /…`, `post _search`).
pub fn is_request_line(line: &str) -> bool {
    let mut words = line.split_whitespace();
    words.next().is_some_and(|m| METHODS.iter().any(|x| x.eq_ignore_ascii_case(m))) && words.next().is_some()
}

/// Parses one statement. `//` and `#` comment lines are ignored.
pub fn parse_request(text: &str) -> DbResult<Request> {
    let mut lines = text.lines().filter(|l| {
        let t = l.trim_start();
        !(t.starts_with("//") || t.starts_with('#'))
    });
    let first = lines.by_ref().find(|l| !l.trim().is_empty()).unwrap_or("");
    if !is_request_line(first) {
        return Err(DbError::invalid("Start the request with a method and a path, e.g. GET /orders/_search."));
    }
    let mut words = first.split_whitespace();
    let method = words.next().unwrap_or_default().to_ascii_uppercase();
    let path = words.next().unwrap_or_default();
    if words.next().is_some() {
        return Err(DbError::invalid("Put the request body on the lines after the method and path."));
    }
    let path = if path.starts_with('/') { path.to_owned() } else { format!("/{path}") };
    if path.contains("..") || path.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(DbError::invalid("The path contains characters that are not allowed."));
    }
    let body = lines.collect::<Vec<_>>().join("\n").trim().to_owned();
    Ok(Request { method, path, body })
}

/// Endpoints that only read even when called with `POST`.
const READ_ENDPOINTS: &[&str] = &[
    "_search",
    "_msearch",
    "_count",
    "_mget",
    "_explain",
    "_validate",
    "_field_caps",
    "_sql",
    "_eql",
    "_async_search",
    "_knn_search",
    "_termvectors",
    "_mtermvectors",
    "_rank_eval",
    "_render",
    "_pit",
    "_terms_enum",
    "_analyze",
    "_cat",
    "_cluster",
    "_nodes",
    "_stats",
    "_mapping",
    "_settings",
    "_alias",
    "_aliases",
    "_resolve",
    "_ml",
];

/// What a request does.
pub(super) fn classify(text: &str) -> (StatementKind, bool, Option<DestructiveReason>) {
    let Ok(req) = parse_request(text) else {
        return (StatementKind::Other, true, None);
    };
    let segments = req.segments();
    let endpoint = segments.iter().rev().find(|s| s.starts_with('_')).copied().unwrap_or("");
    let first_endpoint = segments.iter().find(|s| s.starts_with('_')).copied().unwrap_or("");
    match req.method.as_str() {
        "GET" | "HEAD" => (StatementKind::Select, false, None),
        "DELETE" if first_endpoint.is_empty() || (first_endpoint == "_all" && segments.len() == 1) => {
            // `DELETE /index` drops it with every document; `/_all` or wildcards drop many.
            (StatementKind::Drop, true, Some(DestructiveReason::Drop))
        }
        "DELETE" => (StatementKind::Delete, true, None),
        "POST" if endpoint == "_delete_by_query" => {
            let everything = req.body.is_empty() || req.body.contains("match_all");
            (StatementKind::Delete, true, everything.then_some(DestructiveReason::DeleteAllDocuments))
        }
        "POST" if endpoint == "_update_by_query" => {
            let everything = req.body.is_empty() || req.body.contains("match_all");
            (StatementKind::Update, true, everything.then_some(DestructiveReason::UpdateAllDocuments))
        }
        "POST" if matches!(endpoint, "_close" | "_freeze" | "_shrink" | "_split" | "_clone" | "_restore") => {
            (StatementKind::Alter, true, Some(DestructiveReason::ServerCommand))
        }
        "PUT" if first_endpoint == "_cluster" => (StatementKind::Set, true, Some(DestructiveReason::ServerCommand)),
        // Cluster and node actions change state (reroute, voting, secure settings), except explanations.
        "POST" if matches!(first_endpoint, "_cluster" | "_nodes") => {
            let read = req.path.contains("allocation/explain");
            (if read { StatementKind::Select } else { StatementKind::Other }, !read, None)
        }
        "POST" if READ_ENDPOINTS.contains(&first_endpoint) || READ_ENDPOINTS.contains(&endpoint) => {
            // `_cluster/reroute`, `_nodes/…/_reload_secure_settings` and the like change state.
            let changes = matches!(endpoint, "_reroute" | "_reload_secure_settings" | "_close" | "_open");
            (if changes { StatementKind::Other } else { StatementKind::Select }, changes, None)
        }
        "PUT" if endpoint.is_empty() && segments.len() == 1 => (StatementKind::Create, true, None),
        "PUT" | "POST" if matches!(endpoint, "_doc" | "_create" | "_bulk") => (StatementKind::Insert, true, None),
        "POST" if endpoint == "_update" => (StatementKind::Update, true, None),
        _ => (StatementKind::Other, true, None),
    }
}

#[cfg(test)]
mod tests {
    use crate::sql::{DestructiveReason as D, SqlFlavor, StatementKind as K, split_statements};

    fn split(src: &str) -> Vec<(String, K, bool, Option<D>)> {
        split_statements(src, SqlFlavor::QueryDsl)
            .into_iter()
            .map(|s| (s.text(src).to_owned(), s.kind, s.writes, s.destructive))
            .collect()
    }

    #[test]
    fn splits_on_method_lines() {
        let src = "GET /orders/_search\n{\n  \"query\": { \"match\": { \"note\": \"GET /x\" } }\n}\n\n# comment\nPOST orders/_doc\n{ \"a\": 1 }\nDELETE /old";
        let s = split(src);
        assert_eq!(s.len(), 3);
        assert!(s[0].0.ends_with('}'));
        assert_eq!((s[0].1, s[0].2), (K::Select, false));
        assert_eq!((s[1].1, s[1].2), (K::Insert, true));
        assert_eq!((s[2].1, s[2].3), (K::Drop, Some(D::Drop)));
    }

    #[test]
    fn classifies_requests() {
        let one = |s: &str| split(s).remove(0);
        assert!(!one("POST /orders/_search\n{}").2);
        assert!(!one("POST _sql?format=json\n{\"query\": \"SELECT 1\"}").2);
        assert!(!one("GET _cat/indices?v").2);
        assert_eq!(one("POST /o/_delete_by_query\n{\"query\":{\"match_all\":{}}}").3, Some(D::DeleteAllDocuments));
        assert_eq!(one("POST /o/_delete_by_query\n{\"query\":{\"term\":{\"a\":1}}}").3, None);
        assert_eq!(one("DELETE /o/_doc/1").1, K::Delete);
        assert_eq!(one("DELETE /_all").3, Some(D::Drop));
        assert_eq!(one("PUT _cluster/settings\n{}").3, Some(D::ServerCommand));
        assert!(one("POST _cluster/reroute").2);
        assert_eq!(one("PUT /new_index").1, K::Create);
    }

    #[test]
    fn parses_requests() {
        let r = super::parse_request("  get orders/_search?size=5\n{\"a\":1}\n").unwrap();
        assert_eq!(
            (r.method.as_str(), r.path.as_str(), r.body.as_str()),
            ("GET", "/orders/_search?size=5", "{\"a\":1}")
        );
        assert_eq!(r.segments(), ["orders", "_search"]);
        assert!(super::parse_request("{\"a\": 1}").is_err());
        assert!(super::parse_request("GET /a/../b").is_err());
    }
}
