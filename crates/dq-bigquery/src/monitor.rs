//! Monitoring: running and pending query jobs of the billing project
//! (`jobs.list`, every user's where the account may see them) with cancel,
//! and the largest tables from each dataset's `__TABLES__`. `BigQuery` is
//! serverless: there are no sessions, connections, locks or server settings.

use async_trait::async_trait;
use dq_core::DbResult;
use dq_core::adapter::Session;
use dq_core::monitor::{Monitor, MonitorSnapshot, ObjectSize, ServerProcess, ServerSetting, short_query};
use dq_core::node::{NodeKind, ObjectRef};
use dq_core::sql::{BigQueryDialect, Dialect};
use serde_json::Value;

use crate::catalog;
use crate::client::{JobRef, MAX_LISTED};
use crate::session::BqSession;

/// Datasets whose tables are sized for "largest tables".
const SIZED_DATASETS: usize = 20;

/// A stable process id for a job id (FNV-1a), positive so the UI can show it.
pub fn pid_of(job_id: &str) -> i64 {
    let hash = job_id.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    (hash >> 1) as i64
}

fn process(job: &Value, now_ms: i64) -> Option<(ServerProcess, JobRef)> {
    let reference: JobRef = serde_json::from_value(job["jobReference"].clone()).ok()?;
    let state = job.pointer("/status/state").and_then(Value::as_str).unwrap_or("RUNNING");
    if state == "DONE" {
        return None;
    }
    let started = job.pointer("/statistics/startTime").or_else(|| job.pointer("/statistics/creationTime"));
    let started: Option<i64> = started.and_then(Value::as_str).and_then(|s| s.parse().ok());
    let process = ServerProcess {
        pid: pid_of(&reference.job_id),
        user: job["user_email"].as_str().map(str::to_owned),
        database: Some(reference.project_id.clone()),
        client: Some(reference.job_id.clone()),
        state: Some(state.to_ascii_lowercase()),
        duration_ms: started.map(|s| (now_ms - s).max(0)),
        query: short_query(job.pointer("/configuration/query/query").and_then(Value::as_str).map(str::to_owned)),
        wait: None,
        blocked_by: Vec::new(),
        active: state == "RUNNING",
    };
    Some((process, reference))
}

impl BqSession {
    /// Unfinished jobs: every user's when allowed (`bigquery.jobs.listAll`), else the account's own.
    async fn running_jobs(&self) -> (Vec<Value>, Option<String>) {
        let project = self.client.project.clone();
        let mut query =
            vec![("stateFilter", "running"), ("stateFilter", "pending"), ("projection", "full"), ("maxResults", "500")];
        query.push(("allUsers", "true"));
        let jobs = |v: Value| v["jobs"].as_array().cloned().unwrap_or_default();
        if let Ok(found) = self.client.get(&project, &["jobs"], &query).await {
            return (jobs(found), None);
        }
        query.pop();
        let note = "Only this account's jobs are shown: listing every user's jobs needs bigquery.jobs.listAll.";
        (self.client.get(&project, &["jobs"], &query).await.map(jobs).unwrap_or_default(), Some(note.into()))
    }
}

#[async_trait]
impl Monitor for BqSession {
    async fn snapshot(&self, long_running_secs: u32) -> DbResult<MonitorSnapshot> {
        let (jobs, note) = self.running_jobs().await;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let (processes, references): (Vec<ServerProcess>, Vec<JobRef>) =
            jobs.iter().filter_map(|j| process(j, now_ms)).unzip();
        self.remember_jobs(references);
        let long_ms = i64::from(long_running_secs) * 1000;
        let mut unavailable =
            vec!["BigQuery is serverless: there are no connections, locks or replicas to show.".to_owned()];
        unavailable.extend(note);
        Ok(MonitorSnapshot {
            version: self.info().version.clone(),
            connections: None,
            active_queries: Some(processes.iter().filter(|p| p.active).count() as i64),
            long_running: Some(processes.iter().filter(|p| p.duration_ms.unwrap_or(0) > long_ms).count() as i64),
            blocked: None,
            replication: None,
            processes,
            locks: Vec::new(),
            unavailable,
        })
    }

    async fn server_settings(&self, _search: Option<&str>) -> DbResult<Vec<ServerSetting>> {
        Ok(Vec::new())
    }

    async fn largest_tables(&self, database: &str, limit: u32) -> DbResult<Vec<ObjectSize>> {
        let project = if database.is_empty() { self.client.project.clone() } else { database.to_owned() };
        let d = BigQueryDialect;
        let mut sizes = Vec::new();
        for dataset in catalog::datasets(&self.client, &project).await?.into_iter().take(SIZED_DATASETS) {
            let dataset = dataset.object.name;
            let sql = format!(
                "SELECT table_id, size_bytes, row_count FROM {}.{}.__TABLES__ ORDER BY size_bytes DESC LIMIT {}",
                d.quote_ident(&project),
                d.quote_ident(&dataset),
                limit.clamp(1, 1000)
            );
            for r in catalog::query(&self.client, &sql, &[]).await.unwrap_or_default() {
                let bytes = r.number(1).unwrap_or(0);
                sizes.push(ObjectSize {
                    object: ObjectRef::new(NodeKind::Table, project.clone(), Some(dataset.clone()), r.string(0)),
                    total_bytes: bytes,
                    table_bytes: bytes,
                    index_bytes: 0,
                    rows: r.number(2),
                });
            }
        }
        sizes.sort_by_key(|s| std::cmp::Reverse(s.total_bytes));
        sizes.truncate(limit.min(MAX_LISTED as u32) as usize);
        Ok(sizes)
    }

    /// Cancels the job; a `BigQuery` job has no connection to end, so `terminate` does the same.
    async fn kill(&self, pid: i64, _terminate: bool) -> DbResult<bool> {
        let Some(job) = self.listed_job(pid) else { return Ok(false) };
        self.client.cancel(&job).await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lists_unfinished_jobs() {
        assert_eq!(pid_of("job_a"), pid_of("job_a"));
        assert!(pid_of("job_a") > 0 && pid_of("job_a") != pid_of("job_b"));
        let job = json!({ "jobReference": { "projectId": "p", "jobId": "j1", "location": "EU" },
            "user_email": "ana@example.com", "status": { "state": "RUNNING" },
            "statistics": { "startTime": "1000" }, "configuration": { "query": { "query": "SELECT 1" } } });
        let (p, r) = process(&job, 4000).unwrap();
        assert_eq!((p.duration_ms, p.query.as_deref(), p.active), (Some(3000), Some("SELECT 1"), true));
        assert_eq!(r.location.as_deref(), Some("EU"));
        assert!(
            process(&json!({ "jobReference": { "projectId": "p", "jobId": "j2" }, "status": { "state": "DONE" } }), 0)
                .is_none()
        );
    }
}
