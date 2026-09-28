//! Command-execution seam for the package-manager port. The JS module calls
//! `spawnSync` (with `encoding: 'utf8'`, piped stdio, and a timeout); the Rust
//! port routes every probe through [`CommandRunner`] so tests can inject the
//! vitest `vi.mock('node:child_process')` fake. Production uses
//! [`TokioCommandRunner`] (kill on timeout, `windowsHide` parity).
//!
//! 中文说明：所有探测命令统一走 [`CommandRunner`] trait，测试注入
//! [`testing::FakeRunner`]（对应 vitest 的 `vi.mock('node:child_process')`），
//! 生产用 [`TokioCommandRunner`]：超时即 kill、对齐 `windowsHide`。

use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

/// `spawnSync` result fields the JS module reads: `status` + `stdout`.
#[derive(Debug, Clone)]
/// 供包管理器探测逻辑消费的最小结果面。
pub struct CommandOutput {
    /// Exit status; a signal-terminated child (JS `status: null`) is reported
    /// as a non-zero code so every `status !== 0` check behaves the same.
    /// 信号终止时记为 -1，使所有「status 非 0 即失败」判断保持一致。
    pub status: i32,
    /// utf8 解码，非法字节有损替换。
    pub stdout: String,
}

/// trait 对象化的异步返回；`None` 表示 spawn 失败或超时。
pub type RunFuture = Pin<Box<dyn Future<Output = Option<CommandOutput>> + Send>>;

/// 子进程执行接缝：生产为 tokio 实现，测试注入同步应答的 fake。
pub trait CommandRunner: Send + Sync + 'static {
    /// Runs `command args...` to completion within `timeout`. `None` mirrors
    /// the JS catch path (spawn failure or timeout).
    /// 超时用 `tokio::time::timeout` 实现，到点后由 `kill_on_drop` 收尾。
    fn run(&self, command: &str, args: &[String], timeout: Duration) -> RunFuture;
}

/// Production runner over `tokio::process` (`spawnSync` parity: stdin
/// ignored, stdout/stderr piped, utf8 decoding, hard timeout with kill).
/// 零字段单元结构，实现细节全部在 `run` 内。
pub struct TokioCommandRunner;

/// tokio 版执行实现。
impl CommandRunner for TokioCommandRunner {
    /// spawn 失败、超时或等待失败均返回 `None`；正常结束返回退出码与 stdout。
    fn run(&self, command: &str, args: &[String], timeout: Duration) -> RunFuture {
        let command = command.to_string();
        let args = args.to_vec();
        Box::pin(async move {
            let mut builder = tokio::process::Command::new(&command);
            builder
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            #[cfg(windows)]
            {
                // JS `windowsHide: true` → CREATE_NO_WINDOW.
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                builder.creation_flags(CREATE_NO_WINDOW);
            }
            let child = builder.spawn().ok()?;
            let output = tokio::time::timeout(timeout, child.wait_with_output())
                .await
                .ok()?
                .ok()?;
            Some(CommandOutput {
                status: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            })
        })
    }
}

/// 测试专用的假 runner 与断言辅助（仅 `cfg(test)` 下编译）。
#[cfg(test)]
pub mod testing {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Fake mirroring the vitest `vi.mock('node:child_process')` stub:
    /// records every invocation and answers synchronously from the handler.
    /// `None` from the handler emulates a spawn failure/timeout.
    /// 与 vitest stub 的「记录 + 即时应答」语义一一对应。
    pub struct FakeRunner {
        /// 按命令与参数即时计算应答；返回 `None` 模拟失败/超时。
        handler: Arc<dyn Fn(&str, &[String]) -> Option<CommandOutput> + Send + Sync>,
        /// 按序记录的 (command, args, timeout) 三元组。
        calls: Mutex<Vec<(String, Vec<String>, Duration)>>,
    }

    /// 构造与断言辅助。
    impl FakeRunner {
        /// 以自定义 handler 构造（返回 `Arc` 便于注入 runtime）。
        pub fn new<F>(handler: F) -> Arc<Self>
        where
            F: Fn(&str, &[String]) -> Option<CommandOutput> + Send + Sync + 'static,
        {
            Arc::new(Self {
                handler: Arc::new(handler),
                calls: Mutex::new(Vec::new()),
            })
        }

        /// The vitest default mock: every spawn succeeds with a stdout path.
        /// 每次调用都成功，stdout 固定为给定字符串。
        pub fn always_ok(stdout: &str) -> Arc<Self> {
            let stdout = stdout.to_string();
            Self::new(move |_, _| {
                Some(CommandOutput {
                    status: 0,
                    stdout: stdout.clone(),
                })
            })
        }

        /// 返回已记录调用，用于断言探测顺序与参数。
        pub fn calls(&self) -> Vec<(String, Vec<String>, Duration)> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }

        /// 已记录的调用次数。
        pub fn call_count(&self) -> usize {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).len()
        }
    }

    /// fake 的接缝实现。
    impl CommandRunner for FakeRunner {
        /// 记录调用后同步执行 handler，并把结果包装成 future。
        fn run(&self, command: &str, args: &[String], timeout: Duration) -> RunFuture {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).push((
                command.to_string(),
                args.to_vec(),
                timeout,
            ));
            let result = (self.handler)(command, args);
            Box::pin(async move { result })
        }
    }
}
