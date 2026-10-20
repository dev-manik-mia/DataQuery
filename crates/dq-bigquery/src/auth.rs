//! OAuth 2.0 access tokens for the `BigQuery` API, from a service-account key
//! (an RS256-signed JWT exchanged at the token endpoint), an `authorized_user`
//! file (the refresh token `gcloud auth application-default login` writes) or
//! the Google Cloud CLI. The key file is read when the connection opens and
//! never copied; tokens stay in memory until shortly before they expire and
//! are never logged or stored.

use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use dq_core::adapter::ConnectParams;
use dq_core::config::AuthMethod;
use dq_core::{DbError, DbResult, ErrorKind};
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

pub const SCOPE: &str = "https://www.googleapis.com/auth/bigquery";
const TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
/// A token is renewed this long before it expires.
const MARGIN: Duration = Duration::from_secs(120);
/// `gcloud` does not say when its token expires; it is asked again after this.
const GCLOUD_TTL: Duration = Duration::from_secs(300);
/// Lifetime requested for a signed assertion (Google's maximum).
const ASSERTION_SECS: i64 = 3600;

enum Source {
    /// An emulator endpoint that takes no credentials.
    Anonymous,
    ServiceAccount {
        email: String,
        key_id: Option<String>,
        key: Box<RsaKeyPair>,
        token_uri: String,
    },
    AuthorizedUser {
        client_id: String,
        client_secret: String,
        refresh_token: String,
        token_uri: String,
    },
    Gcloud,
}

/// Where tokens come from, and the current one.
pub struct Credentials {
    source: Source,
    /// One caller refreshes at a time; the others wait for its token.
    cached: Mutex<Option<(String, Instant)>>,
}

#[derive(Deserialize)]
struct KeyFile {
    #[serde(rename = "type")]
    kind: String,
    client_email: Option<String>,
    private_key: Option<String>,
    private_key_id: Option<String>,
    token_uri: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    expires_in: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

fn auth_error(message: impl AsRef<str>) -> DbError {
    DbError::new(ErrorKind::Auth, message)
}

fn key_error(what: &str) -> DbError {
    auth_error(format!("The key file is not a usable Google credentials file: {what}."))
        .with_action(Some("Download a JSON key for the service account from the Google Cloud console."))
}

/// The DER bytes of a PEM `PRIVATE KEY` block (PKCS #8, as service-account keys are).
pub fn pem_to_der(pem: &str) -> DbResult<Vec<u8>> {
    let body = pem
        .split("-----BEGIN PRIVATE KEY-----")
        .nth(1)
        .and_then(|rest| rest.split("-----END PRIVATE KEY-----").next())
        .ok_or_else(|| key_error("no PKCS #8 private key"))?;
    let base64: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    STANDARD.decode(base64).map_err(|_| key_error("the private key is not valid base64"))
}

/// The token endpoint named in a key file: HTTPS only, since the file's secrets are sent there.
fn token_uri(uri: Option<String>) -> DbResult<String> {
    let uri = uri.unwrap_or_else(|| TOKEN_URI.to_owned());
    if uri.starts_with("https://") { Ok(uri) } else { Err(key_error("the token_uri is not an HTTPS address")) }
}

fn parse_key_file(text: &str) -> DbResult<Source> {
    let file: KeyFile = serde_json::from_str(text).map_err(|_| key_error("it is not JSON"))?;
    match file.kind.as_str() {
        "service_account" => {
            let email = file.client_email.ok_or_else(|| key_error("client_email is missing"))?;
            let pem = file.private_key.ok_or_else(|| key_error("private_key is missing"))?;
            let key = RsaKeyPair::from_pkcs8(&pem_to_der(&pem)?)
                .map_err(|_| key_error("the private key is not an RSA key"))?;
            Ok(Source::ServiceAccount {
                email,
                key_id: file.private_key_id,
                key: Box::new(key),
                token_uri: token_uri(file.token_uri)?,
            })
        }
        "authorized_user" => Ok(Source::AuthorizedUser {
            client_id: file.client_id.ok_or_else(|| key_error("client_id is missing"))?,
            client_secret: file.client_secret.ok_or_else(|| key_error("client_secret is missing"))?,
            refresh_token: file.refresh_token.ok_or_else(|| key_error("refresh_token is missing"))?,
            token_uri: token_uri(file.token_uri)?,
        }),
        other => Err(key_error(&format!("credentials of type `{other}` are not supported"))),
    }
}

/// A signed JWT that asks for a `BigQuery` token as the service account, valid from `now` (Unix seconds).
pub fn assertion(email: &str, key_id: Option<&str>, key: &RsaKeyPair, audience: &str, now: i64) -> DbResult<String> {
    let mut header = json!({ "alg": "RS256", "typ": "JWT" });
    if let Some(kid) = key_id {
        header["kid"] = json!(kid);
    }
    let claims = json!({ "iss": email, "scope": SCOPE, "aud": audience, "iat": now, "exp": now + ASSERTION_SECS });
    let signing_input =
        format!("{}.{}", URL_SAFE_NO_PAD.encode(header.to_string()), URL_SAFE_NO_PAD.encode(claims.to_string()));
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(&RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), signing_input.as_bytes(), &mut signature)
        .map_err(|_| DbError::internal("could not sign the token request"))?;
    Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature)))
}

impl Credentials {
    /// Reads the connection's credentials: the key file, the Google Cloud CLI, or
    /// none for an emulator endpoint.
    pub async fn open(params: &ConnectParams) -> DbResult<Self> {
        let config = &params.config;
        let source = match (&config.auth, config.property("key_file")) {
            (AuthMethod::GcpIam, _) => Source::Gcloud,
            (_, Some(path)) => {
                let text = tokio::fs::read_to_string(path).await.map_err(|e| {
                    auth_error(format!("The key file could not be read ({}).", e.kind()))
                        .with_action(Some("Choose the service account's JSON key file again."))
                })?;
                parse_key_file(&text)?
            }
            (_, None) => Source::Anonymous,
        };
        Ok(Self { source, cached: Mutex::new(None) })
    }

    /// The bearer token for the next request; `None` against an emulator.
    pub async fn token(&self, http: &reqwest::Client) -> DbResult<Option<String>> {
        if matches!(self.source, Source::Anonymous) {
            return Ok(None);
        }
        let mut cached = self.cached.lock().await;
        if let Some((token, expires)) = cached.as_ref()
            && Instant::now() + MARGIN < *expires
        {
            return Ok(Some(token.clone()));
        }
        let (token, lifetime) = self.fetch(http).await?;
        *cached = Some((token.clone(), Instant::now() + lifetime));
        Ok(Some(token))
    }

    /// Drops the current token, after the API refused it.
    pub async fn invalidate(&self) {
        *self.cached.lock().await = None;
    }

    pub fn is_anonymous(&self) -> bool {
        matches!(self.source, Source::Anonymous)
    }

    /// The account the credentials name (a service account's email), for display.
    pub fn account(&self) -> Option<&str> {
        match &self.source {
            Source::ServiceAccount { email, .. } => Some(email),
            _ => None,
        }
    }

    async fn fetch(&self, http: &reqwest::Client) -> DbResult<(String, Duration)> {
        let (uri, form): (&str, Vec<(&str, String)>) = match &self.source {
            Source::Anonymous => return Err(DbError::internal("no credentials")),
            Source::Gcloud => {
                let token = dq_core::programs::cloud_cli_token(
                    "gcloud",
                    &["auth", "print-access-token"],
                    "Install the Google Cloud CLI and run `gcloud auth login`.",
                )
                .await?;
                return Ok((token, GCLOUD_TTL + MARGIN));
            }
            Source::ServiceAccount { email, key_id, key, token_uri } => {
                let now = chrono::Utc::now().timestamp();
                let jwt = assertion(email, key_id.as_deref(), key, token_uri, now)?;
                (
                    token_uri,
                    vec![("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer".into()), ("assertion", jwt)],
                )
            }
            Source::AuthorizedUser { client_id, client_secret, refresh_token, token_uri } => (
                token_uri,
                vec![
                    ("grant_type", "refresh_token".into()),
                    ("client_id", client_id.clone()),
                    ("client_secret", client_secret.clone()),
                    ("refresh_token", refresh_token.clone()),
                ],
            ),
        };
        let response = http.post(uri).form(&form).send().await.map_err(|e| {
            DbError::new(
                if e.is_timeout() { ErrorKind::Timeout } else { ErrorKind::Network },
                "The Google token endpoint could not be reached.",
            )
        })?;
        let answer: TokenResponse =
            response.json().await.map_err(|_| auth_error("The Google token endpoint sent an unreadable answer."))?;
        if let Some(token) = answer.access_token {
            return Ok((token, Duration::from_secs(answer.expires_in.unwrap_or(3600))));
        }
        let reason = answer.error_description.or(answer.error).unwrap_or_else(|| "no token".into());
        Err(auth_error(format!("Google refused the credentials: {reason}"))
            .with_action(Some("Check that the key is active and the account may use BigQuery.")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{KeyPair, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    const KEY_FILE: &str = include_str!("../testdata/service-account.json");

    #[test]
    fn signs_a_verifiable_assertion() {
        let Source::ServiceAccount { email, key_id, key, token_uri } = parse_key_file(KEY_FILE).unwrap() else {
            panic!("a service account");
        };
        assert_eq!(token_uri, TOKEN_URI);
        let jwt = assertion(&email, key_id.as_deref(), &key, &token_uri, 1_700_000_000).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let decode =
            |p: &str| serde_json::from_slice::<serde_json::Value>(&URL_SAFE_NO_PAD.decode(p).unwrap()).unwrap();
        assert_eq!(decode(parts[0]), json!({ "alg": "RS256", "typ": "JWT", "kid": "test-key-1" }));
        let claims = decode(parts[1]);
        assert_eq!(claims["iss"], "tester@dataquery-test.iam.gserviceaccount.com");
        assert_eq!((claims["scope"].as_str(), claims["aud"].as_str()), (Some(SCOPE), Some(TOKEN_URI)));
        assert_eq!(claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(), 3600);
        let public = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.public_key().as_ref());
        let signed = format!("{}.{}", parts[0], parts[1]);
        public.verify(signed.as_bytes(), &URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).expect("valid signature");
    }

    #[test]
    fn rejects_unusable_key_files() {
        assert!(parse_key_file("not json").is_err());
        assert!(parse_key_file(r#"{"type": "external_account"}"#).is_err());
        assert!(parse_key_file(r#"{"type": "service_account", "client_email": "a@b", "private_key": "x"}"#).is_err());
        let insecure = KEY_FILE.replace("https://oauth2.googleapis.com/token", "http://example.com/token");
        assert!(parse_key_file(&insecure).is_err());
        let user = r#"{"type": "authorized_user", "client_id": "c", "client_secret": "s", "refresh_token": "r"}"#;
        assert!(matches!(parse_key_file(user), Ok(Source::AuthorizedUser { .. })));
        // Errors never repeat the file's contents.
        let error = parse_key_file(r#"{"type": "secret-value"}"#).err().unwrap();
        assert!(error.message.contains("secret-value"), "the type is named, not a secret");
        let bad_key = r#"{"type": "service_account", "client_email": "a@b", "private_key": "-----BEGIN PRIVATE KEY-----\nc2VjcmV0\n-----END PRIVATE KEY-----"}"#;
        assert!(!parse_key_file(bad_key).err().unwrap().message.contains("c2VjcmV0"));
    }
}
