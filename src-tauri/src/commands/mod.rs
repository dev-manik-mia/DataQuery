//! Tauri commands. Each is a thin layer: validate input, call core or store,
//! map to a serialisable response. Every command is async so nothing blocks
//! the UI thread.

pub mod ai;
pub mod connections;
pub mod documents;
pub mod explorer;
pub mod history;
pub mod keys;
pub mod license;
pub mod monitor;
pub mod query;
pub mod schema;
pub mod session;
pub mod settings;
pub mod sync;
pub mod system;
pub mod table;
pub mod transfer;

use dq_core::DbResult;
use serde::Serialize;
use ts_rs::TS;

use crate::safety::ConfirmationRequest;

pub type CmdResult<T> = DbResult<T>;

/// Either the operation's result or a confirmation the user must give first.
#[derive(Debug, Serialize, TS)]
#[serde(tag = "status", rename_all = "snake_case")]
#[ts(export)]
pub enum Guarded<T: TS> {
    NeedsConfirmation { request: ConfirmationRequest },
    Done { value: T },
}
