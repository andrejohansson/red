//! One atomic, owner-only file write, shared by every store that persists to the
//! user's config directory.
//!
//! Every one of these files is written by replacing it whole: a temp sibling,
//! flushed, then renamed over the target, so a crash or a full disk leaves either
//! the old file or the new one and never half of the new one. Rename is the atomic
//! step, and it is only atomic within a filesystem, which is why the temp file is
//! a sibling rather than something under `/tmp`.
//!
//! The temp name is what this module exists for. Every caller used to build it as
//! `<name>.tmp.<pid>`, which is unique *per process* - and several of these stores
//! persist from a spawned thread per change, so two writes in flight in one
//! process shared a name, interleaved their bytes, and left a file that failed to
//! parse on the next launch. Here the name carries a per-write counter as well.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

/// Distinguishes concurrent writes within one process. The pid alone does not:
/// these stores persist from a spawned thread, so N threads can be writing the
/// same store at once.
static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Write `contents` to `dest`, atomically and owner-only, waiting for the bytes to
/// reach the disk.
///
/// Creates `dest`'s parent directory if it is missing. On Unix the file is created
/// `0600` before any bytes are written, rather than chmod'ed afterwards: these
/// files hold query text, saved predicates and connection settings, and a
/// world-readable window between create and chmod is a window.
pub fn write_private(dest: &Path, contents: &[u8]) -> Result<()> {
    write_inner(dest, contents, true)
}

/// [`write_private`] without the `fsync`.
///
/// For stores written from the UI thread on an ordinary interaction (opening a
/// key, toggling a star) where a durability wait per click is a visible stall. The
/// rename is still atomic, so a reader never sees a torn file; what is given up is
/// only the guarantee that the *last* write survives a power cut, which for a
/// throwaway convenience list with a fail-open loader costs nothing.
pub fn write_private_fast(dest: &Path, contents: &[u8]) -> Result<()> {
    write_inner(dest, contents, false)
}

fn write_inner(dest: &Path, contents: &[u8], sync: bool) -> Result<()> {
    use std::io::Write;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).context("creating the config directory")?;
    }
    let tmp = temp_sibling(dest);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let write = (|| -> std::io::Result<()> {
        let mut f = opts.open(&tmp)?;
        f.write_all(contents)?;
        if sync {
            f.sync_all()?;
        }
        Ok(())
    })();
    if let Err(e) = write {
        // A temp file left behind would never be cleaned up by anything else: the
        // name is unique per write, so nothing later reuses or overwrites it.
        let _ = std::fs::remove_file(&tmp);
        return Err(e).context(format!("writing {}", tmp.display()));
    }
    std::fs::rename(&tmp, dest).context(format!("replacing {}", dest.display()))
}

/// A sibling path for `dest`'s temp file, unique across processes *and* across
/// concurrent writes within one process.
fn temp_sibling(dest: &Path) -> std::path::PathBuf {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = format!(".{name}.tmp.{}.{seq}", std::process::id());
    dest.with_file_name(tmp)
}

#[cfg(test)]
mod tests {
    use super::{temp_sibling, write_private};

    #[test]
    fn a_write_replaces_the_file_whole() {
        let dir = std::env::temp_dir().join(format!("red-atomic-{}", std::process::id()));
        let dest = dir.join("nested").join("store.json");
        write_private(&dest, b"first").unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "first");
        // Replacing an existing file leaves no remnant of the old contents.
        write_private(&dest, b"2").unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "2");
        // And no temp files survive.
        let leftovers: Vec<_> = std::fs::read_dir(dest.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "left {leftovers:?} behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bug this module exists for: one temp name per *process* meant two
    /// writer threads shared it and interleaved their bytes.
    #[test]
    fn concurrent_writes_do_not_share_a_temp_name() {
        let dest = std::env::temp_dir().join("red-atomic-seq.json");
        let a = temp_sibling(&dest);
        let b = temp_sibling(&dest);
        assert_ne!(a, b);
        assert_eq!(a.parent(), dest.parent(), "the temp file is a sibling");
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dest =
            std::env::temp_dir().join(format!("red-atomic-mode-{}.json", std::process::id()));
        write_private(&dest, b"{}").unwrap();
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_file(&dest);
    }
}
