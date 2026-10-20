//! Local persistence for DataQuery.
//!
//! Non-secret metadata lives in a SQLite file in the app data directory.
//! Passwords live only in the OS keychain through [`SecretStore`]; no type in
//! this crate that is written to disk has a field that could hold one.

mod files;
pub mod import;
mod model;
pub mod script_files;
mod secrets;
mod store;
pub mod sync;

pub use model::*;
pub use secrets::{KeychainStatus, KeyringSecrets, MemorySecrets, SecretStore};
pub use store::Store;
