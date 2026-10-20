use dq_core::config::{ParsedUrl, parse_connection_url};
use dq_core::engine::{EngineInfo, catalog};
use dq_core::sql::{SqlFlavor, StatementInfo, split_statements};
use dq_core::{DbError, ErrorKind};
use dq_store::KeychainStatus;
use serde::Serialize;
use tauri::State;
use ts_rs::TS;

use super::CmdResult;
use crate::state::{AppState, blocking};

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct AppInfo {
    pub name: &'static str,
    pub version: &'static str,
    pub os: &'static str,
    pub arch: &'static str,
    pub license: &'static str,
}

#[tauri::command]
pub async fn app_info() -> CmdResult<AppInfo> {
    Ok(AppInfo {
        name: "DataQuery",
        version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        // `LicenseRef-Proprietary` in Cargo.toml; shown as "Proprietary".
        license: env!("CARGO_PKG_LICENSE").strip_prefix("LicenseRef-").unwrap_or(env!("CARGO_PKG_LICENSE")),
    })
}

#[tauri::command]
pub async fn list_engines(state: State<'_, AppState>) -> CmdResult<Vec<EngineInfo>> {
    Ok(catalog(|e| state.registry.capabilities(e)))
}

#[tauri::command]
pub async fn keychain_status(state: State<'_, AppState>) -> CmdResult<KeychainStatus> {
    let secrets = state.secrets.clone();
    blocking(move || Ok(secrets.status())).await
}

const MAX_SQL_FILE_BYTES: u64 = 8 * 1024 * 1024;

fn sql_file_path(path: &str) -> CmdResult<std::path::PathBuf> {
    let path = std::path::PathBuf::from(path);
    if !path.is_absolute() {
        return Err(DbError::invalid("Choose a file with the file dialog."));
    }
    Ok(path)
}

/// Reads a `.sql` file chosen in the open dialog or dropped on the window.
#[tauri::command]
pub async fn read_sql_file(path: String) -> CmdResult<String> {
    let path = sql_file_path(&path)?;
    blocking(move || {
        let io = |e: std::io::Error| DbError::new(ErrorKind::Storage, format!("Cannot read {}: {e}", path.display()));
        if std::fs::metadata(&path).map_err(io)?.len() > MAX_SQL_FILE_BYTES {
            return Err(DbError::invalid("SQL files larger than 8 MB cannot be opened in the editor."));
        }
        let bytes = std::fs::read(&path).map_err(io)?;
        String::from_utf8(bytes).map_err(|_| DbError::invalid("The file is not UTF-8 text."))
    })
    .await
}

/// Writes editor text to a file chosen in the save dialog.
#[tauri::command]
pub async fn write_sql_file(path: String, contents: String) -> CmdResult<()> {
    let path = sql_file_path(&path)?;
    blocking(move || {
        std::fs::write(&path, contents)
            .map_err(|e| DbError::new(ErrorKind::Storage, format!("Cannot write {}: {e}", path.display())))
    })
    .await
}

/// Statement boundaries and safety flags for the editor.
#[tauri::command]
pub async fn analyze_sql(sql: String, flavor: SqlFlavor) -> CmdResult<Vec<StatementInfo>> {
    Ok(split_statements(&sql, flavor))
}

/// Basic formatter: keywords upper-cased, one clause per line.
#[tauri::command]
pub async fn format_sql(sql: String, flavor: SqlFlavor) -> CmdResult<String> {
    Ok(dq_core::sql::format::format_sql(&sql, flavor))
}

/// Parses a connection URL. The password is returned for the form only and is
/// stored in the keychain when the connection is saved, never in the URL.
#[tauri::command]
pub async fn parse_url(url: String) -> CmdResult<ParsedUrl> {
    parse_connection_url(&url)
}
