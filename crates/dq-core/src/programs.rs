//! Finding external command-line programs (cloud CLIs, dump tools).
//!
//! Apps started from the Finder or a desktop launcher often get a minimal
//! `PATH`, so the usual install directories are searched too.

use std::path::{Path, PathBuf};

/// `PATH`, then the common install directories, then `extra`, without duplicates.
pub fn search_path(extra: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/snap/bin"].map(PathBuf::from));
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/bin"));
    }
    dirs.extend(extra);
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| !d.as_os_str().is_empty() && seen.insert(d.clone()));
    dirs
}

/// Subdirectories `<parent>/<prefix>*/<suffix>`, e.g. one `bin` per installed
/// version (`/usr/lib/postgresql/*/bin`). Missing parents give nothing.
pub fn versioned_dirs(parent: impl AsRef<Path>, prefix: &str, suffix: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent.as_ref()) else { return Vec::new() };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
        .map(|e| e.path().join(suffix))
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// File names a program `name` may have on this platform.
fn file_names(name: &str) -> Vec<String> {
    if cfg!(windows) {
        [".exe", ".cmd", ""].iter().map(|ext| format!("{name}{ext}")).collect()
    } else {
        vec![name.to_owned()]
    }
}

/// Every `name` program in `dirs`, in search order.
pub fn find_all(dirs: &[PathBuf], name: &str) -> Vec<PathBuf> {
    let names = file_names(name);
    dirs.iter().flat_map(|dir| names.iter().map(move |n| dir.join(n))).filter(|p| p.is_file()).collect()
}

/// The first `name` program in `dirs`.
pub fn find(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    find_all(dirs, name).into_iter().next()
}

/// A command for `program` that never opens a console window (Windows GUI apps
/// otherwise flash one per child process) and is killed if dropped.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    command.kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_programs_and_versioned_dirs() {
        let root = tempfile::tempdir().unwrap();
        for v in ["15", "17", "other"] {
            std::fs::create_dir_all(root.path().join(v).join("bin")).unwrap();
        }
        std::fs::create_dir_all(root.path().join("16")).unwrap();
        let dirs = versioned_dirs(root.path(), "1", "bin");
        assert_eq!(dirs, vec![root.path().join("15/bin"), root.path().join("17/bin")], "only matching dirs with a bin");
        let tool = if cfg!(windows) { "pg_dump.exe" } else { "pg_dump" };
        std::fs::write(root.path().join("17/bin").join(tool), "").unwrap();
        assert_eq!(find(&dirs, "pg_dump"), Some(root.path().join("17/bin").join(tool)));
        assert_eq!(find(&dirs, "psql"), None);
        assert_eq!(versioned_dirs(root.path().join("missing"), "", "bin"), Vec::<PathBuf>::new());
        let path = search_path([root.path().to_path_buf(), root.path().to_path_buf()]);
        assert_eq!(path.iter().filter(|d| *d == root.path()).count(), 1, "no duplicates");
    }
}
