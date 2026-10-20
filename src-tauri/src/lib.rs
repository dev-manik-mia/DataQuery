//! DataQuery desktop application: Tauri setup and command registration.

mod ai;
mod ai_client;
mod cloud_auth;
mod commands;
mod connector;
mod folder_sync;
mod license;
mod registry;
mod safety;
mod state;

use std::sync::Arc;

use dq_store::{KeyringSecrets, Store};
use tauri::menu::{AboutMetadata, Menu, PredefinedMenuItem, Submenu};
use tauri::{Emitter, Manager};

use crate::state::AppState;

pub fn run() {
    dq_core::tls::install_crypto_provider();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "dataquery=info,dq_=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let store = Store::open(&data_dir.join("dataquery.db")).map_err(|e| e.message)?;
            let known_hosts = dq_ssh::KnownHosts::new(data_dir.join("known_hosts"));
            app.manage(AppState::new(store, Arc::new(KeyringSecrets), known_hosts));
            app.manage(folder_sync::FolderSync::default());
            folder_sync::start(app.handle());
            app.manage(commands::data_compare::DataCompares::default());
            app.set_menu(menu(app.handle())?)?;
            spawn_idle_reaper(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::ai::ai_disclosure,
            commands::license::license_status,
            commands::license::license_activate,
            commands::license::license_deactivate,
            commands::license::open_pricing,
            commands::ai::ai_ask,
            commands::ai::ai_providers,
            commands::ai::ai_models,
            commands::ai::ai_key_status,
            commands::ai::ai_set_key,
            commands::system::app_info,
            commands::system::list_engines,
            commands::system::keychain_status,
            commands::system::analyze_sql,
            commands::system::format_sql,
            commands::system::parse_url,
            commands::system::read_sql_file,
            commands::system::write_sql_file,
            commands::settings::get_settings,
            commands::settings::save_settings,
            commands::settings::get_ui_state,
            commands::settings::set_ui_state,
            commands::connections::list_connections,
            commands::connections::save_connection,
            commands::connections::delete_connection,
            commands::connections::duplicate_connection,
            commands::connections::set_favorite,
            commands::connections::create_folder,
            commands::connections::rename_folder,
            commands::connections::delete_folder,
            commands::connections::test_connection,
            commands::connections::draft_databases,
            commands::connections::trust_host_key,
            commands::connections::export_connections,
            commands::connections::import_connections,
            commands::connections::detect_connection_sources,
            commands::connections::read_connection_source,
            commands::connections::import_connection_list,
            commands::session::connect,
            commands::session::reconnect,
            commands::session::disconnect,
            commands::explorer::list_children,
            commands::explorer::search_objects,
            commands::explorer::qualified_name,
            commands::explorer::generate_sql,
            commands::explorer::object_dependencies,
            commands::explorer::schema_model,
            commands::query::run_sql,
            commands::query::fetch_more,
            commands::query::cancel_query,
            commands::query::transaction,
            commands::query::explain,
            commands::monitor::monitor_snapshot,
            commands::monitor::server_settings,
            commands::monitor::largest_tables,
            commands::monitor::kill_process,
            commands::users::user_accounts,
            commands::users::user_privileges,
            commands::users::user_change_preview,
            commands::users::apply_user_change,
            commands::schema::schema_change_preview,
            commands::schema::apply_schema_changes,
            commands::schema::schema_diff,
            commands::schema::framework_migrations,
            commands::data_compare::data_compare_plan,
            commands::data_compare::data_compare,
            commands::data_compare::data_compare_sql,
            commands::data_compare::data_compare_save,
            commands::data_compare::data_compare_apply,
            commands::data_compare::data_compare_discard,
            commands::table::table_structure,
            commands::table::table_ddl,
            commands::table::table_page,
            commands::table::row_count,
            commands::table::preview_changes,
            commands::table::apply_changes,
            commands::table::cell_value,
            commands::table::save_cell,
            commands::table::key_roles,
            commands::query::query_analysis,
            commands::query::session_options,
            commands::query::set_session_option,
            commands::documents::documents_find,
            commands::documents::documents_count,
            commands::documents::documents_aggregate,
            commands::documents::documents_indexes,
            commands::documents::documents_validation,
            commands::documents::documents_statement,
            commands::documents::documents_write,
            commands::keys::keys_scan,
            commands::keys::keys_count,
            commands::keys::keys_value,
            commands::keys::keys_write,
            commands::keys::keys_console,
            commands::transfer::export_data,
            commands::transfer::import_preview,
            commands::transfer::import_data,
            commands::transfer::cancel_transfer,
            commands::backup::backup_tools,
            commands::backup::backup_plan,
            commands::backup::backup_database,
            commands::backup::restore_database,
            commands::history::list_history,
            commands::history::clear_history,
            commands::history::apply_history_retention,
            commands::history::list_scripts,
            commands::history::save_script,
            commands::history::update_script_sql,
            commands::history::delete_script,
            commands::history::list_snippets,
            commands::history::save_snippet,
            commands::history::delete_snippet,
            commands::sync::sync_status,
            commands::sync::sync_configure,
            commands::sync::sync_disable,
            commands::sync::sync_now,
            commands::sync::scripts_folder_status,
            commands::sync::set_scripts_folder,
            commands::sync::scripts_folder_sync,
        ])
        .run(tauri::generate_context!())
        .expect("error while running DataQuery");
}

/// Event sent to every window when a session is closed by the backend.
pub const SESSION_CLOSED_EVENT: &str = "session-closed";

/// Closes sessions that passed their idle timeout and tells the windows.
fn spawn_idle_reaper(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            tick.tick().await;
            for session_id in app.state::<AppState>().close_idle().await {
                tracing::info!(%session_id, "closed idle session");
                let _ =
                    app.emit(SESSION_CLOSED_EVENT, serde_json::json!({ "session_id": session_id, "reason": "idle" }));
            }
        }
    });
}

/// Standard menus without "Close Window": ⌘W and ⌘T belong to editor tabs.
fn menu(app: &tauri::AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let about = AboutMetadata {
        name: Some("DataQuery".into()),
        version: Some(env!("CARGO_PKG_VERSION").into()),
        ..AboutMetadata::default()
    };
    Menu::with_items(
        app,
        &[
            &Submenu::with_items(
                app,
                "DataQuery",
                true,
                &[
                    &PredefinedMenuItem::about(app, None, Some(about))?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::hide(app, None)?,
                    &PredefinedMenuItem::hide_others(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::quit(app, None)?,
                ],
            )?,
            &Submenu::with_items(
                app,
                "Edit",
                true,
                &[
                    &PredefinedMenuItem::undo(app, None)?,
                    &PredefinedMenuItem::redo(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::cut(app, None)?,
                    &PredefinedMenuItem::copy(app, None)?,
                    &PredefinedMenuItem::paste(app, None)?,
                    &PredefinedMenuItem::select_all(app, None)?,
                ],
            )?,
            &Submenu::with_items(
                app,
                "Window",
                true,
                &[
                    &PredefinedMenuItem::minimize(app, None)?,
                    &PredefinedMenuItem::maximize(app, None)?,
                    &PredefinedMenuItem::fullscreen(app, None)?,
                ],
            )?,
        ],
    )
}
