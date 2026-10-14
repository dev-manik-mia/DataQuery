//! The `DynamoDB` JSON API over HTTPS, signed with AWS Signature Version 4.
//!
//! Every operation is one `POST /` with `X-Amz-Target: DynamoDB_20120810.<Op>`.
//! Throttling and server errors are retried with exponential backoff and
//! jitter. Credentials and signatures are never logged.

use std::fmt::Write;
use std::net::SocketAddr;
use std::time::Duration;

use dq_core::adapter::ConnectParams;
use dq_core::config::{AuthMethod, SslMode};
use dq_core::redact::redact_known;
use dq_core::value::hex;
use dq_core::{DbError, DbResult, ErrorKind};
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::{Digest, Sha256};

const SERVICE: &str = "dynamodb";
const CONTENT_TYPE: &str = "application/x-amz-json-1.0";
const MAX_ATTEMPTS: u32 = 5;

/// AWS credentials. `Debug` never shows the secret or the token.
#[derive(Clone)]
pub struct Credentials {
    pub access_key_id: String,
    secret: String,
    session_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").field("access_key_id", &self.access_key_id).finish_non_exhaustive()
    }
}

impl Credentials {
    pub fn new(access_key_id: impl Into<String>, secret: impl Into<String>, session_token: Option<String>) -> Self {
        Self { access_key_id: access_key_id.into(), secret: secret.into(), session_token }
    }

    /// The password is either the secret access key (with the access key ID as
    /// the username), or the JSON that `aws configure export-credentials
    /// --format process` prints (for AWS CLI profiles, including SSO).
    pub fn from_params(params: &ConnectParams) -> DbResult<Self> {
        let password = params.password().unwrap_or_default();
        if password.trim_start().starts_with('{')
            && let Ok(json) = serde_json::from_str::<Value>(password)
        {
            let field = |k: &str| json.get(k).and_then(Value::as_str).map(str::to_owned);
            if let (Some(id), Some(secret)) = (field("AccessKeyId"), field("SecretAccessKey")) {
                return Ok(Self::new(id, secret, field("SessionToken")));
            }
        }
        let id = params.config.username.trim();
        if id.is_empty() || password.is_empty() {
            return Err(DbError::new(ErrorKind::Auth, "Enter an access key ID and its secret access key.")
                .with_action(Some("Or choose an AWS CLI profile as the authentication method.")));
        }
        Ok(Self::new(id, password, None))
    }
}

/// The region from the `region` property, else the AWS IAM authentication settings.
pub fn region(params: &ConnectParams) -> DbResult<String> {
    let config = &params.config;
    config
        .property("region")
        .map(str::to_owned)
        .or_else(|| match &config.auth {
            AuthMethod::AwsIam { region, .. } if !region.is_empty() => Some(region.clone()),
            _ => None,
        })
        .ok_or_else(|| DbError::invalid("Enter the AWS region, e.g. eu-west-1."))
}

pub struct Client {
    http: reqwest::Client,
    url: String,
    /// `Host` header value, which the signature covers.
    host: String,
    region: String,
    credentials: Credentials,
    /// The endpoint is `DynamoDB` Local (or another local emulator).
    pub local: bool,
}

impl Client {
    pub fn new(params: &ConnectParams) -> DbResult<Self> {
        let config = &params.config;
        let region = region(params)?;
        let credentials = Credentials::from_params(params)?;
        let custom = !config.host.trim().is_empty();
        let host_name = if custom { config.host.trim().to_owned() } else { format!("dynamodb.{region}.amazonaws.com") };
        let tls = !custom || config.ssl_mode != SslMode::Disable;
        let port = if custom { config.port } else { 443 };
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(u64::from(config.connect_timeout_secs.max(1))))
            .tcp_keepalive(
                (config.options.keepalive_secs > 0)
                    .then(|| Duration::from_secs(u64::from(config.options.keepalive_secs))),
            )
            .user_agent("DataQuery")
            .use_rustls_tls();
        if let Some(endpoint) = params.endpoint {
            // Through an SSH tunnel: connect locally, verify TLS for the real host.
            builder = builder.resolve(&host_name, SocketAddr::new(endpoint.ip(), endpoint.port()));
        }
        if tls && custom {
            match config.ssl_mode {
                SslMode::Prefer | SslMode::Require => builder = builder.danger_accept_invalid_certs(true),
                SslMode::VerifyCa => builder = builder.danger_accept_invalid_hostnames(true),
                _ => {}
            }
        }
        if tls && let Some(ca) = &config.tls_files.ca_file {
            let pem = std::fs::read(ca)
                .map_err(|e| DbError::new(ErrorKind::Tls, format!("Could not read the CA file: {e}")))?;
            let cert = reqwest::Certificate::from_pem(&pem)
                .map_err(|e| DbError::new(ErrorKind::Tls, format!("Invalid CA file: {e}")))?;
            builder = builder.add_root_certificate(cert);
        }
        let bracketed = if host_name.contains(':') { format!("[{host_name}]") } else { host_name.clone() };
        let default_port = port == if tls { 443 } else { 80 };
        let host = if default_port { bracketed.clone() } else { format!("{bracketed}:{port}") };
        let scheme = if tls { "https" } else { "http" };
        Ok(Self {
            http: builder.build().map_err(|e| DbError::internal(format!("HTTP client: {e}")))?,
            url: format!("{scheme}://{host}/"),
            host,
            local: custom && matches!(host_name.as_str(), "localhost" | "127.0.0.1" | "::1"),
            region,
            credentials,
        })
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    pub fn access_key_id(&self) -> &str {
        &self.credentials.access_key_id
    }

    /// Calls `DynamoDB_20120810.<operation>` and returns the response JSON.
    pub async fn call(&self, operation: &str, body: &Value) -> DbResult<Value> {
        let payload = body.to_string();
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.send(operation, &payload).await {
                Err(e) if attempt < MAX_ATTEMPTS && retryable(&e) => {
                    tokio::time::sleep(backoff(attempt)).await;
                }
                other => return other,
            }
        }
    }

    async fn send(&self, operation: &str, payload: &str) -> DbResult<Value> {
        let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let target = format!("DynamoDB_20120810.{operation}");
        let mut headers = vec![
            ("content-type", CONTENT_TYPE.to_owned()),
            ("host", self.host.clone()),
            ("x-amz-date", amz_date.clone()),
            ("x-amz-target", target.clone()),
        ];
        if let Some(token) = &self.credentials.session_token {
            headers.push(("x-amz-security-token", token.clone()));
        }
        let authorization = authorization(&Signing {
            method: "POST",
            path: "/",
            query: "",
            headers: &headers,
            payload: payload.as_bytes(),
            credentials: &self.credentials,
            region: &self.region,
            service: SERVICE,
            amz_date: &amz_date,
        });
        let mut request = self
            .http
            .post(&self.url)
            .header("Content-Type", CONTENT_TYPE)
            .header("X-Amz-Date", &amz_date)
            .header("X-Amz-Target", &target)
            .header("Authorization", authorization)
            .body(payload.to_owned());
        if let Some(token) = &self.credentials.session_token {
            request = request.header("X-Amz-Security-Token", token);
        }
        let response = request.send().await.map_err(|e| self.network(&e))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| self.network(&e))?;
        if status.is_success() {
            return serde_json::from_str(&text)
                .map_err(|e| DbError::new(ErrorKind::Server, format!("DynamoDB returned invalid JSON: {e}")));
        }
        Err(self.redacted(api_error(status.as_u16(), &text)))
    }

    fn redacted(&self, mut e: DbError) -> DbError {
        e.message = redact_known(&e.message, &self.credentials.secret);
        e
    }

    fn network(&self, e: &reqwest::Error) -> DbError {
        let message = redact_known(
            &format!("{e}: {}", std::error::Error::source(e).map(ToString::to_string).unwrap_or_default()),
            &self.credentials.secret,
        );
        let kind = if e.is_timeout() {
            ErrorKind::Timeout
        } else if message.contains("certificate") || message.contains("tls") || message.contains("TLS") {
            ErrorKind::Tls
        } else {
            ErrorKind::Network
        };
        DbError::new(kind, message)
    }
}

fn retryable(e: &DbError) -> bool {
    e.kind == ErrorKind::Server
        && e.code.as_deref().is_some_and(|c| {
            matches!(
                c,
                "ProvisionedThroughputExceededException"
                    | "ThrottlingException"
                    | "RequestLimitExceeded"
                    | "InternalServerError"
                    | "ServiceUnavailable"
                    | "LimitExceededException"
            ) || c.starts_with("HTTP 5")
        })
}

/// 50 ms × 2ⁿ, capped at 2 s, with up to 50 % jitter.
pub fn backoff(attempt: u32) -> Duration {
    let base = 50u64.saturating_mul(1 << attempt.min(6)).min(2_000);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    Duration::from_millis(base / 2 + u64::from(nanos) % (base / 2 + 1))
}

/// Maps `{"__type": "…#ResourceNotFoundException", "message": "…"}` to an error.
pub fn api_error(status: u16, body: &str) -> DbError {
    let json: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let code = json
        .get("__type")
        .and_then(Value::as_str)
        .map_or_else(|| format!("HTTP {status}"), |t| t.rsplit('#').next().unwrap_or(t).to_owned());
    let message = json
        .get("message")
        .or_else(|| json.get("Message"))
        .and_then(Value::as_str)
        .map_or_else(|| body.trim().chars().take(500).collect::<String>(), str::to_owned);
    let (kind, message, action) = match code.as_str() {
        "ConditionalCheckFailedException" => {
            (ErrorKind::Constraint, "The item changed or no longer exists. Reload.".to_owned(), None)
        }
        "TransactionCanceledException" => {
            let reasons: Vec<String> = json
                .get("CancellationReasons")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
                .filter_map(|(i, r)| {
                    let code = r.get("Code").and_then(Value::as_str).filter(|c| *c != "None")?;
                    let text = r.get("Message").and_then(Value::as_str).unwrap_or_default();
                    Some(format!("statement {}: {code} {text}", i + 1).trim().to_owned())
                })
                .collect();
            let detail = if reasons.is_empty() { message } else { reasons.join("; ") };
            (ErrorKind::Constraint, format!("The transaction was cancelled: {detail}"), None)
        }
        "ResourceNotFoundException" => (ErrorKind::NotFound, message, Some("Check the table name and the region.")),
        "ValidationException" | "SerializationException" => (ErrorKind::InvalidInput, message, None),
        "UnrecognizedClientException" | "InvalidSignatureException" | "MissingAuthenticationTokenException" => (
            ErrorKind::Auth,
            message,
            Some("Check the access key ID, the secret access key and the region. The saved secret was not changed."),
        ),
        "ExpiredTokenException" => {
            (ErrorKind::Auth, message, Some("The temporary credentials expired. Run `aws sso login` and reconnect."))
        }
        "AccessDeniedException" => {
            (ErrorKind::Permission, message, Some("The IAM identity lacks the DynamoDB permission for this action."))
        }
        _ => (ErrorKind::Server, message, None),
    };
    DbError::new(kind, message).with_code(code).with_action(action)
}

/// What a `SigV4` signature covers.
pub struct Signing<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    /// Lower-case names; values are trimmed.
    pub headers: &'a [(&'a str, String)],
    pub payload: &'a [u8],
    pub credentials: &'a Credentials,
    pub region: &'a str,
    pub service: &'a str,
    /// `YYYYMMDDTHHMMSSZ`.
    pub amz_date: &'a str,
}

/// The canonical request, the string to sign and the signature.
pub fn sign(s: &Signing<'_>) -> (String, String, String) {
    let mut headers: Vec<(&str, String)> =
        s.headers.iter().map(|(k, v)| (*k, v.split_whitespace().collect::<Vec<_>>().join(" "))).collect();
    headers.sort_by(|a, b| a.0.cmp(b.0));
    let canonical_headers = headers.iter().fold(String::new(), |mut out, (k, v)| {
        let _ = writeln!(out, "{k}:{v}");
        out
    });
    let signed_headers = headers.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(";");
    let canonical = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{}",
        s.method,
        s.path,
        s.query,
        hex(&Sha256::digest(s.payload))
    );
    let date = &s.amz_date[..8.min(s.amz_date.len())];
    let scope = format!("{date}/{}/{}/aws4_request", s.region, s.service);
    let to_sign = format!("AWS4-HMAC-SHA256\n{}\n{scope}\n{}", s.amz_date, hex(&Sha256::digest(canonical.as_bytes())));
    let mut key = hmac(format!("AWS4{}", s.credentials.secret).as_bytes(), date.as_bytes());
    for part in [s.region, s.service, "aws4_request"] {
        key = hmac(&key, part.as_bytes());
    }
    let signature = hex(&hmac(&key, to_sign.as_bytes()));
    (canonical, to_sign, signature)
}

/// The `Authorization` header value.
pub fn authorization(s: &Signing<'_>) -> String {
    let (_, _, signature) = sign(s);
    let mut names: Vec<&str> = s.headers.iter().map(|(k, _)| *k).collect();
    names.sort_unstable();
    let date = &s.amz_date[..8.min(s.amz_date.len())];
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{date}/{}/{}/aws4_request, SignedHeaders={}, Signature={signature}",
        s.credentials.access_key_id,
        s.region,
        s.service,
        names.join(";")
    )
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(key) else { return Vec::new() };
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Credentials {
        Credentials::new("AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", None)
    }

    /// `get-vanilla` from the AWS Signature Version 4 test suite.
    #[test]
    fn aws_test_suite_get_vanilla() {
        let headers = [("host", "example.amazonaws.com".to_owned()), ("x-amz-date", "20150830T123600Z".to_owned())];
        let signing = Signing {
            method: "GET",
            path: "/",
            query: "",
            headers: &headers,
            payload: b"",
            credentials: &example(),
            region: "us-east-1",
            service: "service",
            amz_date: "20150830T123600Z",
        };
        let (canonical, to_sign, signature) = sign(&signing);
        assert_eq!(
            canonical,
            "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\n\
             e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            to_sign,
            "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n\
             bb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63"
        );
        assert_eq!(signature, "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31");
        assert_eq!(
            authorization(&signing),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    /// A `DynamoDB` request: headers sorted, the payload hashed, the scope names the service.
    #[test]
    fn dynamodb_request() {
        let headers = [
            ("x-amz-target", "DynamoDB_20120810.ListTables".to_owned()),
            ("content-type", CONTENT_TYPE.to_owned()),
            ("host", "dynamodb.eu-west-1.amazonaws.com".to_owned()),
            ("x-amz-date", "20261014T101500Z".to_owned()),
            ("x-amz-security-token", "  tok  en ".to_owned()),
        ];
        let creds = Credentials::new("AKIDEXAMPLE", "secret", Some("token".into()));
        let signing = Signing {
            method: "POST",
            path: "/",
            query: "",
            headers: &headers,
            payload: b"{}",
            credentials: &creds,
            region: "eu-west-1",
            service: SERVICE,
            amz_date: "20261014T101500Z",
        };
        let (canonical, _, signature) = sign(&signing);
        assert!(canonical.starts_with(
            "POST\n/\n\ncontent-type:application/x-amz-json-1.0\nhost:dynamodb.eu-west-1.amazonaws.com\n\
             x-amz-date:20261014T101500Z\nx-amz-security-token:tok en\nx-amz-target:DynamoDB_20120810.ListTables\n\n"
        ));
        // SHA-256 of `{}`.
        assert!(canonical.ends_with("44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"));
        assert_eq!(signature.len(), 64);
        let auth = authorization(&signing);
        assert!(auth.contains("Credential=AKIDEXAMPLE/20261014/eu-west-1/dynamodb/aws4_request"));
        assert!(auth.contains("SignedHeaders=content-type;host;x-amz-date;x-amz-security-token;x-amz-target"));
        assert!(!auth.contains("secret"));
        assert!(!format!("{creds:?}").contains("secret") && !format!("{creds:?}").contains("token"));
    }

    #[test]
    fn errors_map_to_kinds() {
        let e = api_error(
            400,
            r#"{"__type":"com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException","message":"x"}"#,
        );
        assert_eq!((e.kind, e.code.as_deref()), (ErrorKind::Constraint, Some("ConditionalCheckFailedException")));
        let e = api_error(400, r#"{"__type":"x#ResourceNotFoundException","message":"Requested resource not found"}"#);
        assert_eq!(e.kind, ErrorKind::NotFound);
        let e = api_error(400, r#"{"__type":"x#UnrecognizedClientException","message":"bad"}"#);
        assert_eq!(e.kind, ErrorKind::Auth);
        let e = api_error(
            400,
            r#"{"__type":"x#TransactionCanceledException","Message":"m","CancellationReasons":[{"Code":"None"},{"Code":"ConditionalCheckFailed","Message":"failed"}]}"#,
        );
        assert!(e.message.contains("statement 2: ConditionalCheckFailed failed"), "{}", e.message);
        let e = api_error(400, r#"{"__type":"x#ProvisionedThroughputExceededException","message":"slow"}"#);
        assert!(retryable(&e));
        assert!(retryable(&api_error(503, "oops")));
        assert!(!retryable(&api_error(400, r#"{"__type":"x#ValidationException","message":"v"}"#)));
    }

    #[test]
    fn credentials_from_process_json() {
        use dq_core::config::ConnConfig;
        let json = r#"{"Version":1,"AccessKeyId":"ASIA1","SecretAccessKey":"s","SessionToken":"t"}"#;
        let p = ConnectParams::new(ConnConfig::default(), Some(json.into()));
        let c = Credentials::from_params(&p).unwrap();
        assert_eq!((c.access_key_id.as_str(), c.session_token.as_deref()), ("ASIA1", Some("t")));
        let p = ConnectParams::new(ConnConfig { username: "AKIA2".into(), ..ConnConfig::default() }, Some("s".into()));
        assert_eq!(Credentials::from_params(&p).unwrap().access_key_id, "AKIA2");
        assert!(Credentials::from_params(&ConnectParams::new(ConnConfig::default(), None)).is_err());
    }
}
