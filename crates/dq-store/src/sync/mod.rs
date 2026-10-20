//! Connection sync through a folder the user already syncs (iCloud Drive,
//! Dropbox, a git checkout …). There is no DataQuery service: every
//! device reads and rewrites one encrypted file, [`SYNC_FILE`].
//!
//! The file holds folders and connections (name, folder, environment,
//! favorite and the full [`ConnConfig`]) — the same secret-free data as the
//! connection export (T2.19). Passwords, SSH secrets, tokens and AI keys never
//! enter it; they stay in each device's keychain under the connection id, which
//! is the same on every device.
//!
//! Merging is last-writer-wins per folder and per connection on
//! `(updated_at, device)`. A device notices its own edits by comparing each
//! item with the fingerprint it recorded at the previous sync, so every code
//! path that changes a connection is covered. Deletions travel as tombstones
//! that expire after [`TOMBSTONE_TTL_MS`]; a device offline for longer may
//! bring a deleted connection back.

mod crypto;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use dq_core::config::{ConnConfig, Environment};
use dq_core::{DbError, DbResult};
use serde::{Deserialize, Serialize};

pub use crypto::{KdfParams, SyncKey};

use crate::files::{fingerprint, io_error, write_atomic};
use crate::{ConnectionDraft, Folder, PasswordChange, SavedConnection, SecretStore, Store};

/// The file every device reads and writes.
pub const SYNC_FILE: &str = "DataQuery.connections.dqsync";
/// Keychain id of this device's sync passphrase.
pub const PASSPHRASE_KEY: &str = "sync:passphrase";
/// Deletions are remembered this long (90 days).
pub const TOMBSTONE_TTL_MS: i64 = 90 * 86_400_000;

const FORMAT: &str = "dataquery-sync";
const STATE_KEY: &str = "sync:state";
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// Start of the sync file name and of the copies cloud clients make of it.
pub const FILE_STEM: &str = "DataQuery.connections";
const FILE_EXT: &str = ".dqsync";

/// When and where an item last changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub updated_at: i64,
    pub device: String,
}

impl Stamp {
    fn key(&self) -> (i64, &str) {
        (self.updated_at, &self.device)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncedFolder {
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub stamp: Stamp,
}

/// A connection as stored in the sync file. Deliberately has no field that
/// could hold a secret (and no `has_password` flags: those are per device).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncedConnection {
    pub id: String,
    pub name: String,
    pub folder_id: Option<String>,
    pub environment: Environment,
    pub favorite: bool,
    pub config: ConnConfig,
    #[serde(flatten)]
    pub stamp: Stamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Folder,
    Connection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    pub kind: ItemKind,
    pub id: String,
    pub deleted_at: i64,
    pub device: String,
}

/// The decrypted content of the sync file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncPayload {
    pub format: String,
    pub version: u32,
    pub written_by: String,
    pub written_at: i64,
    pub folders: Vec<SyncedFolder>,
    pub connections: Vec<SyncedConnection>,
    pub tombstones: Vec<Tombstone>,
}

impl SyncPayload {
    fn content_fingerprint(&self) -> String {
        let json = serde_json::to_vec(&(&self.folders, &self.connections, &self.tombstones)).unwrap_or_default();
        fingerprint(&json)
    }

    fn sort(&mut self) {
        self.folders.sort_by(|a, b| a.id.cmp(&b.id));
        self.connections.sort_by(|a, b| a.id.cmp(&b.id));
        self.tombstones.sort_by(|a, b| (a.kind, &a.id).cmp(&(b.kind, &b.id)));
    }
}

trait Item: Clone {
    const KIND: ItemKind;
    fn id(&self) -> &str;
    fn stamp(&self) -> &Stamp;
    fn stamp_mut(&mut self) -> &mut Stamp;
    /// Fingerprint of everything but the stamp.
    fn body(&self) -> String;
}

impl Item for SyncedFolder {
    const KIND: ItemKind = ItemKind::Folder;
    fn id(&self) -> &str {
        &self.id
    }
    fn stamp(&self) -> &Stamp {
        &self.stamp
    }
    fn stamp_mut(&mut self) -> &mut Stamp {
        &mut self.stamp
    }
    fn body(&self) -> String {
        fingerprint(self.name.as_bytes())
    }
}

impl Item for SyncedConnection {
    const KIND: ItemKind = ItemKind::Connection;
    fn id(&self) -> &str {
        &self.id
    }
    fn stamp(&self) -> &Stamp {
        &self.stamp
    }
    fn stamp_mut(&mut self) -> &mut Stamp {
        &mut self.stamp
    }
    fn body(&self) -> String {
        let json = serde_json::to_vec(&(&self.name, &self.folder_id, self.environment, self.favorite, &self.config));
        fingerprint(&json.unwrap_or_default())
    }
}

/// What this device knew after its last sync, kept in the local store.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncState {
    pub device_id: String,
    /// `kind:id` → body fingerprint and stamp at the last sync.
    known: BTreeMap<String, (String, Stamp)>,
    tombstones: Vec<Tombstone>,
    pub last_synced_at: Option<i64>,
}

fn known_key(kind: ItemKind, id: &str) -> String {
    format!("{}:{id}", if kind == ItemKind::Folder { "f" } else { "c" })
}

impl SyncState {
    pub fn load(store: &Store) -> DbResult<Self> {
        let mut state: Self =
            store.get_value(STATE_KEY)?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
        if state.device_id.is_empty() {
            state.device_id = uuid::Uuid::new_v4().to_string();
            state.save(store)?;
        }
        Ok(state)
    }

    fn save(&self, store: &Store) -> DbResult<()> {
        store.set_value(STATE_KEY, &serde_json::to_value(self).map_err(|e| DbError::internal(e.to_string()))?)
    }

    /// Stamps local items: unchanged ones keep the stamp of the last sync,
    /// new or edited ones get `now` and this device. Known items that are gone
    /// locally become tombstones.
    fn local<T: Item>(&self, items: Vec<T>, tombstones: &mut Vec<Tombstone>, now: i64) -> Vec<T> {
        let present: BTreeSet<&str> = items.iter().map(Item::id).collect();
        let prefix = known_key(T::KIND, "");
        for key in self.known.keys().filter_map(|k| k.strip_prefix(&prefix)) {
            if !present.contains(key) && !tombstones.iter().any(|t| t.kind == T::KIND && t.id == key) {
                tombstones.push(Tombstone {
                    kind: T::KIND,
                    id: key.to_owned(),
                    deleted_at: now,
                    device: self.device_id.clone(),
                });
            }
        }
        items
            .into_iter()
            .map(|mut item| {
                *item.stamp_mut() = match self.known.get(&known_key(T::KIND, item.id())) {
                    Some((body, stamp)) if *body == item.body() => stamp.clone(),
                    _ => Stamp { updated_at: now, device: self.device_id.clone() },
                };
                item
            })
            .collect()
    }
}

/// Last writer wins per id; a deletion wins a tie with an edit.
fn merge<T: Item>(sides: Vec<Vec<T>>, tombstones: &[Tombstone]) -> Vec<T> {
    let mut live: BTreeMap<String, T> = BTreeMap::new();
    for item in sides.into_iter().flatten() {
        match live.get(item.id()) {
            Some(current) if current.stamp().key() >= item.stamp().key() => {}
            _ => {
                live.insert(item.id().to_owned(), item);
            }
        }
    }
    live.into_values()
        .filter(|item| {
            !tombstones.iter().any(|t| {
                t.kind == T::KIND && t.id == item.id() && (t.deleted_at, t.device.as_str()) >= item.stamp().key()
            })
        })
        .collect()
}

/// The newest tombstone per item, without expired ones and without those an
/// edit made after the deletion has overridden.
fn merge_tombstones(all: Vec<Tombstone>, now: i64) -> Vec<Tombstone> {
    let mut newest: BTreeMap<(ItemKind, String), Tombstone> = BTreeMap::new();
    for t in all.into_iter().filter(|t| t.deleted_at >= now - TOMBSTONE_TTL_MS) {
        match newest.get(&(t.kind, t.id.clone())) {
            Some(current) if (current.deleted_at, &current.device) >= (t.deleted_at, &t.device) => {}
            _ => {
                newest.insert((t.kind, t.id.clone()), t);
            }
        }
    }
    newest.into_values().collect()
}

/// Merges any number of payloads (this device's view first) into one.
fn merge_payloads(device: &str, now: i64, payloads: Vec<SyncPayload>) -> SyncPayload {
    let all_tombstones = payloads.iter().flat_map(|p| p.tombstones.iter().cloned()).collect();
    let tombstones = merge_tombstones(all_tombstones, now);
    let (mut folders, mut connections) = (Vec::new(), Vec::new());
    for p in payloads {
        folders.push(p.folders);
        connections.push(p.connections);
    }
    let folders: Vec<SyncedFolder> = merge(folders, &tombstones);
    let mut connections: Vec<SyncedConnection> = merge(connections, &tombstones);
    // Connections in a deleted folder move to "no folder", as they do locally.
    for c in &mut connections {
        if c.folder_id.as_ref().is_some_and(|id| !folders.iter().any(|f| &f.id == id)) {
            c.folder_id = None;
        }
    }
    // A tombstone that lost to a later edit is dropped so the item stays alive.
    let alive: BTreeSet<(ItemKind, &str)> = folders
        .iter()
        .map(|f| (ItemKind::Folder, f.id.as_str()))
        .chain(connections.iter().map(|c| (ItemKind::Connection, c.id.as_str())))
        .collect();
    let tombstones = tombstones.into_iter().filter(|t| !alive.contains(&(t.kind, t.id.as_str()))).collect();
    let mut merged = SyncPayload {
        format: FORMAT.into(),
        version: 1,
        written_by: device.to_owned(),
        written_at: now,
        folders,
        connections,
        tombstones,
    };
    merged.sort();
    merged
}

/// Result of one sync.
#[derive(Debug, Clone, Default)]
pub struct SyncOutcome {
    /// Local folders or connections were added, changed or deleted.
    pub changed: bool,
    /// The sync file was (re)written.
    pub wrote: bool,
    pub connections: usize,
    /// Items that could not be applied here, or conflicted copies that could not be read.
    pub warnings: Vec<String>,
}

fn synced_connection(c: SavedConnection) -> SyncedConnection {
    SyncedConnection {
        id: c.id,
        name: c.name,
        folder_id: c.folder_id,
        environment: c.environment,
        favorite: c.favorite,
        config: c.config,
        stamp: Stamp { updated_at: 0, device: String::new() },
    }
}

/// Copies the cloud client left when two devices wrote at once, e.g.
/// `DataQuery.connections (conflicted copy 2026-01-02).dqsync` or `DataQuery.connections 2.dqsync`.
fn conflicted_copies(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut copies: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n != SYNC_FILE && n.starts_with(FILE_STEM) && n.ends_with(FILE_EXT))
        })
        .collect();
    copies.sort();
    copies
}

fn read_payload(path: &Path, passphrase: &str, key: &mut Option<SyncKey>) -> DbResult<SyncPayload> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| io_error("read", path, &e))?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return Err(DbError::invalid(format!("{} is not a sync file DataQuery can read.", path.display())));
    }
    let bytes = std::fs::read(path).map_err(|e| io_error("read", path, &e))?;
    let (plain, derived) = crypto::open(passphrase, &bytes, key.take())?;
    *key = Some(derived);
    let payload: SyncPayload = serde_json::from_slice(&plain)
        .map_err(|e| DbError::invalid(format!("The sync file content is invalid: {e}")))?;
    if payload.format != FORMAT || payload.version != 1 {
        return Err(DbError::invalid("The sync file was written by a newer DataQuery; update this copy."));
    }
    Ok(payload)
}

/// The full sync path of a folder.
pub fn sync_file(dir: &Path) -> PathBuf {
    dir.join(SYNC_FILE)
}

/// Runs one sync of this device's folders and connections with the file in `dir`.
///
/// `key` caches the derived key between runs; pass the same slot every time
/// and clear it when the passphrase changes.
pub fn sync_with_folder(
    store: &Store,
    secrets: &dyn SecretStore,
    dir: &Path,
    passphrase: &str,
    key: &mut Option<SyncKey>,
    params: KdfParams,
    now: i64,
) -> DbResult<SyncOutcome> {
    if !dir.is_dir() {
        return Err(DbError::invalid(format!("The sync folder {} is not available.", dir.display())));
    }
    let main = sync_file(dir);
    if !main.exists() && dir.join(format!(".{SYNC_FILE}.icloud")).exists() {
        return Err(DbError::invalid(
            "The sync file is in iCloud Drive but not downloaded yet. Open the folder in Finder, or wait a moment.",
        ));
    }

    let mut state = SyncState::load(store)?;
    let mut outcome = SyncOutcome::default();
    let mut tombstones = state.tombstones.clone();
    let local_folders = state.local(
        store
            .folders()?
            .into_iter()
            .map(|f| SyncedFolder { id: f.id, name: f.name, stamp: Stamp { updated_at: 0, device: String::new() } })
            .collect(),
        &mut tombstones,
        now,
    );
    let saved = store.connections()?;
    let local_connections = state.local(saved.iter().cloned().map(synced_connection).collect(), &mut tombstones, now);
    let mut local = SyncPayload {
        format: FORMAT.into(),
        version: 1,
        written_by: state.device_id.clone(),
        written_at: now,
        folders: local_folders,
        connections: local_connections,
        tombstones,
    };
    local.sort();

    // Remote: the main file, plus conflicted copies a cloud client made.
    let remote = if main.exists() { Some(read_payload(&main, passphrase, key)?) } else { None };
    let mut payloads = vec![local.clone()];
    payloads.extend(remote.clone());
    let mut merged_copies = Vec::new();
    for copy in conflicted_copies(dir) {
        let name = copy.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut copy_key = None;
        match read_payload(&copy, passphrase, &mut copy_key) {
            Ok(p) => {
                payloads.push(p);
                merged_copies.push(copy);
            }
            Err(e) => outcome.warnings.push(format!("{name}: {}", e.message)),
        }
    }
    let merged = merge_payloads(&state.device_id, now, payloads);

    // Apply the merged view locally: folders first, connections may point at them.
    let local_folder_ids: BTreeMap<&str, &SyncedFolder> = local.folders.iter().map(|f| (f.id.as_str(), f)).collect();
    for f in &merged.folders {
        if local_folder_ids.get(f.id.as_str()).is_none_or(|l| l.name != f.name) {
            match store.put_folder(&Folder { id: f.id.clone(), name: f.name.clone() }) {
                Ok(()) => outcome.changed = true,
                Err(e) => outcome.warnings.push(format!("Folder {}: {}", f.name, e.message)),
            }
        }
    }
    let folder_ids: BTreeSet<String> = store.folders()?.into_iter().map(|f| f.id).collect();
    let saved_by_id: BTreeMap<&str, &SavedConnection> = saved.iter().map(|c| (c.id.as_str(), c)).collect();
    let local_by_id: BTreeMap<&str, &SyncedConnection> = local.connections.iter().map(|c| (c.id.as_str(), c)).collect();
    for c in &merged.connections {
        if local_by_id.get(c.id.as_str()).is_some_and(|l| l.body() == c.body()) {
            continue;
        }
        let existing = saved_by_id.get(c.id.as_str());
        let hops = c.config.ssh.as_ref().map_or(0, |t| t.hops().count());
        let mut ssh = existing.map(|e| e.ssh_secrets.clone()).unwrap_or_default();
        ssh.resize(hops, false);
        let draft = ConnectionDraft {
            id: Some(c.id.clone()),
            name: c.name.clone(),
            folder_id: c.folder_id.clone().filter(|f| folder_ids.contains(f)),
            environment: c.environment,
            favorite: c.favorite,
            config: c.config.clone(),
            password: PasswordChange::Keep,
            ssh_secrets: Vec::new(),
        };
        // Secrets are untouched: the keychain keeps them under the same id.
        match store.save_connection(&draft, existing.is_some_and(|e| e.has_password), &ssh) {
            Ok(_) => outcome.changed = true,
            Err(e) => outcome.warnings.push(format!("{}: {}", c.name, e.message)),
        }
    }
    for t in &merged.tombstones {
        match t.kind {
            ItemKind::Connection => {
                if let Some(existing) = saved_by_id.get(t.id.as_str()) {
                    let mut keys = vec![SavedConnection::password_key(&t.id)];
                    keys.extend((0..existing.ssh_secrets.len()).map(|hop| SavedConnection::ssh_secret_key(&t.id, hop)));
                    keys.iter().try_for_each(|k| secrets.delete(k))?;
                    store.delete_connection(&t.id)?;
                    outcome.changed = true;
                }
            }
            ItemKind::Folder => {
                if local_folder_ids.contains_key(t.id.as_str()) {
                    store.delete_folder(&t.id)?;
                    outcome.changed = true;
                }
            }
        }
    }

    // Write when the file is missing or the merge differs from it.
    let differs = remote.as_ref().is_none_or(|r| {
        let mut r = r.clone();
        r.sort();
        r.content_fingerprint() != merged.content_fingerprint()
    });
    if differs || !merged_copies.is_empty() {
        let json = serde_json::to_vec(&merged).map_err(|e| DbError::internal(e.to_string()))?;
        if key.is_none() {
            *key = Some(SyncKey::generate(passphrase, params)?);
        }
        let sealed = crypto::seal(key.as_ref().expect("key set above"), &json)?;
        write_atomic(&main, &sealed)?;
        for copy in &merged_copies {
            let _ = std::fs::remove_file(copy);
        }
        outcome.wrote = true;
    }

    // Remember what is now in sync. Items that could not be applied here are
    // not recorded, so a later sync never mistakes them for local deletions.
    let here: BTreeSet<String> = store
        .folders()?
        .into_iter()
        .map(|f| known_key(ItemKind::Folder, &f.id))
        .chain(store.connections()?.into_iter().map(|c| known_key(ItemKind::Connection, &c.id)))
        .collect();
    state.known = merged
        .folders
        .iter()
        .map(|f| (known_key(ItemKind::Folder, &f.id), (f.body(), f.stamp.clone())))
        .chain(merged.connections.iter().map(|c| (known_key(ItemKind::Connection, &c.id), (c.body(), c.stamp.clone()))))
        .filter(|(k, _)| here.contains(k))
        .collect();
    state.tombstones = merged.tombstones;
    state.last_synced_at = Some(now);
    state.save(store)?;
    outcome.connections = merged.connections.len();
    Ok(outcome)
}

#[cfg(test)]
mod tests;
