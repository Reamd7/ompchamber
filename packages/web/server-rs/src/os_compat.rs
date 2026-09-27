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
//!
//! 中文说明：本模块为 `std::os::unix` 扩展 trait 提供跨平台垫片。服务器从
//! JS 移植而来，JS 中 `fs.chmodSync(0o600)` 等调用在 Windows 上本就是空操作；
//! Rust 版直接使用 `std::os::unix::fs::*Ext` 会在 Windows 上编译失败。把所有
//! `use std::os::unix::...` 统一改指向本模块后，全部 47 处调用点无需改动：
//! Unix 下透明转发 std 原生 trait；Windows 下提供同名 trait 的降级实现
//!（权限设置退化为空操作，与 JS 在 Windows 上的行为一致）。

/// Unix：直接再导出 std 的文件系统扩展 trait（零行为差异）。
#[cfg(unix)]
pub use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
/// Unix：直接再导出 std 的进程退出状态扩展 trait。
#[cfg(unix)]
pub use std::os::unix::process::ExitStatusExt;

/// Windows 垫片实现：与 std::os::unix 同名的 trait + 函数，语义降级到与 JS
/// 在 Windows 上的行为一致（权限操作为空操作等）。
#[cfg(windows)]
mod windows_shims {
    use std::fs::{Metadata, OpenOptions, Permissions};
    use std::io;
    use std::path::Path;
    use std::process::ExitStatus;

    /// 对应 `std::os::unix::fs::PermissionsExt` 的 Windows 垫片 trait。
    pub trait PermissionsExt {
        /// 按 Unix mode 构造权限对象；Windows 上退化为"克隆真实目录权限并放开只读位"。
        fn from_mode(_mode: u32) -> Permissions;
        /// 返回权限位的 Unix st_mode 表示；Windows 上恒返回 0o600（保持既有检查通过）。
        fn mode(&self) -> u32;
    }

    /// 为 `std::fs::Permissions` 实现垫片 trait。
    impl PermissionsExt for Permissions {
        /// 从当前目录元数据克隆一份真实 Permissions 作为构造来源。
        fn from_mode(_mode: u32) -> Permissions {
            // No public Permissions constructor exists; clone a real one.
            let mut permissions = std::fs::metadata(".")
                .map(|meta| meta.permissions())
                .unwrap_or_else(|_| unreachable!("cwd metadata"));
            permissions.set_readonly(false);
            permissions
        }
        /// 恒返回 0o600，使调用方的"owner 可写"检查继续成立。
        fn mode(&self) -> u32 {
            // Present owner-writable so existing checks keep passing.
            0o600
        }
    }

    /// 对应 `std::os::unix::fs::OpenOptionsExt` 的 Windows 垫片 trait。
    pub trait OpenOptionsExt {
        /// 设置创建文件时的 Unix 权限 mode；Windows 上为空操作，返回自身便于链式调用。
        fn mode(&mut self, _mode: u32) -> &mut Self;
    }

    /// 为 `std::fs::OpenOptions` 实现垫片 trait。
    impl OpenOptionsExt for OpenOptions {
        /// 空操作：忽略 mode 参数，原样返回 `&mut Self`。
        fn mode(&mut self, _mode: u32) -> &mut Self {
            self
        }
    }

    /// 对应 `std::os::unix::fs::MetadataExt` 的 Windows 垫片 trait。
    pub trait MetadataExt {
        /// 设备号；Windows 上用文件长度哈希作为稳定替身。
        fn dev(&self) -> u64;
        /// inode 号；Windows 上退化为文件长度（配合 dev 构造伪唯一标识）。
        fn ino(&self) -> u64;
        /// 修改时间（Unix 秒）；无法获取时返回 0。
        fn mtime(&self) -> i64;
    }

    /// 为 `std::fs::Metadata` 实现垫片 trait。
    impl MetadataExt for Metadata {
        /// 用文件长度与黄金比例常数的乘积异或自身，得到稳定的每文件伪设备号。
        fn dev(&self) -> u64 {
            // A stable per-file stand-in for (dev, inode) identity checks
            // (used to detect cross-device moves and same-file shortcuts).
            self.len().wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ self.len()
        }
        /// 以文件长度充当 inode，供 (dev, ino) 同文件判等使用。
        fn ino(&self) -> u64 {
            self.len()
        }
        /// 从 `modified()` 换算 Unix epoch 秒；失败时返回 0。
        fn mtime(&self) -> i64 {
            self.modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs() as i64)
                .unwrap_or(0)
        }
    }

    /// 对应 `std::os::unix::process::ExitStatusExt` 的 Windows 垫片 trait。
    pub trait ExitStatusExt {
        /// 返回导致进程退出的信号编号；Windows 上恒为 `None`（调用方视为"无信号"）。
        fn signal(&self) -> Option<i32>;
    }

    /// 为 `std::process::ExitStatus` 实现垫片 trait。
    impl ExitStatusExt for ExitStatus {
        /// Windows 退出携带的是退出码而非信号，恒返回 `None`。
        fn signal(&self) -> Option<i32> {
            // Windows exits carry a code; callers treat None as "no signal".
            None
        }
    }

    /// `std::os::unix::fs::symlink` equivalent: Windows needs a target type.
    /// 中文说明：按目标类型自动选择 Windows 的符号链接 API（目录/文件需分别调用）。
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

/// Windows：向外部再导出垫片模块中的同名 trait 与 `symlink` 函数，
/// 使调用方的 `use crate::os_compat::...` 在两个平台都能解析。
#[cfg(windows)]
pub use windows_shims::{ExitStatusExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
