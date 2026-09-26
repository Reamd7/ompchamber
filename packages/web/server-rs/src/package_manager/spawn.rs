//! Command-execution seam for the package-manager port. The JS module calls
//! `spawnSync` (with `encoding: 'utf8'`, piped stdio, and a timeout); the Rust
//! port routes every probe through [`CommandRunner`] so tests can inject the
//! vitest `vi.mock('node:child_process')` fake. Production uses
//! [`TokioCommandRunner`] (kill on timeout, `windowsHide` parity).

use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

/// `spawnSync` result fields the JS module reads: `status` + `stdout`.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// Exit status; a signal-terminated child (JS `status: null`) is reported
    /// as a non-zero code so every `status !== 0` check behaves the same.
    pub status: i32,
    pub stdout: String,
}

pub type RunFuture = Pin<Box<dyn Future<Output = Option<CommandOutput>> + Send>>;

pub trait CommandRunner: Send + Sync + 'static {
    /// Runs `command args...` to completion within `timeout`. `None` mirrors
    /// the JS catch path (spawn failure or timeout).
    fn run(&self, command: &str, args: &[String], timeout: Duration) -> RunFuture;
}

/// Production runner over `tokio::process` (`spawnSync` parity: stdin
/// ignored, stdout/stderr piped, utf8 decoding, hard timeout with kill).
pub struct TokioCommandRunner;

impl CommandRunner for TokioCommandRunner {
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

#[cfg(test)]
pub mod testing {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Fake mirroring the vitest `vi.mock('node:child_process')` stub:
    /// records every invocation and answers synchronously from the handler.
    /// `None` from the handler emulates a spawn failure/timeout.
    pub struct FakeRunner {
        handler: Arc<dyn Fn(&str, &[String]) -> Option<CommandOutput> + Send + Sync>,
        calls: Mutex<Vec<(String, Vec<String>, Duration)>>,
    }

    impl FakeRunner {
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
        pub fn always_ok(stdout: &str) -> Arc<Self> {
            let stdout = stdout.to_string();
            Self::new(move |_, _| {
                Some(CommandOutput {
                    status: 0,
                    stdout: stdout.clone(),
                })
            })
        }

        pub fn calls(&self) -> Vec<(String, Vec<String>, Duration)> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }

        pub fn call_count(&self) -> usize {
            self.calls.lock().unwrap_or_else(|e| e.into_inner()).len()
        }
    }

    impl CommandRunner for FakeRunner {
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
