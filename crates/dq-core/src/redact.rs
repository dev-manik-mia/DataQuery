//! Removes secrets from text before it is logged, stored or displayed.

use std::sync::LazyLock;

use regex::Regex;

static URL_CREDENTIALS: LazyLock<Regex> = LazyLock::new(|| {
    // scheme://user:password@host  →  scheme://user:***@host
    Regex::new(r"(?i)\b([a-z][a-z0-9+.\-]*://[^:/@\s]*):[^@\s]*@").expect("valid regex")
});

static KEY_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    // password=..., pwd: ..., token=..., secret='...', api_key="..."
    Regex::new(
        r#"(?i)\b(password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key)(\s*[=:]\s*)("[^"]*"|'[^']*'|[^\s;&,]+)"#,
    )
    .expect("valid regex")
});

static SQL_PASSWORD: LazyLock<Regex> = LazyLock::new(|| {
    // CREATE ROLE x PASSWORD 'secret' / ALTER USER x IDENTIFIED BY 'secret'
    Regex::new(r#"(?i)\b(password|identified\s+by)(\s+)('(?:[^']|'')*'|"[^"]*")"#).expect("valid regex")
});

static BEARER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bbearer\s+[a-z0-9._\-]+").expect("valid regex"));

pub const MASK: &str = "***";

/// Returns `text` with credentials in URLs, key/value pairs and bearer tokens masked.
pub fn redact(text: &str) -> String {
    let text = URL_CREDENTIALS.replace_all(text, format!("$1:{MASK}@"));
    let text = KEY_VALUE.replace_all(&text, format!("$1$2{MASK}"));
    let text = SQL_PASSWORD.replace_all(&text, format!("$1$2'{MASK}'"));
    BEARER.replace_all(&text, format!("Bearer {MASK}")).into_owned()
}

/// Masks every occurrence of a known secret, then applies [`redact`].
pub fn redact_known(text: &str, secret: &str) -> String {
    if secret.len() >= 3 { redact(&text.replace(secret, MASK)) } else { redact(text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_url_password() {
        assert_eq!(
            redact("failed: postgres://app:s3cr3t@db.local:5432/app"),
            "failed: postgres://app:***@db.local:5432/app"
        );
        assert_eq!(redact("mysql://root:@localhost"), "mysql://root:***@localhost");
    }

    #[test]
    fn masks_key_values() {
        let out = redact("host=db password=hunter2 user=app token='abc def' api_key=\"k\"");
        assert!(!out.contains("hunter2"));
        assert!(!out.contains("abc def"));
        assert!(out.contains("password=***"));
        assert!(out.contains("api_key=***"));
        assert!(out.contains("user=app"));
    }

    #[test]
    fn masks_sql_passwords() {
        assert_eq!(redact("CREATE ROLE app LOGIN PASSWORD 'it''s'"), "CREATE ROLE app LOGIN PASSWORD '***'");
        assert_eq!(redact("ALTER USER x IDENTIFIED BY \"p\";"), "ALTER USER x IDENTIFIED BY '***';");
    }

    #[test]
    fn masks_bearer_tokens() {
        assert_eq!(redact("Authorization: Bearer eyJ.abc-1"), "Authorization: Bearer ***");
    }

    #[test]
    fn masks_known_secret_anywhere() {
        assert_eq!(redact_known("auth failed for pa55word!", "pa55word!"), "auth failed for ***");
    }

    #[test]
    fn leaves_plain_text_alone() {
        let s = "column \"total\" does not exist";
        assert_eq!(redact(s), s);
    }
}
