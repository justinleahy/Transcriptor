//! Owner-only file permissions for local data (Unix). No-ops elsewhere.
//!
//! Recordings, transcripts, and settings are private to the user; the app data
//! is not encrypted at rest, so at least keep other local accounts out.

use std::path::Path;

/// Restrict a directory to the owner (0700). Logs and ignores failures.
pub fn harden_dir(path: &Path) {
    set_mode(path, 0o700);
}

/// Restrict a file to the owner (0600). Logs and ignores failures.
pub fn harden_file(path: &Path) {
    set_mode(path, 0o600);
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
        log::warn!("could not set mode {mode:o} on {}: {e}", path.display());
    }
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn sets_owner_only_modes() {
        let dir = std::env::temp_dir().join(format!("transcriptor-perms-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, b"x").unwrap();
        harden_dir(&dir);
        harden_file(&file);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&file), 0o600);
        let _ = std::fs::remove_dir_all(dir);
    }
}
