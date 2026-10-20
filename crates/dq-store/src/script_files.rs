//! Saved scripts mirrored as plain `.sql` files in a folder the user chooses,
//! so a team can share them through git (or any synced folder).
//!
//! Layout: one file per script, `<folder>/<name>.sql`, where the script folder
//! `Reports/Monthly` becomes nested sub-folders. Names and folders are made
//! safe for every platform (see [`sanitize`]); clashes, also ones that differ
//! only in case, get ` (2)`, ` (3)` … suffixes.
//!
//! Metadata that SQL cannot hold goes in an optional first line:
//!
//! ```sql
//! -- dataquery: {"tags":["billing"],"pinned":true,"connection":"kestrel-prod"}
//! SELECT …
//! ```
//!
//! The line is written only when there is something to say, so most files are
//! the SQL alone. Keys: `tags`, `pinned`, `connection` (by name, as ids differ
//! per device), and `name` / `folder` only when the file path cannot carry
//! them exactly (characters a file name cannot hold).
//!
//! The local store stays the source of truth for the app. [`reconcile`] makes
//! the two agree: it remembers, per script, the file path and fingerprints of
//! the file and of the script at the last reconcile, so it can tell which side
//! changed. When both changed, the file wins and the app's version is kept as a
//! `… (conflict)` copy; nothing is overwritten silently. A file that moved
//! with unchanged content (`git mv`, Finder) keeps its script.
//!
//! Dotfiles and dot-folders (`.git`), non-`.sql` files, symbolic links and
//! files over 1 MB are ignored; the scan stops at [`MAX_FILES`] files.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use dq_core::{DbError, DbResult};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::files::{fingerprint, io_error, write_atomic};
use crate::{SavedScript, ScriptDraft, Store};

/// First-line prefix of the metadata header.
pub const HEADER_PREFIX: &str = "-- dataquery: ";
pub const MAX_FILES: usize = 5_000;
const MAX_FILE_BYTES: u64 = 1024 * 1024 + 4096;
const MAX_DEPTH: usize = 8;
const MAX_STEM_CHARS: usize = 100;
const INDEX_KEY: &str = "scripts_folder:index";
const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1", "lpt2",
    "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Script metadata in the optional first line of a file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
}

/// Splits a file into its header (empty when there is none) and SQL.
pub fn parse(text: &str) -> (Header, &str) {
    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    first
        .strip_prefix(HEADER_PREFIX)
        .and_then(|json| serde_json::from_str(json.trim_end_matches('\r')).ok())
        .map_or((Header::default(), text), |h| (h, rest))
}

/// The file content: the header line when it has anything, then the SQL.
pub fn render(header: &Header, sql: &str) -> String {
    if *header == Header::default() {
        return sql.to_owned();
    }
    let json = serde_json::to_string(header).unwrap_or_default();
    format!("{HEADER_PREFIX}{json}\n{sql}")
}

/// A file or folder name that is valid on Windows, macOS and Linux: no path
/// separators, reserved characters or control characters, no leading dot (so
/// never `..` or a hidden file), no trailing dot or space, not a reserved
/// Windows device name, at most 100 characters. Unicode letters are kept.
pub fn sanitize(name: &str) -> String {
    let cleaned: String =
        name.chars()
            .map(|c| {
                if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                    '_'
                } else {
                    c
                }
            })
            .collect();
    let capped: String = cleaned.trim().trim_start_matches('.').chars().take(MAX_STEM_CHARS).collect();
    let mut s = capped.trim_end_matches(['.', ' ']).to_owned();
    if s.is_empty() {
        return "untitled".into();
    }
    let base = s.split('.').next().unwrap_or_default().trim_end().to_ascii_lowercase();
    if RESERVED.contains(&base.as_str()) {
        s.insert(base.len(), '_');
    }
    s
}

fn folder_parts(folder: Option<&str>) -> Vec<String> {
    folder.unwrap_or_default().split('/').map(str::trim).filter(|p| !p.is_empty()).map(sanitize).collect()
}

/// Where a script lives in the folder, `/`-separated: `Reports/paid orders.sql`.
pub fn relative_path(name: &str, folder: Option<&str>) -> String {
    let mut parts = folder_parts(folder);
    parts.push(format!("{}.sql", sanitize(name)));
    parts.join("/")
}

/// The folder (`a/b`, or `None` at the top) and file stem of a relative path.
fn split_path(rel: &str) -> (Option<&str>, &str) {
    let (dir, file) = rel.rsplit_once('/').map_or((None, rel), |(d, f)| (Some(d), f));
    let stem = file.len().checked_sub(4).and_then(|n| file.get(..n)).unwrap_or(file);
    (dir, stem)
}

/// `rest` is empty or a ` (n)` clash suffix.
fn clash_suffix(rest: &str) -> bool {
    rest.is_empty()
        || rest
            .strip_prefix(" (")
            .and_then(|r| r.strip_suffix(')'))
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// The stem was made from `name`: its sanitized form, any case, maybe with a clash suffix.
fn stem_matches(stem: &str, name: &str) -> bool {
    let base = sanitize(name);
    stem.get(..base.len()).is_some_and(|p| p.eq_ignore_ascii_case(&base)) && clash_suffix(&stem[base.len()..])
}

/// The stem gives back `name` exactly, so the header need not repeat it.
fn stem_is_name(stem: &str, name: &str) -> bool {
    stem.strip_prefix(name).is_some_and(clash_suffix)
}

fn folder_matches(dir: Option<&str>, folder: Option<&str>) -> bool {
    folder_parts(folder).join("/").eq_ignore_ascii_case(dir.unwrap_or_default())
}

/// The folder as written in the store, `a/b`, with blank parts dropped.
fn folder_text(folder: &str) -> String {
    folder.split('/').map(str::trim).filter(|p| !p.is_empty()).collect::<Vec<_>>().join("/")
}

/// File content for `script` at `rel`.
pub fn script_file(script: &SavedScript, connection: Option<&str>, rel: &str) -> String {
    let (dir, stem) = split_path(rel);
    let header = Header {
        name: (!stem_is_name(stem, &script.name)).then(|| script.name.clone()),
        folder: script.folder.clone().filter(|f| folder_text(f) != dir.unwrap_or_default()),
        tags: script.tags.clone(),
        pinned: script.pinned,
        connection: connection.map(str::to_owned),
    };
    render(&header, &script.sql)
}

/// The script a file describes. `connection_id` resolves a connection name.
pub fn draft_from_file(rel: &str, text: &str, connection_id: impl Fn(&str) -> Option<String>) -> ScriptDraft {
    let (header, sql) = parse(text);
    let (dir, stem) = split_path(rel);
    let name = header.name.filter(|n| stem_matches(stem, n)).unwrap_or_else(|| stem.trim().to_owned());
    let folder = header.folder.filter(|f| folder_matches(dir, Some(f))).or_else(|| dir.map(str::to_owned));
    ScriptDraft {
        id: None,
        name: if name.is_empty() { "untitled".into() } else { name },
        sql: sql.to_owned(),
        connection_id: header.connection.as_deref().and_then(connection_id),
        folder,
        tags: header.tags,
        pinned: header.pinned,
    }
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct DiskFile {
    rel: String,
    abs: PathBuf,
    len: u64,
    mtime: i64,
}

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64)
}

/// `.sql` files under `root` by lower-cased relative path. Never follows
/// symbolic links, so nothing outside the folder is read.
fn scan(root: &Path, skipped: &mut Vec<String>) -> BTreeMap<String, DiskFile> {
    let mut files = BTreeMap::new();
    let mut stack = vec![(root.to_path_buf(), String::new(), 0usize)];
    while let Some((dir, prefix, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            skipped.push(format!("{}: cannot be read", if prefix.is_empty() { "." } else { &prefix }));
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
            if name.starts_with('.') {
                continue;
            }
            let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
            if meta.is_dir() {
                if depth < MAX_DEPTH {
                    stack.push((entry.path(), rel, depth + 1));
                }
            } else if meta.is_file() && Path::new(&name).extension().is_some_and(|e| e.eq_ignore_ascii_case("sql")) {
                if meta.len() > MAX_FILE_BYTES {
                    skipped.push(format!("{rel}: larger than 1 MB"));
                } else if files.len() >= MAX_FILES {
                    skipped.push(format!("{rel}: more than {MAX_FILES} files"));
                } else if let Some(other) = files.get(&rel.to_lowercase()).map(|f: &DiskFile| f.rel.clone()) {
                    skipped.push(format!("{rel}: same name as {other} apart from case"));
                } else {
                    let file =
                        DiskFile { abs: entry.path(), len: meta.len(), mtime: mtime_ms(&meta), rel: rel.clone() };
                    files.insert(rel.to_lowercase(), file);
                }
            }
        }
    }
    files
}

// ---------------------------------------------------------------------------
// Reconcile
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    path: String,
    /// Fingerprint of the file content at the last reconcile.
    file: String,
    /// Fingerprint of the script in the store at the last reconcile.
    script: String,
    len: u64,
    mtime: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Index {
    root: String,
    entries: BTreeMap<String, Entry>,
}

/// What one reconcile did.
#[derive(Debug, Clone, Default, Serialize, TS)]
#[ts(export)]
pub struct ScriptsFolderReport {
    pub folder: String,
    pub files: u32,
    /// Scripts created from new files.
    pub imported: u32,
    /// Scripts updated from changed or moved files.
    pub updated: u32,
    /// Scripts removed because their file was deleted.
    pub removed: u32,
    /// Files written for scripts changed in the app.
    pub written: u32,
    /// Files deleted for scripts deleted in the app.
    pub deleted_files: u32,
    /// Scripts whose file and app copy both changed; the app copy was kept as "… (conflict)".
    pub conflicts: Vec<String>,
    /// Files that were not read, with the reason.
    pub skipped: Vec<String>,
    /// Ids of scripts changed or removed from disk, so open tabs can react.
    pub changed_ids: Vec<String>,
    #[ts(type = "number")]
    pub at: i64,
}

fn script_fingerprint(s: &SavedScript) -> String {
    let json = serde_json::to_vec(&(&s.name, &s.folder, &s.tags, s.pinned, &s.connection_id, &s.sql));
    fingerprint(&json.unwrap_or_default())
}

struct Reconciler<'a> {
    store: &'a Store,
    root: PathBuf,
    names: HashMap<String, String>,
    disk: BTreeMap<String, DiskFile>,
    /// Lower-cased paths in use: on disk or written in this run.
    taken: HashSet<String>,
    claimed: HashSet<String>,
    entries: BTreeMap<String, Entry>,
    report: ScriptsFolderReport,
}

impl Reconciler<'_> {
    fn read(&mut self, file: &DiskFile) -> Option<String> {
        match std::fs::read(&file.abs).map(String::from_utf8) {
            Ok(Ok(text)) => Some(text),
            Ok(Err(_)) => {
                self.report.skipped.push(format!("{}: not UTF-8 text", file.rel));
                None
            }
            Err(e) => {
                self.report.skipped.push(format!("{}: {e}", file.rel));
                None
            }
        }
    }

    fn file_fingerprint(&mut self, file: &DiskFile, entry: Option<&Entry>) -> Option<String> {
        match entry {
            Some(e) if e.path == file.rel && e.len == file.len && e.mtime == file.mtime => Some(e.file.clone()),
            _ => self.read(file).map(|t| fingerprint(t.as_bytes())),
        }
    }

    /// Absolute path for `rel`, creating real (non-link) folders on the way.
    fn prepare(&self, rel: &str) -> DbResult<PathBuf> {
        let mut path = self.root.clone();
        let parts: Vec<&str> = rel.split('/').collect();
        for (i, part) in parts.iter().enumerate() {
            if part.is_empty() || part.starts_with('.') || part.contains(['\\', ':']) {
                return Err(DbError::invalid(format!("Unsafe script path {rel}.")));
            }
            path.push(part);
            if i + 1 < parts.len() {
                match std::fs::symlink_metadata(&path) {
                    Ok(m) if m.is_dir() => {}
                    Ok(_) => return Err(DbError::invalid(format!("{} is not a folder.", path.display()))),
                    Err(_) => std::fs::create_dir(&path).map_err(|e| io_error("create", &path, &e))?,
                }
            }
        }
        if std::fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file()) {
            return Err(DbError::invalid(format!("{} is not a regular file.", path.display())));
        }
        Ok(path)
    }

    fn unique(&self, desired: &str) -> String {
        let (dir, stem) = split_path(desired);
        let mut candidate = desired.to_owned();
        let mut n = 2;
        while self.taken.contains(&candidate.to_lowercase()) {
            let file = format!("{stem} ({n}).sql");
            candidate = dir.map_or(file.clone(), |d| format!("{d}/{file}"));
            n += 1;
        }
        candidate
    }

    /// Writes `script`, keeping `current` when it still fits its name and folder.
    /// A failure is reported and leaves the script unmapped until the next run.
    fn write(&mut self, script: &SavedScript, current: Option<&str>) {
        if let Err(e) = self.try_write(script, current) {
            self.report.skipped.push(format!("{}: {}", script.name, e.message));
        }
    }

    fn try_write(&mut self, script: &SavedScript, current: Option<&str>) -> DbResult<()> {
        let fits = current.filter(|rel| {
            let (dir, stem) = split_path(rel);
            stem_matches(stem, &script.name) && folder_matches(dir, script.folder.as_deref())
        });
        let rel = match fits {
            Some(rel) => rel.to_owned(),
            None => self.unique(&relative_path(&script.name, script.folder.as_deref())),
        };
        let connection = script.connection_id.as_ref().and_then(|id| self.names.get(id)).map(String::as_str);
        let text = script_file(script, connection, &rel);
        let path = self.prepare(&rel)?;
        write_atomic(&path, text.as_bytes())?;
        let meta = std::fs::metadata(&path).map_err(|e| io_error("read", &path, &e))?;
        self.taken.insert(rel.to_lowercase());
        self.claimed.insert(rel.to_lowercase());
        if let Some(old) = current.filter(|old| !old.eq_ignore_ascii_case(&rel)) {
            self.remove(old);
        }
        self.entries.insert(
            script.id.clone(),
            Entry {
                path: rel,
                file: fingerprint(text.as_bytes()),
                script: script_fingerprint(script),
                len: meta.len(),
                mtime: mtime_ms(&meta),
            },
        );
        self.report.written += 1;
        Ok(())
    }

    /// Deletes a file and the folders it leaves empty.
    fn remove(&mut self, rel: &str) {
        let path = self.root.join(rel);
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) && std::fs::remove_file(&path).is_ok() {
            self.report.deleted_files += 1;
            self.taken.remove(&rel.to_lowercase());
            let mut dir = path.parent();
            while let Some(d) = dir.filter(|d| *d != self.root && d.starts_with(&self.root)) {
                if std::fs::remove_dir(d).is_err() {
                    break;
                }
                dir = d.parent();
            }
        }
    }

    /// Creates or updates script `id` from a file.
    fn load(&mut self, id: Option<String>, file: &DiskFile, text: &str) -> DbResult<SavedScript> {
        let names = &self.names;
        let mut draft = draft_from_file(&file.rel, text, |name| {
            names.iter().find(|(_, n)| n.as_str() == name).map(|(id, _)| id.clone())
        });
        draft.id = id;
        let saved = self.store.save_script(&draft)?;
        self.claimed.insert(file.rel.to_lowercase());
        self.report.changed_ids.push(saved.id.clone());
        self.entries.insert(
            saved.id.clone(),
            Entry {
                path: file.rel.clone(),
                file: fingerprint(text.as_bytes()),
                script: script_fingerprint(&saved),
                len: file.len,
                mtime: file.mtime,
            },
        );
        Ok(saved)
    }
}

/// Makes the scripts in the store and the `.sql` files under `root` agree.
pub fn reconcile(store: &Store, root: &Path, now: i64) -> DbResult<ScriptsFolderReport> {
    let root = root.canonicalize().map_err(|e| io_error("open", root, &e))?;
    if !root.is_dir() {
        return Err(DbError::invalid(format!("{} is not a folder.", root.display())));
    }
    let root_key = root.to_string_lossy().into_owned();
    let index: Index = store.get_value(INDEX_KEY)?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    // A different folder starts over; scripts and files are matched up again.
    let previous = if index.root == root_key { index.entries } else { BTreeMap::new() };
    let mut skipped = Vec::new();
    let disk = scan(&root, &mut skipped);
    let mut r = Reconciler {
        store,
        names: store.connections()?.into_iter().map(|c| (c.id, c.name)).collect(),
        taken: disk.keys().cloned().collect(),
        disk,
        root,
        claimed: HashSet::new(),
        entries: BTreeMap::new(),
        report: ScriptsFolderReport { folder: root_key.clone(), skipped, at: now, ..Default::default() },
    };
    let scripts: BTreeMap<String, SavedScript> = store.scripts()?.into_iter().map(|s| (s.id.clone(), s)).collect();
    let indexed: HashSet<String> = previous.values().map(|e| e.path.to_lowercase()).collect();

    for (id, entry) in &previous {
        let script = scripts.get(id);
        let script_changed = script.is_some_and(|s| script_fingerprint(s) != entry.script);
        let mut file =
            r.disk.get(&entry.path.to_lowercase()).filter(|f| !r.claimed.contains(&f.rel.to_lowercase())).cloned();
        let mut fingerprint_now = None;
        if file.is_none() && script.is_some() {
            // Moved or renamed: an unindexed file with exactly the old content.
            let candidates: Vec<DiskFile> = r
                .disk
                .iter()
                .filter(|(k, _)| !indexed.contains(*k) && !r.claimed.contains(*k))
                .map(|(_, f)| f.clone())
                .collect();
            for f in candidates {
                if f.len == entry.len && r.file_fingerprint(&f, None).as_ref() == Some(&entry.file) {
                    fingerprint_now = Some(entry.file.clone());
                    file = Some(f);
                    break;
                }
            }
        }
        match (script, file) {
            (Some(s), Some(f)) => {
                let moved = !f.rel.eq_ignore_ascii_case(&entry.path);
                let Some(now_fp) = fingerprint_now.or_else(|| r.file_fingerprint(&f, Some(entry))) else {
                    r.claimed.insert(f.rel.to_lowercase());
                    r.entries.insert(id.clone(), entry.clone());
                    continue;
                };
                let file_changed = moved || now_fp != entry.file;
                r.claimed.insert(f.rel.to_lowercase());
                match (file_changed, script_changed) {
                    (false, false) => {
                        r.entries.insert(id.clone(), Entry { len: f.len, mtime: f.mtime, ..entry.clone() });
                    }
                    (false, true) => r.write(s, Some(&f.rel)),
                    (true, changed) => {
                        if changed {
                            let copy = r.store.save_script(&ScriptDraft {
                                id: None,
                                name: format!("{} (conflict)", s.name),
                                sql: s.sql.clone(),
                                connection_id: s.connection_id.clone(),
                                folder: s.folder.clone(),
                                tags: s.tags.clone(),
                                pinned: false,
                            })?;
                            r.report.conflicts.push(copy.name);
                        }
                        let Some(text) = r.read(&f) else {
                            r.entries.insert(id.clone(), entry.clone());
                            continue;
                        };
                        match r.load(Some(id.clone()), &f, &text) {
                            Ok(_) => r.report.updated += 1,
                            Err(e) => r.report.skipped.push(format!("{}: {}", f.rel, e.message)),
                        }
                    }
                }
            }
            (Some(s), None) => {
                if script_changed {
                    r.write(s, None);
                } else {
                    store.delete_script(id)?;
                    r.report.removed += 1;
                    r.report.changed_ids.push(id.clone());
                }
            }
            (None, Some(f)) => {
                // Deleted in the app: delete the file unless it changed since.
                if r.file_fingerprint(&f, Some(entry)).as_ref() == Some(&entry.file) {
                    r.claimed.insert(f.rel.to_lowercase());
                    r.remove(&f.rel);
                }
            }
            (None, None) => {}
        }
    }

    // Scripts without a file: new in the app, conflict copies, or a first run.
    for script in store.scripts()? {
        if r.entries.contains_key(&script.id) {
            continue;
        }
        let desired = relative_path(&script.name, script.folder.as_deref()).to_lowercase();
        // A first run on a shared folder adopts identical files instead of duplicating them.
        let adopt =
            r.disk.get(&desired).filter(|_| !r.claimed.contains(&desired) && !indexed.contains(&desired)).cloned();
        if let Some(f) = adopt {
            let connection = script.connection_id.as_ref().and_then(|id| r.names.get(id)).cloned();
            let expected = script_file(&script, connection.as_deref(), &f.rel);
            if r.read(&f).is_some_and(|text| text == expected) {
                r.claimed.insert(desired);
                let entry = Entry {
                    path: f.rel.clone(),
                    file: fingerprint(expected.as_bytes()),
                    script: script_fingerprint(&script),
                    len: f.len,
                    mtime: f.mtime,
                };
                r.entries.insert(script.id.clone(), entry);
                continue;
            }
        }
        r.write(&script, None);
    }

    // Files without a script: added on disk (or by a `git pull`).
    let new_files: Vec<DiskFile> =
        r.disk.iter().filter(|(k, _)| !r.claimed.contains(*k)).map(|(_, f)| f.clone()).collect();
    for f in new_files {
        let Some(text) = r.read(&f) else { continue };
        match r.load(None, &f, &text) {
            Ok(_) => r.report.imported += 1,
            Err(e) => r.report.skipped.push(format!("{}: {}", f.rel, e.message)),
        }
    }

    r.report.files = r.entries.len() as u32;
    let index = Index { root: root_key, entries: r.entries };
    store.set_value(INDEX_KEY, &serde_json::to_value(&index).map_err(|e| DbError::internal(e.to_string()))?)?;
    Ok(r.report)
}

/// Forgets the file mapping, e.g. when the scripts folder is turned off.
pub fn forget(store: &Store) -> DbResult<()> {
    store.set_value(INDEX_KEY, &serde_json::json!({}))
}

#[cfg(test)]
mod tests;
