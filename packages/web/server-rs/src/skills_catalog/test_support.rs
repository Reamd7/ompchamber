//! Shared test helpers for the skills_catalog module tests: unique temp
//! dirs, env-var guards, a global serialization lock (module caches and env
//! vars are process-global, so tests that touch them must not overlap), a
//! fake git runner, and repository fixtures.
//!
//! 中文说明：skills_catalog 各测试共享的辅助：唯一临时目录、环境变量
//! 守卫、全局串行锁（模块缓存与环境变量是进程级状态，相关测试不得
//! 并发重叠）、假 git runner 与仓库夹具。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;

use crate::skills_catalog::git::{GitIdentity, GitResult, GitRunOptions, GitRunner};

/// Serializes tests that touch module-global cache state or process env
/// vars (`OMPCHAMBER_DATA_DIR`, `HOME`). Async tests use `.lock().await`;
/// sync tests use `.blocking_lock()`.
/// 防止并发测试互相踩全局状态的串行锁。
pub static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 以 prefix + 12 位随机后缀创建并返回一个唯一临时目录。
pub fn unique_temp_dir(prefix: &str) -> PathBuf {
    use rand::Rng;
    let mut rng = rand::rng();
    let suffix: String = (0..12)
        .map(|_| {
            let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
            alphabet[rng.random_range(0..alphabet.len())] as char
        })
        .collect();
    let dir = std::env::temp_dir().join(format!("{prefix}-{suffix}"));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Sets an env var for the duration of the guard and restores the previous
/// value (or removes it) on drop.
/// RAII 式环境变量守卫。
pub struct EnvGuard {
    /// 被设置的变量名。
    key: &'static str,
    /// set 之前的原值（None 表示原本不存在）。
    original: Option<String>,
}

/// EnvGuard 的构造。
impl EnvGuard {
    /// 记录原值后设置新值，drop 时恢复。
    pub fn set(key: &'static str, value: &str) -> Self {
        let original = std::env::var(key).ok();
        unsafe { std::env::set_var(key, value) };
        EnvGuard { key, original }
    }
}

/// drop 时恢复环境变量原值。
impl Drop for EnvGuard {
    /// 有原值则写回，否则移除变量。
    fn drop(&mut self) {
        match &self.original {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

/// A `GitRunner` fake: records every invocation and delegates to a test
/// closure that fabricates results.
/// 记录每次调用 argv 并交给闭包伪造结果的 GitRunner 假体。
pub struct FakeGit {
    /// 按 argv 伪造结果的测试闭包。
    handler: Box<dyn Fn(&[String]) -> GitResult + Send + Sync>,
    /// 全部调用记录（每次一份 argv 拷贝）。
    calls: Mutex<Vec<Vec<String>>>,
}

/// FakeGit 的构造与调用记录查询。
impl FakeGit {
    /// 用闭包构造共享的假 runner。
    pub fn new(handler: impl Fn(&[String]) -> GitResult + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(FakeGit {
            handler: Box::new(handler),
            calls: Mutex::new(Vec::new()),
        })
    }

    /// 返回至今所有调用的 argv 快照。
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 筛出以给定前缀开头的调用（按位置逐元素相等）。
    pub fn find_calls_starting_with(&self, prefix: &[&str]) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|call| {
                call.len() >= prefix.len()
                    && call
                        .iter()
                        .zip(prefix.iter())
                        .all(|(actual, expected)| actual == expected)
            })
            .collect()
    }
}

/// GitRunner 实现：记录后同步返回闭包结果。
impl GitRunner for FakeGit {
    /// 记录 argv 并返回伪造结果（忽略 options）。
    fn run(&self, args: &[String], _options: &GitRunOptions) -> BoxFuture<'static, GitResult> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(args.to_vec());
        let result = (self.handler)(args);
        Box::pin(async move { result })
    }
}
/// Build a fake runner that behaves like a successful clone of `fixture`
/// into the clone target, plus the scan command shape: `ls-files` reports
/// `ls_files_stdout`, `show` reads the blob from the fixture on disk, and
/// every other subcommand succeeds quietly. Inspect behavior through
/// [`FakeGit::calls`].
/// 面向扫描流程的假 runner：clone 把 fixture 拷到目标、`ls-files` 回放
/// 给定 stdout、`show` 从 fixture 磁盘读 blob、其余子命令静默成功。
pub fn scan_git_fake(fixture: Option<PathBuf>, ls_files_stdout: String) -> Arc<FakeGit> {
    FakeGit::new(move |args| {
        if args.first().map(String::as_str) == Some("--version") {
            return GitResult::success("git version 2.0.0\n", "");
        }
        if args.first().map(String::as_str) == Some("clone") {
            if let (Some(fixture), Some(target)) = (&fixture, args.last()) {
                copy_dir_recursive(fixture, Path::new(target));
            }
            return GitResult::success("", "");
        }
        if args.len() >= 3 && args[0] == "-C" {
            match args[2].as_str() {
                "ls-files" => return GitResult::success(ls_files_stdout.clone(), ""),
                "show" => {
                    let path = args.last().map(String::as_str).unwrap_or("");
                    let content = fixture
                        .as_deref()
                        .map(|dir| Path::new(dir).join(path.trim_start_matches("HEAD:")))
                        .filter(|path| path.is_file())
                        .and_then(|path| std::fs::read_to_string(path).ok())
                        .unwrap_or_default();
                    return GitResult::success(content, "");
                }
                _ => return GitResult::success("", ""),
            }
        }
        GitResult::success("", "")
    })
}

/// Recursive directory copy used by git fakes to simulate a clone/checkout.
/// 递归拷贝目录（保留符号链接本身），供假 clone/checkout 使用。
pub fn copy_dir_recursive(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create dir");
    for entry in std::fs::read_dir(src).expect("read dir") {
        let entry = entry.expect("dir entry");
        let target = dst.join(entry.file_name());
        let file_type = entry.file_type().expect("file type");
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &target);
        } else if file_type.is_symlink() {
            crate::os_compat::symlink(
                &std::fs::read_link(entry.path()).expect("link target"),
                &target,
            )
            .expect("symlink");
        } else {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

/// Write a SKILL.md with the given frontmatter and body.
/// 生成 `---\n<frontmatter>---\n<body>` 形状的 SKILL.md。
pub fn write_skill_md(dir: &Path, frontmatter: &str, body: &str) {
    std::fs::create_dir_all(dir).expect("create skill dir");
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\n{frontmatter}---\n{body}"),
    )
    .expect("write SKILL.md");
}

/// An identity with an SSH key (used to assert the SSH clone URL is chosen).
/// 带测试 SSH key 的身份，断言走 SSH clone URL 时使用。
pub fn ssh_identity() -> GitIdentity {
    GitIdentity {
        ssh_key: Some("/keys/id_test".to_string()),
    }
}
