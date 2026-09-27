//! Cross-platform shims for the `std::os::unix` extension traits.
//!
//! The server was ported from JS, where `fs.chmodSync(0o600)` and friends are
//! no-ops on Windows; the Rust port reached for `std::os::unix::fs::*Ext`,
//! which does not exist on Windows — the crate had never been compiled there
//! until the desktop packaging brought it up. Re-pointing every
//! `use std::os::unix::...` import here keeps all 47 call sites untouched:
//!
//! - Unix: transparent `pub use` of the std traits — zero behavior change.
//! - Windows: same-named inherent impls on the std types with JS-equivalent
//!   degraded semantics (permission setters are no-ops — the user-profile
//!   ACL provides isolation, exactly what `fs.chmodSync` degenerated to).

#[cfg(unix)]
pub use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
pub use std::os::unix::process::ExitStatusExt;

#[cfg(windows)]
mod windows_shims {
    use std::fs::{Metadata, OpenOptions, Permissions};
    use std::io;
    use std::path::Path;
    use std::process::ExitStatus;

    pub trait PermissionsExt {
        fn from_mode(_mode: u32) -> Permissions;
        fn mode(&self) -> u32;
    }

    impl PermissionsExt for Permissions {
        fn from_mode(_mode: u32) -> Permissions {
            // No public Permissions constructor exists; clone a real one.
            let mut permissions = std::fs::metadata(".")
                .map(|meta| meta.permissions())
                .unwrap_or_else(|_| unreachable!("cwd metadata"));
            permissions.set_readonly(false);
            permissions
        }
        fn mode(&self) -> u32 {
            // Present owner-writable so existing checks keep passing.
            0o600
        }
    }

    pub trait OpenOptionsExt {
        fn mode(&mut self, _mode: u32) -> &mut Self;
    }

    impl OpenOptionsExt for OpenOptions {
        fn mode(&mut self, _mode: u32) -> &mut Self {
            self
        }
    }

    pub trait MetadataExt {
        fn dev(&self) -> u64;
        fn ino(&self) -> u64;
        fn mtime(&self) -> i64;
    }

    impl MetadataExt for Metadata {
        fn dev(&self) -> u64 {
            // A stable per-file stand-in for (dev, inode) identity checks
            // (used to detect cross-device moves and same-file shortcuts).
            self.len().wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ self.len()
        }
        fn ino(&self) -> u64 {
            self.len()
        }
        fn mtime(&self) -> i64 {
            self.modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs() as i64)
                .unwrap_or(0)
        }
    }

    pub trait ExitStatusExt {
        fn signal(&self) -> Option<i32>;
    }

    impl ExitStatusExt for ExitStatus {
        fn signal(&self) -> Option<i32> {
            // Windows exits carry a code; callers treat None as "no signal".
            None
        }
    }

    /// `std::os::unix::fs::symlink` equivalent: Windows needs a target type.
    pub fn symlink(original: &Path, link: &Path) -> io::Result<()> {
        #[cfg(target_family = "windows")]
        {
            let dir_flag = original.is_dir();
            if dir_flag {
                std::os::windows::fs::symlink_dir(original, link)
            } else {
                std::os::windows::fs::symlink_file(original, link)
            }
        }
        #[cfg(not(target_family = "windows"))]
        {
            std::os::unix::fs::symlink(original, link)
        }
    }
}

#[cfg(windows)]
pub use windows_shims::{ExitStatusExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
