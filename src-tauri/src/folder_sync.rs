//! Background work for user-chosen folders: the encrypted connection sync file
//! and the scripts folder. Both run on blocking threads, are serialised by a
//! lock each, are triggered by a file watcher (debounced) as well as by the
//! app, and report to every window through an event.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dq_core::DbResult;
use dq_store::script_files::{self, ScriptsFolderReport};
use dq_store::sync::{self, KdfParams, SyncKey};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use ts_rs::TS;

use crate::state::{AppState, blocking, now_ms};

/// Event with a [`SyncStatus`] after every connection sync.
pub const SYNC_EVENT: &str = "sync-status";
/// Event with a [`ScriptsFolderStatus`] after every scripts-folder reconcile.
pub const SCRIPTS_EVENT: &str = "scripts-folder";

const DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Default, Serialize, TS)]
#[ts(export)]
pub struct SyncStatus {
    /// The sync folder; `None` when sync is off.
    pub folder: Option<String>,
    /// A passphrase for this device is in the keychain.
    pub has_passphrase: bool,
    pub running: bool,
    #[ts(type = "number | null")]
    pub last_synced_at: Option<i64>,
    pub error: Option<String>,
    /// Items that could not be applied here, unreadable conflicted copies.
    pub warnings: Vec<String>,
    /// The last sync changed this device's folders or connections.
    pub changed: bool,
    pub connections: u32,
}

#[derive(Debug, Clone, Default, Serialize, TS)]
#[ts(export)]
pub struct ScriptsFolderStatus {
    /// The scripts folder; `None` when off.
    pub folder: Option<String>,
    pub report: Option<ScriptsFolderReport>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct FolderSync {
    sync_lock: tokio::sync::Mutex<()>,
    scripts_lock: tokio::sync::Mutex<()>,
    scripts_pending: AtomicBool,
    /// Derived from the passphrase; dropped when the passphrase changes.
    key: Mutex<Option<SyncKey>>,
    sync_status: Mutex<SyncStatus>,
    scripts_status: Mutex<ScriptsFolderStatus>,
    sync_watcher: Mutex<Option<RecommendedWatcher>>,
    scripts_watcher: Mutex<Option<RecommendedWatcher>>,
}

fn non_empty(path: &str) -> Option<String> {
    Some(path.to_owned()).filter(|p| !p.is_empty())
}

impl FolderSync {
    pub fn forget_key(&self) {
        self.key.lock().expect("sync key lock").take();
    }

    pub fn sync_status(&self) -> SyncStatus {
        self.sync_status.lock().expect("sync status lock").clone()
    }

    pub fn scripts_status(&self) -> ScriptsFolderStatus {
        self.scripts_status.lock().expect("scripts status lock").clone()
    }

    fn set_sync_status(&self, app: &AppHandle, status: SyncStatus) {
        *self.sync_status.lock().expect("sync status lock") = status.clone();
        let _ = app.emit(SYNC_EVENT, status);
    }

    fn set_scripts_status(&self, app: &AppHandle, status: ScriptsFolderStatus) {
        *self.scripts_status.lock().expect("scripts status lock") = status.clone();
        let _ = app.emit(SCRIPTS_EVENT, status);
    }
}

/// Starts watching the configured folders and runs a first sync of each.
pub fn start(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        rewatch(&app).await;
        run_sync(&app).await;
        run_scripts(&app).await;
    });
}

/// Watches `path` and calls `on_change` once things have been quiet for
/// [`DEBOUNCE`]. Dropping the watcher ends the debounce thread.
fn watch(
    path: &Path,
    mode: RecursiveMode,
    relevant: impl Fn(&Path) -> bool + Send + 'static,
    on_change: impl Fn() + Send + 'static,
) -> notify::Result<RecommendedWatcher> {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok_and(|e| !e.kind.is_access() && e.paths.iter().any(|p| relevant(p))) {
            let _ = tx.send(());
        }
    })?;
    watcher.watch(path, mode)?;
    std::thread::Builder::new()
        .name("dq-folder-watch".into())
        .spawn(move || {
            while rx.recv().is_ok() {
                while rx.recv_timeout(DEBOUNCE).is_ok() {}
                on_change();
            }
        })
        .map_err(notify::Error::io)?;
    Ok(watcher)
}

/// (Re)creates the watchers for the folders in Settings.
pub async fn rewatch(app: &AppHandle) {
    let Ok(settings) = app.state::<AppState>().settings().await else { return };
    let fs = app.state::<FolderSync>();

    let sync_watcher = non_empty(&settings.sync_folder).and_then(|dir| {
        let handle = app.clone();
        // The sync file, a cloud client's conflicted copies and iCloud placeholders
        // (`.DataQuery.connections.dqsync.icloud`); not our hidden temp files.
        let relevant = |p: &Path| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with(sync::FILE_STEM)
                    || (n.starts_with('.') && n.contains(sync::FILE_STEM) && n.ends_with("icloud"))
            })
        };
        let on_change = move || {
            let app = handle.clone();
            tauri::async_runtime::spawn(async move { run_sync(&app).await });
        };
        watch(Path::new(&dir), RecursiveMode::NonRecursive, relevant, on_change)
            .inspect_err(|e| tracing::warn!("cannot watch the sync folder: {e}"))
            .ok()
    });
    *fs.sync_watcher.lock().expect("watcher lock") = sync_watcher;

    let scripts_watcher = non_empty(&settings.scripts_folder).and_then(|dir| {
        let root = PathBuf::from(&dir);
        let canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
        let handle = app.clone();
        // Anything but dotfiles and dot-folders (`.git`, our temp files).
        let relevant = move |p: &Path| {
            let rel = p.strip_prefix(&canonical).or_else(|_| p.strip_prefix(&root));
            rel.is_ok_and(|rel| !rel.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')))
        };
        let on_change = move || scripts_changed(&handle);
        watch(Path::new(&dir), RecursiveMode::Recursive, relevant, on_change)
            .inspect_err(|e| tracing::warn!("cannot watch the scripts folder: {e}"))
            .ok()
    });
    *fs.scripts_watcher.lock().expect("watcher lock") = scripts_watcher;
}

/// Syncs connections with the sync folder, if one is configured.
pub async fn run_sync(app: &AppHandle) -> SyncStatus {
    let fs = app.state::<FolderSync>();
    let state = app.state::<AppState>();
    let _running = fs.sync_lock.lock().await;
    let settings = match state.settings().await {
        Ok(s) => s,
        Err(e) => return SyncStatus { error: Some(e.message), ..fs.sync_status() },
    };
    let Some(folder) = non_empty(&settings.sync_folder) else {
        let status = SyncStatus::default();
        fs.set_sync_status(app, status.clone());
        return status;
    };
    let previous = fs.sync_status();
    fs.set_sync_status(app, SyncStatus { folder: Some(folder.clone()), running: true, ..previous.clone() });

    let secrets = state.secrets.clone();
    let store = state.store.clone();
    let key = fs.key.lock().expect("sync key lock").take();
    let dir = PathBuf::from(&folder);
    let result = blocking(move || {
        let Some(passphrase) = secrets.get(sync::PASSPHRASE_KEY)? else {
            return Ok((None, key));
        };
        let mut key = key;
        let outcome =
            sync::sync_with_folder(&store, &*secrets, &dir, &passphrase, &mut key, KdfParams::default(), now_ms());
        Ok((Some(outcome), key))
    })
    .await;

    let mut status = SyncStatus { folder: Some(folder), has_passphrase: true, ..SyncStatus::default() };
    match result {
        Ok((outcome, key)) => {
            *fs.key.lock().expect("sync key lock") = key;
            match outcome {
                None => {
                    status.has_passphrase = false;
                    status.error = Some("Enter the sync passphrase for this device.".into());
                }
                Some(Ok(outcome)) => {
                    status.last_synced_at = Some(now_ms());
                    status.changed = outcome.changed;
                    status.connections = outcome.connections as u32;
                    status.warnings = outcome.warnings;
                }
                Some(Err(e)) => status.error = Some(e.message),
            }
        }
        Err(e) => status.error = Some(e.message),
    }
    if status.last_synced_at.is_none() {
        status.last_synced_at = previous.last_synced_at;
    }
    if let Some(error) = &status.error {
        tracing::warn!("connection sync failed: {error}");
    }
    fs.set_sync_status(app, status.clone());
    status
}

/// Reconciles saved scripts with the scripts folder, if one is configured.
pub async fn run_scripts(app: &AppHandle) -> ScriptsFolderStatus {
    let fs = app.state::<FolderSync>();
    let state = app.state::<AppState>();
    let _running = fs.scripts_lock.lock().await;
    let folder = match state.settings().await {
        Ok(s) => non_empty(&s.scripts_folder),
        Err(e) => return ScriptsFolderStatus { error: Some(e.message), ..fs.scripts_status() },
    };
    let Some(folder) = folder else {
        let status = ScriptsFolderStatus::default();
        fs.set_scripts_status(app, status.clone());
        return status;
    };
    let dir = PathBuf::from(&folder);
    let result: DbResult<ScriptsFolderReport> =
        state.with_store(move |s| script_files::reconcile(s, &dir, now_ms())).await;
    let status = match result {
        Ok(report) => ScriptsFolderStatus { folder: Some(folder), report: Some(report), error: None },
        Err(e) => {
            tracing::warn!("scripts folder sync failed: {}", e.message);
            ScriptsFolderStatus { folder: Some(folder), report: None, error: Some(e.message) }
        }
    };
    fs.set_scripts_status(app, status.clone());
    status
}

/// Schedules a reconcile after scripts changed in the app or on disk;
/// changes that arrive while one is queued share it.
pub fn scripts_changed(app: &AppHandle) {
    let fs = app.state::<FolderSync>();
    if fs.scripts_pending.swap(true, Ordering::AcqRel) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        app.state::<FolderSync>().scripts_pending.store(false, Ordering::Release);
        run_scripts(&app).await;
    });
}
