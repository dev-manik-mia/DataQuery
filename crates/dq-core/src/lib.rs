//! Engine-neutral building blocks for DataQuery.
//!
//! Every database adapter implements [`adapter::DatabaseAdapter`] and
//! [`adapter::Session`] using the types defined here. Anything that crosses the
//! IPC boundary derives [`ts_rs::TS`] so the frontend types are generated from
//! this crate rather than written by hand.

pub mod adapter;
pub mod config;
pub mod diff;
pub mod document;
pub mod edit;
pub mod engine;
pub mod error;
pub mod explain;
pub mod keyvalue;
pub mod migrations;
pub mod monitor;
pub mod node;
pub mod ops;
pub mod redact;
pub mod result;
pub mod schema;
pub mod sql;
pub mod table;
pub mod tls;
pub mod transfer;
pub mod value;

pub use error::{DbError, DbResult, ErrorKind};
