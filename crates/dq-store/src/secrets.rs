//! Password storage. The keychain is the only place a password is persisted.

use std::collections::HashMap;
use std::sync::Mutex;

use dq_core::{DbError, DbResult, ErrorKind};
use serde::Serialize;
use ts_rs::TS;

const SERVICE: &str = "dev.dataquery.app";

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct KeychainStatus {
    pub available: bool,
    /// Platform name, e.g. "macOS Keychain".
    pub backend: String,
    pub detail: Option<String>,
}

pub trait SecretStore: Send + Sync {
    fn get(&self, id: &str) -> DbResult<Option<String>>;
    fn set(&self, id: &str, secret: &str) -> DbResult<()>;
    fn delete(&self, id: &str) -> DbResult<()>;
    fn status(&self) -> KeychainStatus;
}

/// The OS keychain: macOS Keychain, Windows Credential Manager or the Linux Secret Service.
#[derive(Debug, Default)]
pub struct KeyringSecrets;

fn backend_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macOS Keychain"
    } else if cfg!(target_os = "windows") {
        "Windows Credential Manager"
    } else {
        "Secret Service"
    }
}

fn keychain_error(e: keyring::Error) -> DbError {
    // keyring errors never include the secret itself.
    DbError::new(ErrorKind::Keychain, format!("{} error: {e}", backend_name()))
}

fn entry(id: &str) -> DbResult<keyring::Entry> {
    keyring::Entry::new(SERVICE, id).map_err(keychain_error)
}

impl SecretStore for KeyringSecrets {
    fn get(&self, id: &str) -> DbResult<Option<String>> {
        match entry(id)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(keychain_error(e)),
        }
    }

    fn set(&self, id: &str, secret: &str) -> DbResult<()> {
        entry(id)?.set_password(secret).map_err(keychain_error)
    }

    fn delete(&self, id: &str) -> DbResult<()> {
        match entry(id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(keychain_error(e)),
        }
    }

    fn status(&self) -> KeychainStatus {
        let probe = entry("__dataquery_probe__").and_then(|e| match e.get_password() {
            Ok(_) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(keychain_error(e)),
        });
        KeychainStatus {
            available: probe.is_ok(),
            backend: backend_name().to_owned(),
            detail: probe.err().map(|e| e.message),
        }
    }
}

/// In-memory store for tests and for platforms without a keychain.
#[derive(Debug, Default)]
pub struct MemorySecrets(Mutex<HashMap<String, String>>);

impl SecretStore for MemorySecrets {
    fn get(&self, id: &str) -> DbResult<Option<String>> {
        Ok(self.0.lock().expect("secrets lock").get(id).cloned())
    }

    fn set(&self, id: &str, secret: &str) -> DbResult<()> {
        self.0.lock().expect("secrets lock").insert(id.to_owned(), secret.to_owned());
        Ok(())
    }

    fn delete(&self, id: &str) -> DbResult<()> {
        self.0.lock().expect("secrets lock").remove(id);
        Ok(())
    }

    fn status(&self) -> KeychainStatus {
        KeychainStatus { available: true, backend: "In-memory (tests)".into(), detail: None }
    }
}
