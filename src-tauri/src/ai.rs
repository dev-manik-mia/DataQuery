//! Opt-in AI assistant (screen 09). Sends only what the disclosure lists:
//! the question, the SQL or error being asked about, and table/column names
//! and types. Never credentials, rows or results. Answers come back as text
//! and SQL that the UI places in the editor; nothing here executes SQL.

use dq_core::schema::SchemaModel;
use dq_core::sql::SqlFlavor;
use dq_core::{DbError, DbResult};
use dq_store::{AiProvider, Settings};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::ai_client;

/// Schema text sent with a request, at most.
const MAX_SCHEMA_CHARS: usize = 60_000;
const MAX_INPUT_CHARS: usize = 20_000;

#[derive(Debug, Clone, Deserialize, TS)]
#[serde(tag = "task", rename_all = "snake_case")]
#[ts(export)]
pub enum AiRequest {
    /// Natural language → SQL.
    Sql {
        question: String,
    },
    ExplainQuery {
        sql: String,
    },
    ExplainError {
        sql: String,
        error: String,
    },
    SummarizeSchema,
}

/// What a request would send, shown before (and with) every request.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct AiDisclosure {
    pub provider: String,
    pub model: String,
    /// e.g. "Column names and types for 2 tables", "Your question".
    pub sent: Vec<String>,
    pub not_sent: Vec<String>,
    /// Tables whose structure is included.
    pub tables: Vec<String>,
    #[ts(type = "number")]
    pub characters: u64,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct AiAnswer {
    pub text: String,
    /// SQL to place in the editor. Never run automatically.
    pub sql: Option<String>,
    pub model: String,
    pub disclosure: AiDisclosure,
}

/// The prepared prompt plus its disclosure.
pub struct Prepared {
    system: String,
    user: String,
    pub disclosure: AiDisclosure,
}

fn clip(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

/// Words of the question, to send the most relevant tables first.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() > 2)
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Table structures as compact text, relevant tables first, within the size budget.
fn schema_text(model: &SchemaModel, hint: &str) -> (String, Vec<String>) {
    let hint = words(hint);
    let score = |name: &str| {
        let name = name.to_ascii_lowercase();
        hint.iter().filter(|w| name.contains(w.trim_end_matches('s')) || w.contains(&name)).count()
    };
    let mut tables: Vec<_> = model.tables.iter().collect();
    tables.sort_by_key(|t| std::cmp::Reverse(score(&t.object.name)));
    let mut text = String::new();
    let mut included = Vec::new();
    for t in tables {
        let name = match &t.object.schema {
            Some(s) => format!("{s}.{}", t.object.name),
            None => t.object.name.clone(),
        };
        let cols: Vec<String> = t
            .columns
            .iter()
            .map(|c| {
                format!(
                    "{} {}{}{}",
                    c.name,
                    c.data_type,
                    if c.nullable { "" } else { " not null" },
                    if c.primary_key { " pk" } else { "" }
                )
            })
            .collect();
        let line = format!("{name}({})\n", cols.join(", "));
        if text.len() + line.len() > MAX_SCHEMA_CHARS {
            break;
        }
        text.push_str(&line);
        included.push(name);
    }
    for fk in &model.foreign_keys {
        let line = format!(
            "fk {}.{}({}) -> {}({})\n",
            fk.source.schema.as_deref().unwrap_or(""),
            fk.source.name,
            fk.columns.join(","),
            fk.target.name,
            fk.target_columns.join(",")
        );
        if text.len() + line.len() > MAX_SCHEMA_CHARS {
            break;
        }
        text.push_str(&line);
    }
    (text, included)
}

/// Builds the prompt and the disclosure for a request.
pub fn prepare(
    settings: &Settings,
    flavor: SqlFlavor,
    version: &str,
    model_schema: &SchemaModel,
    request: &AiRequest,
) -> DbResult<Prepared> {
    let dialect = match flavor {
        SqlFlavor::Postgres if version.starts_with("CockroachDB") => "CockroachDB (PostgreSQL-compatible)",
        SqlFlavor::Postgres => "PostgreSQL",
        SqlFlavor::Mysql => "MySQL/MariaDB",
        SqlFlavor::Sqlserver => "SQL Server T-SQL",
        SqlFlavor::Oracle => "Oracle SQL",
        SqlFlavor::Clickhouse => "ClickHouse SQL",
        SqlFlavor::Duckdb => "DuckDB SQL",
        SqlFlavor::Sqlite => "SQLite",
        SqlFlavor::Snowflake => "Snowflake SQL",
        SqlFlavor::Bigquery => "BigQuery GoogleSQL (backtick `project.dataset.table` names)",
        SqlFlavor::Mongodb => "MongoDB shell (mongosh) syntax, e.g. db.orders.find({ … })",
        SqlFlavor::Partiql => "DynamoDB PartiQL",
        SqlFlavor::Redis => "Redis commands (one per line, redis-cli syntax)",
        SqlFlavor::Cql => "Cassandra CQL (restrict the partition key; avoid ALLOW FILTERING)",
        SqlFlavor::QueryDsl => {
            "Elasticsearch requests in Kibana Dev Tools form (a `GET /index/_search` line, then the JSON body)"
        }
        SqlFlavor::Cypher => "Neo4j Cypher",
    };
    let (hint, task, mut sent) = match request {
        AiRequest::Sql { question } => {
            if question.trim().is_empty() {
                return Err(DbError::invalid("Ask a question first."));
            }
            (
                question.as_str(),
                format!("Write one {dialect} query that answers:\n{}", clip(question, MAX_INPUT_CHARS)),
                vec!["Your question".to_owned()],
            )
        }
        AiRequest::ExplainQuery { sql } => (
            sql.as_str(),
            format!(
                "Explain what this query does, step by step, and point out problems:\n```sql\n{}\n```",
                clip(sql, MAX_INPUT_CHARS)
            ),
            vec!["The SQL in the editor".to_owned()],
        ),
        AiRequest::ExplainError { sql, error } => (
            sql.as_str(),
            format!(
                "Explain this error and suggest a fix.\n```sql\n{}\n```\nError: {}",
                clip(sql, MAX_INPUT_CHARS),
                clip(error, 4_000)
            ),
            vec!["The SQL in the editor".to_owned(), "The error message".to_owned()],
        ),
        AiRequest::SummarizeSchema => (
            "",
            "Summarize this schema: the main entities, how they relate, and anything unusual.".to_owned(),
            Vec::new(),
        ),
    };
    let (schema, tables) = schema_text(model_schema, hint);
    sent.insert(
        0,
        format!("Column names and types for {} table{}", tables.len(), if tables.len() == 1 { "" } else { "s" }),
    );
    let system = format!(
        "You help a developer working in a {dialect} database ({version}) inside a desktop SQL client. \
         Use only the tables and columns in the schema below. When you write SQL, put exactly one statement \
         in a ```sql block; the user reviews it and runs it themselves. Prefer read-only queries unless asked \
         to change data, and say so clearly when a statement writes or deletes. Keep explanations short.\n\nSchema:\n{schema}"
    );
    let disclosure = AiDisclosure {
        provider: match settings.ai_provider {
            AiProvider::Custom => format!("Custom endpoint {}", settings.ai_endpoint.trim()),
            other => ai_client::info(other).name.to_owned(),
        },
        model: ai_client::model(settings)?,
        sent,
        not_sent: vec!["Credentials and connection details".into(), "Row data".into(), "Query results".into()],
        tables,
        characters: (system.len() + task.len()) as u64,
    };
    Ok(Prepared { system, user: task, disclosure })
}

/// The first fenced SQL code block of an answer.
pub fn extract_sql(text: &str) -> Option<String> {
    let start = text.find("```sql").map(|i| i + 6).or_else(|| text.find("```").map(|i| i + 3))?;
    let rest = &text[start..];
    let end = rest.find("```")?;
    Some(rest[..end].trim().to_owned()).filter(|s| !s.is_empty())
}

/// Sends a prepared request to the configured provider with that provider's key.
pub async fn ask(settings: &Settings, api_key: Option<String>, prepared: Prepared) -> DbResult<AiAnswer> {
    let target = ai_client::Target::new(settings, api_key)?;
    let model = prepared.disclosure.model.clone();
    let text = ai_client::complete(&target, &model, &prepared.system, &prepared.user).await?;
    Ok(AiAnswer { sql: extract_sql(&text), text, model, disclosure: prepared.disclosure })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dq_core::node::{NodeKind, ObjectRef};
    use dq_core::schema::{ModelColumn, ModelTable};

    fn model() -> SchemaModel {
        let table = |name: &str| ModelTable {
            object: ObjectRef::new(NodeKind::Table, "app", Some("public".into()), name),
            columns: vec![ModelColumn {
                name: "id".into(),
                data_type: "bigint".into(),
                nullable: false,
                primary_key: true,
            }],
        };
        SchemaModel { tables: vec![table("audit_log"), table("customers"), table("orders")], ..SchemaModel::default() }
    }

    #[test]
    fn disclosure_lists_what_is_sent_and_relevant_tables_come_first() {
        let p = prepare(
            &Settings::default(),
            SqlFlavor::Postgres,
            "PostgreSQL 17",
            &model(),
            &AiRequest::Sql { question: "Which customers have most orders?".into() },
        )
        .unwrap();
        assert_eq!(p.disclosure.model, "claude-opus-5-5");
        assert_eq!(p.disclosure.provider, "Claude (Anthropic)");
        assert_eq!(p.disclosure.sent, ["Column names and types for 3 tables", "Your question"]);
        assert!(p.disclosure.not_sent.iter().any(|n| n == "Row data"));
        assert_ne!(p.disclosure.tables[0], "public.audit_log");
        assert!(p.system.contains("public.orders(id bigint not null pk)"));
        assert!(
            prepare(&Settings::default(), SqlFlavor::Mysql, "", &model(), &AiRequest::Sql { question: " ".into() })
                .is_err()
        );
    }

    #[test]
    fn extracts_sql_blocks() {
        assert_eq!(extract_sql("Here:\n```sql\nSELECT 1;\n```\nDone").as_deref(), Some("SELECT 1;"));
        assert_eq!(extract_sql("no code"), None);
    }
}
