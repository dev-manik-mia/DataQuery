//! File helpers shared by the sync file and the scripts folder.

use std::io::Write;
use std::path::Path;

use dq_core::{DbError, DbResult, ErrorKind};

pub(crate) fn io_error(what: &str, path: &Path, e: &std::io::Error) -> DbError {
    DbError::new(ErrorKind::Storage, format!("Cannot {what} {}: {e}", path.display()))
}

/// Writes `bytes` to `path` atomically: a hidden temporary file in the same
/// folder is written, flushed to disk and renamed over the target, so readers
/// (and cloud sync clients) see either the old or the new file, never half.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> DbResult<()> {
    let dir = path.parent().ok_or_else(|| DbError::invalid("The file has no parent folder."))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(io_error("write", path, &e));
    }
    Ok(())
}

/// 64-bit FNV-1a as 16 hex digits: a stable fingerprint for change detection
/// (not for security), identical across runs, platforms and Rust versions.
pub(crate) fn fingerprint(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3));
    format!("{hash:016x}")
}
