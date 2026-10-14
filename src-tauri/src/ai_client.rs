//! Model providers for the AI assistant, each used with the user's own API key.
//!
//! Three wire formats cover them: Anthropic's Messages API (Claude), the
//! `OpenAI` chat-completions API (`OpenAI`, `DeepSeek`, Mistral and any compatible
//! custom endpoint) and Google's Gemini `generateContent`. Keys are sent only
//! in headers, never logged, and never over plain HTTP to another machine.

use std::time::Duration;

use dq_core::{DbError, DbResult, ErrorKind};
use dq_store::{AiProvider, Settings};
use serde::Serialize;
use serde_json::{Value, json};
use ts_rs::TS;

/// Request formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Api {
    Anthropic,
    OpenAiChat,
    Gemini,
}

/// What the app knows about a provider.
#[derive(Debug, Clone, Copy, Serialize, TS)]
#[ts(export)]
pub struct ProviderInfo {
    pub provider: AiProvider,
    pub name: &'static str,
    /// Base URL of the API; empty for the custom provider (it uses `ai_endpoint`).
    pub base_url: &'static str,
    /// Used when no model is chosen; `None` means the user picks one (see "Load models").
    pub default_model: Option<&'static str>,
    /// The provider refuses requests without a key (custom servers often need none).
    pub key_required: bool,
    /// Where to create a key.
    pub key_url: &'static str,
    #[serde(skip)]
    #[ts(skip)]
    api: Api,
}

/// The providers, in display order.
pub const PROVIDERS: &[ProviderInfo] = &[
    ProviderInfo {
        provider: AiProvider::Anthropic,
        name: "Claude (Anthropic)",
        base_url: "https://api.anthropic.com/v1",
        default_model: Some("claude-opus-5-5"),
        key_required: true,
        key_url: "https://console.anthropic.com/settings/keys",
        api: Api::Anthropic,
    },
    ProviderInfo {
        provider: AiProvider::Openai,
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        default_model: None,
        key_required: true,
        key_url: "https://platform.openai.com/api-keys",
        api: Api::OpenAiChat,
    },
    ProviderInfo {
        provider: AiProvider::Google,
        name: "Google Gemini",
        base_url: "https://generativelanguage.googleapis.com/v1beta",
        default_model: None,
        key_required: true,
        key_url: "https://aistudio.google.com/app/apikey",
        api: Api::Gemini,
    },
    ProviderInfo {
        provider: AiProvider::Deepseek,
        name: "DeepSeek",
        base_url: "https://api.deepseek.com",
        default_model: Some("deepseek-chat"),
        key_required: true,
        key_url: "https://platform.deepseek.com/api_keys",
        api: Api::OpenAiChat,
    },
    ProviderInfo {
        provider: AiProvider::Mistral,
        name: "Mistral",
        base_url: "https://api.mistral.ai/v1",
        default_model: Some("mistral-large-latest"),
        key_required: true,
        key_url: "https://console.mistral.ai/api-keys",
        api: Api::OpenAiChat,
    },
    ProviderInfo {
        provider: AiProvider::Custom,
        name: "Custom (OpenAI-compatible)",
        base_url: "",
        default_model: None,
        key_required: false,
        key_url: "",
        api: Api::OpenAiChat,
    },
];

pub fn info(provider: AiProvider) -> &'static ProviderInfo {
    PROVIDERS.iter().find(|p| p.provider == provider).unwrap_or(&PROVIDERS[0])
}

/// Keychain id of a provider's API key (the Anthropic id predates the others).
pub fn key_id(provider: AiProvider) -> &'static str {
    match provider {
        AiProvider::Anthropic => "ai:anthropic",
        AiProvider::Openai => "ai:openai",
        AiProvider::Google => "ai:google",
        AiProvider::Deepseek => "ai:deepseek",
        AiProvider::Mistral => "ai:mistral",
        AiProvider::Custom => "ai:custom",
    }
}

/// The model a request uses: the chosen one, else the provider's default.
pub fn model(settings: &Settings) -> DbResult<String> {
    let chosen = settings.ai_model.trim();
    if !chosen.is_empty() {
        return Ok(chosen.to_owned());
    }
    info(settings.ai_provider).default_model.map(str::to_owned).ok_or_else(|| {
        DbError::invalid("Choose a model in Settings → AI assistant (Load models lists the ones your key can use).")
    })
}

/// Where requests go and with which key.
pub struct Target {
    pub provider: AiProvider,
    base_url: String,
    key: Option<String>,
    api: Api,
}

impl Target {
    /// Checks the endpoint and the key before anything is sent.
    pub fn new(settings: &Settings, key: Option<String>) -> DbResult<Self> {
        let spec = info(settings.ai_provider);
        let base_url = if spec.provider == AiProvider::Custom {
            settings.ai_endpoint.trim().trim_end_matches('/').to_owned()
        } else {
            spec.base_url.to_owned()
        };
        Self::with_base(spec, base_url, key)
    }

    fn with_base(spec: &ProviderInfo, base_url: String, key: Option<String>) -> DbResult<Self> {
        let key = key.map(|k| k.trim().to_owned()).filter(|k| !k.is_empty());
        let url = url::Url::parse(&base_url)
            .map_err(|_| DbError::invalid("The custom endpoint must be a URL such as http://localhost:11434/v1."))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(DbError::invalid("The custom endpoint must start with http:// or https://."));
        }
        let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1" | "[::1]"));
        if key.is_some() && url.scheme() == "http" && !local {
            return Err(DbError::invalid(
                "The API key would be sent unencrypted. Use an https:// endpoint, or remove the key for a local server.",
            ));
        }
        if spec.key_required && key.is_none() {
            return Err(DbError::new(
                ErrorKind::Auth,
                format!("Add your {} API key in Settings → AI assistant.", spec.name),
            )
            .with_code("AI"));
        }
        Ok(Self { provider: spec.provider, base_url, key, api: spec.api })
    }

    fn name(&self) -> &'static str {
        info(self.provider).name
    }

    fn get(&self, client: &reqwest::Client, path: &str) -> reqwest::RequestBuilder {
        self.auth(client.get(format!("{}{path}", self.base_url)))
    }

    fn post(&self, client: &reqwest::Client, path: &str) -> reqwest::RequestBuilder {
        self.auth(client.post(format!("{}{path}", self.base_url)))
    }

    fn auth(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let Some(key) = &self.key else { return request };
        match self.api {
            Api::Anthropic => request.header("x-api-key", key).header("anthropic-version", "2023-06-01"),
            Api::Gemini => request.header("x-goog-api-key", key),
            Api::OpenAiChat => request.bearer_auth(key),
        }
    }
}

fn client(timeout: Duration) -> DbResult<reqwest::Client> {
    reqwest::Client::builder().timeout(timeout).build().map_err(|e| ai_error(format!("HTTP client: {e}")))
}

fn ai_error(message: impl AsRef<str>) -> DbError {
    DbError::new(ErrorKind::Network, message.as_ref()).with_code("AI")
}

/// Sends a request and returns the JSON body, or a readable error naming the provider.
async fn send(target: &Target, request: reqwest::RequestBuilder) -> DbResult<Value> {
    let name = target.name();
    let response = request.send().await.map_err(|e| {
        if e.is_timeout() {
            DbError::new(ErrorKind::Timeout, format!("{name} did not answer in time.")).with_code("AI")
        } else {
            ai_error(format!("Cannot reach {name}: {e}"))
        }
    })?;
    let status = response.status();
    let body = response.text().await.map_err(|e| ai_error(format!("Unreadable reply from {name}: {e}")))?;
    let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    if status.is_success() {
        return if value.is_null() {
            Err(ai_error(format!("{name} sent a reply that is not JSON.")))
        } else {
            Ok(value)
        };
    }
    // `{ "error": { "message" } }` (OpenAI, Anthropic, Gemini, DeepSeek) or `{ "message" }` (Mistral).
    let message = value["error"]["message"]
        .as_str()
        .or_else(|| value["message"].as_str())
        .or_else(|| value["error"].as_str())
        .map_or_else(|| body.chars().take(300).collect::<String>(), str::to_owned);
    let (kind, hint) = match status.as_u16() {
        401 | 403 => (ErrorKind::Auth, Some("Check the API key in Settings → AI assistant.")),
        404 => (ErrorKind::NotFound, Some("Check the model name (Load models lists the available ones).")),
        429 => (ErrorKind::Server, Some("The provider's rate limit or quota was reached; try again later.")),
        _ => (ErrorKind::Server, None),
    };
    Err(DbError::new(kind, format!("{name} {status}: {message}")).with_code("AI").with_action(hint))
}

/// One question and its answer text.
pub async fn complete(target: &Target, model: &str, system: &str, user: &str) -> DbResult<String> {
    let client = client(Duration::from_secs(300))?;
    let text = match target.api {
        Api::Anthropic => {
            let mut body = json!({
                "model": model,
                "max_tokens": 16000,
                "output_config": { "effort": "medium" },
                "system": system,
                "messages": [{ "role": "user", "content": user }],
            });
            // Server-side refusal fallback, on the models that accept it.
            let fallback = ANTHROPIC_FALLBACK_MODELS.contains(&model);
            if fallback {
                body["fallbacks"] = json!("default");
            }
            let mut request = target.post(&client, "/messages").json(&body);
            if fallback {
                request = request.header("anthropic-beta", "server-side-fallback-2026-07-01");
            }
            let value = send(target, request).await?;
            if value["stop_reason"] == "refusal" {
                return Err(declined());
            }
            value["content"]
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|b| b["type"] == "text")
                        .filter_map(|b| b["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default()
        }
        Api::OpenAiChat => {
            let body = json!({
                "model": model,
                "messages": [
                    { "role": "system", "content": system },
                    { "role": "user", "content": user },
                ],
            });
            let value = send(target, target.post(&client, "/chat/completions").json(&body)).await?;
            if value["choices"][0]["finish_reason"] == "content_filter" {
                return Err(declined());
            }
            value["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_owned()
        }
        Api::Gemini => {
            let body = json!({
                "systemInstruction": { "parts": [{ "text": system }] },
                "contents": [{ "role": "user", "parts": [{ "text": user }] }],
            });
            let path = format!("/models/{}:generateContent", model.trim_start_matches("models/"));
            let value = send(target, target.post(&client, &path).json(&body)).await?;
            let candidate = &value["candidates"][0];
            if value["promptFeedback"]["blockReason"].is_string()
                || matches!(candidate["finishReason"].as_str(), Some("SAFETY" | "PROHIBITED_CONTENT" | "BLOCKLIST"))
            {
                return Err(declined());
            }
            candidate["content"]["parts"]
                .as_array()
                .map(|parts| parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(""))
                .unwrap_or_default()
        }
    };
    if text.trim().is_empty() {
        return Err(ai_error(format!("{} returned no text.", target.name())));
    }
    Ok(text)
}

/// Claude models that accept the server-side refusal fallback.
const ANTHROPIC_FALLBACK_MODELS: &[&str] =
    &["claude-fable-5-1", "claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5"];

fn declined() -> DbError {
    DbError::new(ErrorKind::Server, "The model declined this request.").with_code("AI")
}

/// Models the key can use for chat, sorted. Also serves as a key test: it costs no tokens.
pub async fn list_models(target: &Target) -> DbResult<Vec<String>> {
    let client = client(Duration::from_secs(30))?;
    let mut models: Vec<String> = match target.api {
        Api::Anthropic => {
            let value = send(target, target.get(&client, "/models?limit=1000")).await?;
            ids(&value["data"], "id")
        }
        Api::OpenAiChat => {
            let value = send(target, target.get(&client, "/models")).await?;
            // Some servers answer `{ "data": [...] }`, Ollama's native API `{ "models": [...] }`.
            let mut found = ids(&value["data"], "id");
            if found.is_empty() {
                found = ids(&value["models"], "name");
            }
            // OpenAI lists embedding, image and audio models too.
            found.retain(|id| !NOT_CHAT.iter().any(|w| id.contains(w)));
            found
        }
        Api::Gemini => {
            let value = send(target, target.get(&client, "/models?pageSize=1000")).await?;
            value["models"]
                .as_array()
                .map(|models| {
                    models
                        .iter()
                        .filter(|m| {
                            m["supportedGenerationMethods"]
                                .as_array()
                                .is_some_and(|ms| ms.iter().any(|x| x == "generateContent"))
                        })
                        .filter_map(|m| m["name"].as_str())
                        .map(|n| n.trim_start_matches("models/").to_owned())
                        .collect()
                })
                .unwrap_or_default()
        }
    };
    models.sort();
    models.dedup();
    Ok(models)
}

/// Model-name parts of non-chat models in OpenAI-style lists.
const NOT_CHAT: &[&str] =
    &["embedding", "embed", "tts", "whisper", "dall-e", "moderation", "transcribe", "image", "audio", "realtime"];

fn ids(list: &Value, field: &str) -> Vec<String> {
    list.as_array()
        .map(|items| items.iter().filter_map(|m| m[field].as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A one-request HTTP server: returns the base URL and, later, the raw request it received.
    async fn mock(status: u16, body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                request.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&request);
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let length = text[..head_end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_owned())
                        })
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= head_end + 4 + length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (base, handle)
    }

    fn target(provider: AiProvider, base: &str, key: Option<&str>) -> Target {
        Target::with_base(info(provider), base.to_owned(), key.map(str::to_owned)).unwrap()
    }

    #[tokio::test]
    async fn openai_style_chat() {
        let (base, request) =
            mock(200, r#"{"choices":[{"message":{"content":"Use ```sql\nSELECT 1\n```"},"finish_reason":"stop"}]}"#)
                .await;
        let t = target(AiProvider::Deepseek, &base, Some("sk-test"));
        let text = complete(&t, "deepseek-chat", "sys", "question").await.unwrap();
        assert!(text.contains("SELECT 1"));
        let raw = request.await.unwrap();
        assert!(raw.starts_with("POST /chat/completions "));
        assert!(raw.to_ascii_lowercase().contains("authorization: bearer sk-test"));
        assert!(raw.contains(r#""role":"system""#) && raw.contains(r#""model":"deepseek-chat""#));
    }

    #[tokio::test]
    async fn gemini_generate_content() {
        let (base, request) = mock(
            200,
            r#"{"candidates":[{"content":{"parts":[{"text":"Hello "},{"text":"there"}]},"finishReason":"STOP"}]}"#,
        )
        .await;
        let t = target(AiProvider::Google, &base, Some("g-key"));
        assert_eq!(complete(&t, "models/gemini-x", "sys", "q").await.unwrap(), "Hello there");
        let raw = request.await.unwrap();
        assert!(raw.starts_with("POST /models/gemini-x:generateContent "));
        assert!(raw.to_ascii_lowercase().contains("x-goog-api-key: g-key"));
        assert!(raw.contains("systemInstruction"));
    }

    #[tokio::test]
    async fn anthropic_messages() {
        let (base, request) =
            mock(200, r#"{"content":[{"type":"text","text":"Answer"}],"stop_reason":"end_turn"}"#).await;
        let t = target(AiProvider::Anthropic, &base, Some("ant-key"));
        assert_eq!(complete(&t, "claude-opus-5-5", "sys", "q").await.unwrap(), "Answer");
        let raw = request.await.unwrap().to_ascii_lowercase();
        assert!(raw.starts_with("post /messages "));
        assert!(raw.contains("x-api-key: ant-key") && raw.contains("anthropic-version: 2023-06-01"));
    }

    #[tokio::test]
    async fn errors_are_readable() {
        let (base, _) = mock(401, r#"{"error":{"message":"Incorrect API key provided"}}"#).await;
        let e = complete(&target(AiProvider::Openai, &base, Some("bad")), "m", "s", "u").await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Auth);
        assert!(e.message.contains("Incorrect API key") && e.action.is_some(), "{e:?}");
        let (base, _) = mock(429, r#"{"message":"Requests rate limit exceeded"}"#).await;
        let e = complete(&target(AiProvider::Mistral, &base, Some("k")), "m", "s", "u").await.unwrap_err();
        assert!(e.message.contains("rate limit"), "{e:?}");
        let (base, _) = mock(200, r#"{"candidates":[{"finishReason":"SAFETY"}]}"#).await;
        let e = complete(&target(AiProvider::Google, &base, Some("k")), "m", "s", "u").await.unwrap_err();
        assert!(e.message.contains("declined"));
    }

    #[tokio::test]
    async fn lists_chat_models() {
        let (base, _) =
            mock(200, r#"{"data":[{"id":"gpt-x"},{"id":"text-embedding-3"},{"id":"dall-e-3"},{"id":"gpt-a"}]}"#).await;
        assert_eq!(list_models(&target(AiProvider::Openai, &base, Some("k"))).await.unwrap(), ["gpt-a", "gpt-x"]);
        let (base, _) = mock(
            200,
            r#"{"models":[{"name":"models/gemini-x","supportedGenerationMethods":["generateContent"]},{"name":"models/embedding-1","supportedGenerationMethods":["embedContent"]}]}"#,
        )
        .await;
        assert_eq!(list_models(&target(AiProvider::Google, &base, Some("k"))).await.unwrap(), ["gemini-x"]);
        let (base, _) = mock(200, r#"{"models":[{"name":"qwen2.5-coder"}]}"#).await;
        assert_eq!(list_models(&target(AiProvider::Custom, &base, None)).await.unwrap(), ["qwen2.5-coder"]);
    }

    #[test]
    fn keys_stay_safe() {
        let custom = info(AiProvider::Custom);
        assert!(Target::with_base(custom, "http://ai.example.com/v1".into(), Some("k".into())).is_err());
        assert!(Target::with_base(custom, "http://localhost:11434/v1".into(), Some("k".into())).is_ok());
        assert!(Target::with_base(custom, "http://gpu-box.lan/v1".into(), None).is_ok());
        assert!(Target::with_base(custom, "ftp://x".into(), None).is_err());
        let openai = info(AiProvider::Openai);
        assert_eq!(
            Target::with_base(openai, openai.base_url.into(), None).err().map(|e| e.kind),
            Some(ErrorKind::Auth)
        );
        let distinct: std::collections::HashSet<_> = PROVIDERS.iter().map(|p| key_id(p.provider)).collect();
        assert_eq!(distinct.len(), PROVIDERS.len());
    }

    #[test]
    fn models_default_only_where_stable() {
        let mut s = Settings { ai_provider: AiProvider::Deepseek, ..Settings::default() };
        assert_eq!(model(&s).unwrap(), "deepseek-chat");
        s.ai_provider = AiProvider::Openai;
        assert!(model(&s).is_err());
        s.ai_model = " gpt-x ".into();
        assert_eq!(model(&s).unwrap(), "gpt-x");
    }

    /// A real OpenAI-compatible server, e.g. `DATAQUERY_TEST_OLLAMA=http://localhost:11434/v1`
    /// (optionally `DATAQUERY_TEST_OLLAMA_MODEL`, else the first listed model).
    #[tokio::test]
    async fn custom_endpoint_end_to_end() {
        let Ok(base) = std::env::var("DATAQUERY_TEST_OLLAMA") else {
            eprintln!("DATAQUERY_TEST_OLLAMA not set; skipping");
            return;
        };
        let t = target(AiProvider::Custom, &base, None);
        let models = list_models(&t).await.unwrap();
        assert!(!models.is_empty() && models.iter().all(|m| !m.contains("embed")), "{models:?}");
        let model = std::env::var("DATAQUERY_TEST_OLLAMA_MODEL").unwrap_or_else(|_| models[0].clone());
        let text = complete(
            &t,
            &model,
            "You write PostgreSQL. Answer with one ```sql block only.",
            "Count the rows of table orders.",
        )
        .await
        .unwrap();
        let sql = crate::ai::extract_sql(&text).unwrap_or_default().to_ascii_lowercase();
        assert!(sql.contains("count") && sql.contains("orders"), "{text}");
    }
}
