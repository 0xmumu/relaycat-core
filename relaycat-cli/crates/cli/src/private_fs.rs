//! Helpers for writing files that must never be observed half-written and
//! must not be readable by other local users.
//!
//! Pairing sessions (private key + token), the launcher config and the recent
//! list all used `fs::write`, which truncates in place: a crash mid-write left
//! an unparseable file, and on Unix the default mode (0644) let other users
//! read the pairing private key.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Writes `bytes` to `path` atomically and with owner-only permissions.
///
/// The data goes to a sibling temp file (created with `create_new` so a
/// planted symlink is never followed), is flushed and fsynced, then renamed
/// over the destination. Readers therefore see either the old file or the new
/// one, never a truncated mix.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let file_name = path
        .file_name()
        .with_context(|| format!("{} has no file name", path.display()))?
        .to_string_lossy()
        .into_owned();
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".{file_name}.tmp-{}-{sequence}",
        std::process::id()
    ));

    let write_result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp)
            .with_context(|| format!("failed to create {}", temp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("failed to write {}", temp.display()))?;
        file.flush()
            .with_context(|| format!("failed to flush {}", temp.display()))?;
        // Durability: without the fsync the rename can survive a crash while
        // the data blocks do not, leaving a zero-length file in place.
        file.sync_all()
            .with_context(|| format!("failed to sync {}", temp.display()))?;
        drop(file);

        // Windows refuses to rename over an existing file; removing first
        // loses atomicity there, but is still better than truncating in place
        // (the file is only ever fully written before this point).
        #[cfg(windows)]
        {
            if path.exists() {
                let _ = fs::remove_file(path);
            }
        }
        fs::rename(&temp, path).with_context(|| {
            format!(
                "failed to move {} into place at {}",
                temp.display(),
                path.display()
            )
        })?;

        // Best effort: make the rename itself durable on Unix.
        #[cfg(unix)]
        {
            if let Ok(directory) = File::open(parent) {
                let _ = directory.sync_all();
            }
        }
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write_result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "relaycat-private-fs-{label}-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn writes_owner_only_files() {
        let dir = temp_dir("mode");
        let path = dir.join("session.json");
        write_private_atomic(&path, b"{\"secret\":true}").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"{\"secret\":true}");
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn replaces_an_existing_file_without_leaving_temp_files() {
        let dir = temp_dir("replace");
        let path = dir.join("session.json");
        write_private_atomic(&path, b"first").unwrap();
        write_private_atomic(&path, b"second").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"second");
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp files must be renamed away");
        let _ = fs::remove_dir_all(&dir);
    }
}
