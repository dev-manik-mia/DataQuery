//! DataQuery Pro licences: an Ed25519-signed token verified offline.
//!
//! A purchase on Lemon Squeezy gives a licence key. The app exchanges it once at
//! the activation endpoint (`DataQueryWebsite/functions/api/license`), which
//! activates a seat with Lemon Squeezy and returns a signed token; after that the
//! token is checked offline on every launch. Tokens can also be pasted directly
//! (`DQ1.…`, issued with `scripts/license-token.mjs`) for machines without internet.
//!
//! Only official builds (`DATAQUERY_OFFICIAL_BUILD` set at compile time) apply the
//! Free limits; a development build from source has every feature.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use dq_core::{DbError, DbResult, ErrorKind};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Public half of the signing key; the private half never enters the repository.
const PUBLIC_KEY: [u8; 32] = [
    0x89, 0x6c, 0xfa, 0x31, 0xfa, 0x5c, 0xe6, 0xfd, 0x77, 0x5a, 0xed, 0x83, 0x1e, 0xfd, 0xce, 0x51, 0x7b, 0xf2, 0x80,
    0x1f, 0x05, 0xe8, 0x69, 0xba, 0x21, 0x8a, 0x3d, 0xd4, 0xbd, 0xd7, 0xfc, 0xd7,
];

pub const TOKEN_PREFIX: &str = "DQ1.";
pub const ACTIVATE_URL: &str = "https://dataquery.manikmia.com/api/license/activate";
pub const PRICING_URL: &str = "https://dataquery.manikmia.com/#pricing";
pub const DEACTIVATE_URL: &str = "https://dataquery.manikmia.com/api/license/deactivate";
const MAX_TOKEN_LEN: usize = 4096;

/// Free applies limits only in official builds; `DATAQUERY_FREE_PLAN=1` previews them in a dev build.
pub fn enforced() -> bool {
    option_env!("DATAQUERY_OFFICIAL_BUILD").is_some() || std::env::var_os("DATAQUERY_FREE_PLAN").is_some()
}

/// What Free allows (`TablePlus` Free allows 2 of each).
#[derive(Debug, Clone, Copy, Serialize, TS)]
#[ts(export)]
pub struct FreeLimits {
    pub tabs: u32,
    pub windows: u32,
    pub filters: u32,
}

pub const FREE_LIMITS: FreeLimits = FreeLimits { tabs: 4, windows: 4, filters: 4 };

/// The signed part of a token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct LicenseClaims {
    pub v: u8,
    pub plan: String,
    pub name: String,
    pub email: String,
    /// Lemon Squeezy order number, or an id for hand-issued tokens.
    pub order: String,
    /// Lemon Squeezy activation instance, needed to free the seat again.
    #[serde(default)]
    pub instance: Option<String>,
    /// Issue date, `YYYY-MM-DD`.
    pub issued: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Plan {
    Free,
    Pro,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct LicenseStatus {
    pub plan: Plan,
    /// Built from source: every feature is on and no licence is needed.
    pub source_build: bool,
    pub claims: Option<LicenseClaims>,
    /// Present while Free limits apply.
    pub limits: Option<FreeLimits>,
}

impl LicenseStatus {
    pub fn new(claims: Option<LicenseClaims>) -> Self {
        let source_build = !enforced();
        let plan = if source_build || claims.is_some() { Plan::Pro } else { Plan::Free };
        Self { plan, source_build, limits: (plan == Plan::Free).then_some(FREE_LIMITS), claims }
    }

    /// Refuses a Pro feature on the Free plan.
    pub fn require_pro(&self, feature: &str) -> DbResult<()> {
        if self.plan == Plan::Pro {
            return Ok(());
        }
        Err(DbError::new(ErrorKind::Unsupported, format!("{feature} is part of DataQuery Pro."))
            .with_action(Some("Enter a licence key in Settings → License, or buy Pro once for lifetime updates.")))
    }
}

/// Checks a `DQ1.<payload>.<signature>` token and returns its claims.
pub fn verify(token: &str) -> DbResult<LicenseClaims> {
    verify_with(token, &PUBLIC_KEY)
}

fn verify_with(token: &str, public_key: &[u8; 32]) -> DbResult<LicenseClaims> {
    let invalid = || DbError::invalid("This licence token is not valid. Copy it again from your purchase email.");
    let token = token.trim();
    if token.len() > MAX_TOKEN_LEN {
        return Err(invalid());
    }
    let body = token.strip_prefix(TOKEN_PREFIX).ok_or_else(invalid)?;
    let (payload, signature) = body.split_once('.').ok_or_else(invalid)?;
    let signature: [u8; 64] =
        URL_SAFE_NO_PAD.decode(signature).ok().and_then(|s| s.try_into().ok()).ok_or_else(invalid)?;
    let key = VerifyingKey::from_bytes(public_key).map_err(|_| invalid())?;
    key.verify_strict(payload.as_bytes(), &Signature::from_bytes(&signature)).map_err(|_| invalid())?;
    let claims: LicenseClaims =
        URL_SAFE_NO_PAD.decode(payload).ok().and_then(|p| serde_json::from_slice(&p).ok()).ok_or_else(invalid)?;
    if claims.v != 1 || claims.plan != "pro" {
        return Err(invalid());
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};

    use super::*;

    fn token(key: &SigningKey, claims: &serde_json::Value) -> String {
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        let signature = URL_SAFE_NO_PAD.encode(key.sign(payload.as_bytes()).to_bytes());
        format!("{TOKEN_PREFIX}{payload}.{signature}")
    }

    fn claims() -> serde_json::Value {
        serde_json::json!({ "v": 1, "plan": "pro", "name": "Ada", "email": "ada@example.com", "order": "1001", "instance": "i-1", "issued": "2026-10-20" })
    }

    #[test]
    fn accepts_signed_tokens_and_rejects_tampering() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let public = key.verifying_key().to_bytes();
        let good = token(&key, &claims());
        let parsed = verify_with(&good, &public).unwrap();
        assert_eq!(parsed.name, "Ada");
        assert_eq!(parsed.instance.as_deref(), Some("i-1"));
        assert!(verify_with(&format!("  {good}\n"), &public).is_ok());

        // Same signature, different payload.
        let (_, signature) = good.rsplit_once('.').unwrap();
        let mut other = claims();
        other["name"] = "Mallory".into();
        let forged = format!("{TOKEN_PREFIX}{}.{signature}", URL_SAFE_NO_PAD.encode(other.to_string()));
        assert!(verify_with(&forged, &public).is_err());

        // Another key, the shipped key, garbage.
        let stranger = SigningKey::from_bytes(&[9; 32]);
        assert!(verify_with(&token(&stranger, &claims()), &public).is_err());
        assert!(verify(&good).is_err());
        for bad in ["", "DQ1.", "DQ1.abc", "DQ2.a.b", "DQ1.a.b.c", &"x".repeat(MAX_TOKEN_LEN + 1)] {
            assert!(verify_with(bad, &public).is_err(), "{bad}");
        }
    }

    #[test]
    fn rejects_other_plans_and_versions() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let public = key.verifying_key().to_bytes();
        for (field, value) in [("plan", serde_json::json!("team")), ("v", serde_json::json!(2))] {
            let mut c = claims();
            c[field] = value;
            assert!(verify_with(&token(&key, &c), &public).is_err());
        }
    }

    /// A token from `scripts/license-token.mjs` signed with the real key (never committed).
    #[test]
    fn verifies_a_token_issued_with_the_real_key() {
        let Ok(token) = std::env::var("DATAQUERY_TEST_LICENSE_TOKEN") else { return };
        assert_eq!(verify(&token).unwrap().plan, "pro");
    }

    #[test]
    fn shipped_public_key_is_a_valid_point() {
        assert!(VerifyingKey::from_bytes(&PUBLIC_KEY).is_ok());
    }

    #[test]
    fn free_limits_only_without_a_licence_in_enforced_builds() {
        let status = LicenseStatus::new(None);
        if enforced() {
            assert_eq!(status.plan, Plan::Free);
            assert!(status.require_pro("Schema diff").is_err());
        } else {
            assert!(status.source_build);
            assert!(status.limits.is_none());
            assert!(status.require_pro("Schema diff").is_ok());
        }
    }
}
