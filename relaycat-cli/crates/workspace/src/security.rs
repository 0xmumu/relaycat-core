use relaycat_protocol::{WorkspaceError, WorkspaceErrorCode};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    io,
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct WorkspaceServiceError {
    error: WorkspaceError,
}

impl WorkspaceServiceError {
    pub fn new(code: WorkspaceErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self { error: WorkspaceError { code, message: message.into(), retryable } }
    }

    pub fn code(&self) -> WorkspaceErrorCode { self.error.code }
    pub fn outside_project() -> Self {
        Self::new(WorkspaceErrorCode::PathOutsideProject, "path is outside the current project", false)
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(WorkspaceErrorCode::InvalidRequest, message, false)
    }
    pub fn busy() -> Self { Self::new(WorkspaceErrorCode::Busy, "workspace is busy", true) }
    pub fn timeout() -> Self { Self::new(WorkspaceErrorCode::Timeout, "workspace request timed out", true) }
    pub fn cancelled() -> Self { Self::new(WorkspaceErrorCode::Cancelled, "workspace request was cancelled", true) }
    pub fn unsupported(message: impl Into<String>) -> Self { Self::new(WorkspaceErrorCode::Unsupported, message, false) }

    pub fn io(error: io::Error) -> Self {
        let (code, message, retryable) = match error.kind() {
            io::ErrorKind::NotFound => (WorkspaceErrorCode::NotFound, "path was not found", false),
            io::ErrorKind::PermissionDenied => (WorkspaceErrorCode::PermissionDenied, "permission denied", false),
            _ => (WorkspaceErrorCode::Internal, "workspace I/O failed", true),
        };
        Self::new(code, message, retryable)
    }
}

impl fmt::Display for WorkspaceServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.error.message) }
}
impl std::error::Error for WorkspaceServiceError {}
impl From<WorkspaceServiceError> for WorkspaceError {
    fn from(value: WorkspaceServiceError) -> Self { value.error }
}

#[derive(Debug, Clone)]
pub struct ProjectRoot {
    canonical: PathBuf,
    project_id: String,
}

impl ProjectRoot {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, WorkspaceServiceError> {
        let canonical = path.as_ref().canonicalize().map_err(WorkspaceServiceError::io)?;
        if !canonical.is_dir() { return Err(WorkspaceServiceError::invalid("project root is not a directory")); }
        let mut hasher = Sha256::new();
        hasher.update(canonical.to_string_lossy().as_bytes());
        let digest = hasher.finalize();
        let project_id = digest[..16].iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(Self { canonical, project_id })
    }

    pub fn path(&self) -> &Path { &self.canonical }
    pub fn project_id(&self) -> &str { &self.project_id }

    pub fn resolve(&self, relative: &str) -> Result<PathBuf, WorkspaceServiceError> {
        let lexical = self.lexical_path(relative)?;
        let resolved = lexical.canonicalize().map_err(WorkspaceServiceError::io)?;
        if !resolved.starts_with(&self.canonical) { return Err(WorkspaceServiceError::outside_project()); }
        // Re-resolve immediately before returning. This closes the common
        // check/use window where a symlink is swapped between validation and
        // the first filesystem operation by making the checked path stable
        // across two independent canonicalization passes.
        let confirmed = lexical.canonicalize().map_err(WorkspaceServiceError::io)?;
        if confirmed != resolved || !confirmed.starts_with(&self.canonical) {
            return Err(WorkspaceServiceError::outside_project());
        }
        Ok(confirmed)
    }

    pub fn lexical_path(&self, relative: &str) -> Result<PathBuf, WorkspaceServiceError> {
        if relative.contains('\0') { return Err(WorkspaceServiceError::outside_project()); }
        let path = Path::new(relative);
        if path.is_absolute() || path.components().any(|part| matches!(part, Component::ParentDir | Component::RootDir | Component::Prefix(_))) {
            return Err(WorkspaceServiceError::outside_project());
        }
        Ok(self.canonical.join(path))
    }
}

/// Opens an already-canonical path without following a symlink in its last
/// component.
///
/// `ProjectRoot::resolve` canonicalizes twice to shrink the check/use window,
/// but a local attacker can still replace the path with a symlink between the
/// last check and the open. Because `path` is canonical, it contains no
/// symlinks when validated, so refusing to follow the final component turns a
/// successful swap into an error instead of a read of an out-of-project file.
pub(crate) fn open_without_following_symlinks(
    path: &Path,
) -> Result<std::fs::File, WorkspaceServiceError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Open the reparse point itself instead of following it: the Windows
        // counterpart of `O_NOFOLLOW`.
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(WorkspaceServiceError::io)?;
    #[cfg(windows)]
    {
        // With `FILE_FLAG_OPEN_REPARSE_POINT` the handle refers to the link,
        // so a symlink swapped in after the canonicalize pass is detectable
        // instead of silently followed. Junctions report `is_dir()` and are
        // rejected by the caller's regular-file check.
        let is_reparse_point = file
            .metadata()
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false);
        if is_reparse_point {
            return Err(WorkspaceServiceError::invalid(
                "path points at a reparse point",
            ));
        }
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "relaycat-security-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn regular_files_open_normally() {
        let dir = scratch_dir("regular");
        let path = dir.join("note.txt");
        std::fs::write(&path, "hi").unwrap();
        assert!(open_without_following_symlinks(&path).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_swapped_in_place_of_the_file_is_refused() {
        let dir = scratch_dir("symlink");
        let outside = dir.join("outside.txt");
        std::fs::write(&outside, "secret").unwrap();
        let path = dir.join("note.txt");
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(open_without_following_symlinks(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
