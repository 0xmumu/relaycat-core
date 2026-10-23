//! Helpers for writing files that must never be observed half-written and
//! must not be readable by other local users.
//!
//! Pairing sessions (private key + token), the launcher config and the recent
//! list all used `fs::write`, which truncates in place: a crash mid-write left
//! an unparseable file, and on Unix the default mode (0644) let other users
//! read the pairing private key.

use std::fs::{self, OpenOptions};
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
        #[cfg(windows)]
        restrict_to_current_user(&temp)?;
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
            use std::fs::File;
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

#[cfg(windows)]
fn restrict_to_current_user(path: &Path) -> Result<()> {
    use std::{mem, os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GENERIC_ALL, GetLastError, LocalFree},
        Security::Authorization::{
            EXPLICIT_ACCESS_W, NO_MULTIPLE_TRUSTEE, SET_ACCESS, SE_FILE_OBJECT,
            SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
        },
        Security::{
            DACL_SECURITY_INFORMATION, GetTokenInformation, NO_INHERITANCE,
            PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    unsafe {
        let mut token = ptr::null_mut();
        let mut acl = ptr::null_mut();
        let result = (|| -> Result<()> {
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                anyhow::bail!("OpenProcessToken failed with Windows error {}", GetLastError());
            }

            let mut required = 0;
            let _ = GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut required);
            if required == 0 {
                anyhow::bail!("GetTokenInformation size query failed with Windows error {}", GetLastError());
            }
            let word_count = (required as usize).div_ceil(mem::size_of::<u64>());
            let mut token_information = vec![0_u64; word_count];
            if GetTokenInformation(
                token,
                TokenUser,
                token_information.as_mut_ptr().cast(),
                required,
                &mut required,
            ) == 0
            {
                anyhow::bail!("GetTokenInformation failed with Windows error {}", GetLastError());
            }
            let token_user = &*token_information.as_ptr().cast::<TOKEN_USER>();
            let mut access: EXPLICIT_ACCESS_W = mem::zeroed();
            access.grfAccessPermissions = GENERIC_ALL;
            access.grfAccessMode = SET_ACCESS;
            access.grfInheritance = NO_INHERITANCE;
            access.Trustee = TRUSTEE_W {
                pMultipleTrustee: ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: token_user.User.Sid.cast(),
            };

            let status = SetEntriesInAclW(1, &access, ptr::null(), &mut acl);
            if status != 0 {
                anyhow::bail!("SetEntriesInAclW failed with Windows error {status}");
            }

            let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let status = SetNamedSecurityInfoW(
                path_wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                acl,
                ptr::null_mut(),
            );
            if status != 0 {
                anyhow::bail!("SetNamedSecurityInfoW failed with Windows error {status}");
            }
            Ok(())
        })();
        if !acl.is_null() {
            let _ = LocalFree(acl.cast());
        }
        if !token.is_null() {
            let _ = CloseHandle(token);
        }
        result
    }
}

/// Creates `path` as a fresh owner-only file, replacing whatever was there.
///
/// For artifacts whose contents are produced by a path-based writer
/// (`image::save`, `fs::copy`): the writer opens the destination with
/// truncate-and-create, which on Unix keeps the mode of the *existing* file.
/// Pre-creating with 0600 therefore means the artifact never exists with the
/// default umask, closing the window where a pairing QR PNG (it embeds the
/// pairing token) was briefly group/world readable.
///
/// Any pre-existing entry is unlinked first: that removes a planted symlink
/// (the link itself, not its target) and lets fixed names like `latest.png`
/// be rewritten on every run.
pub fn create_private_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(anyhow::Error::new(error))
                .with_context(|| format!("failed to replace {}", path.display()));
        }
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    Ok(())
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

    #[test]
    fn create_private_file_makes_owner_only_files_and_replaces_them() {
        let dir = temp_dir("create-private");
        let path = dir.join("latest.png");
        create_private_file(&path).unwrap();
        fs::write(&path, b"png-bytes").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"png-bytes");
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "a path-based writer truncating the file must keep the 0600 mode"
        );

        // A second run over the same fixed name must still succeed.
        create_private_file(&path).unwrap();
        fs::write(&path, b"newer").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"newer");
        let _ = fs::remove_dir_all(&dir);
    }
}
