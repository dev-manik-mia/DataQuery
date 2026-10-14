//! DataQuery desktop application: Tauri setup and command registration.

mod commands;
mod registry;
mod safety;
mod state;

use std::sync::Arc;

use dq_store::{KeyringSecrets, Store};
use tauri::Manager;
use tauri::menu::{AboutMetadata, Menu, PredefinedMenuItem, Submenu};

use crate::state::AppState;

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "dataquery=info,dq_=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let store = Store::open(&data_dir.join("dataquery.db")).map_err(|e| e.message)?;
            app.manage(AppState::new(store, Arc::new(KeyringSecrets)));
            app.set_menu(menu(app.handle())?)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
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
            commands::session::connect,
            commands::session::disconnect,
            commands::explorer::list_children,
            commands::explorer::search_objects,
            commands::explorer::qualified_name,
            commands::explorer::generate_select,
            commands::query::run_sql,
            commands::query::fetch_more,
            commands::query::cancel_query,
            commands::table::table_structure,
            commands::table::table_ddl,
            commands::table::table_page,
            commands::table::row_count,
            commands::table::preview_changes,
            commands::table::apply_changes,
            commands::transfer::export_data,
            commands::transfer::csv_preview,
            commands::transfer::import_csv,
            commands::history::list_history,
            commands::history::clear_history,
            commands::history::list_scripts,
            commands::history::save_script,
            commands::history::delete_script,
        ])
        .run(tauri::generate_context!())
        .expect("error while running DataQuery");
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
