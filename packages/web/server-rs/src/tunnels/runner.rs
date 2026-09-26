//! Child-process seam for the tunnel providers. The JS spawns cloudflared /
//! ngrok with `child_process`; this trait is the injectable boundary the JS
//! tests fake and the Rust unit tests use to assert argv shapes.

use std::pin::Pin;
use std::sync::Arc;

use tokio::io::AsyncRead;
use tokio::sync::oneshot;

use super::executable_search::EnvMap;

/// `spawnSync(target, args, { env })` outcome. `None` mirrors a thrown spawn.
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// A running tunnel child process: piped output, a kill handle, and an exit
/// signal (exit code; `None` = terminated by signal / unknown).
pub struct Spawned {
    pub kill: Arc<dyn Fn() + Send + Sync>,
    pub stdout: Option<Pin<Box<dyn AsyncRead + Send + Unpin>>>,
    pub stderr: Option<Pin<Box<dyn AsyncRead + Send + Unpin>>>,
    pub exit: oneshot::Receiver<Option<i32>>,
}

pub trait CommandRunner: Send + Sync {
    /// `resolveExecutableLaunchTarget(command)`: real PATH lookup by
    /// default; fakes pin the command so probes run without a real binary.
    fn resolve(&self, command: &str) -> Option<crate::tunnels::executable_search::LaunchTarget> {
        super::executable_search::resolve_executable_launch_target(
            command,
            &super::executable_search::real_env(),
            super::types::Platform::current(),
            &super::executable_search::real_home(),
            &super::executable_search::RealFs,
        )
    }

    /// JS `spawnSync` (blocking `--version` / `config check` probes).
    fn probe(&self, command: &str, args: &[&str], env: &EnvMap) -> Option<ProbeOutcome>;
    /// JS `spawn` with `stdio: ['ignore', 'pipe', 'pipe']` and a full env map.
    fn spawn(&self, command: &str, args: &[String], env: &EnvMap) -> std::io::Result<Spawned>;
}

/// Sends SIGINT (`killSignal: 'SIGINT'` in the JS spawns). `kill -INT` on
/// Unix, `taskkill` on Windows; a no-op if the pid already left.
pub fn kill_sigint(pid: u32) {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-INT", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

pub struct RealRunner;

impl CommandRunner for RealRunner {
    fn probe(&self, command: &str, args: &[&str], env: &EnvMap) -> Option<ProbeOutcome> {
        let mut process = std::process::Command::new(command);
        process
            .args(args)
            .env_clear()
            .envs(env)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = process.output().ok()?;
        Some(ProbeOutcome {
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn spawn(&self, command: &str, args: &[String], env: &EnvMap) -> std::io::Result<Spawned> {
        let mut process = tokio::process::Command::new(command);
        process
            .args(args)
            .env_clear()
            .envs(env)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = process.spawn()?;

        let _pid = child.id();
        let stdout = child.stdout.take().map(|stream| Box::pin(stream) as _);
        let stderr = child.stderr.take().map(|stream| Box::pin(stream) as _);

        let live_pid = Arc::new(std::sync::atomic::AtomicU64::new(_pid.unwrap_or(0) as u64));
        let kill_pid = live_pid.clone();
        let kill = Arc::new(move || {
            let pid = kill_pid.load(std::sync::atomic::Ordering::SeqCst);
            if pid != 0 {
                kill_sigint(pid as u32);
            }
        });

        let (exit_tx, exit_rx) = oneshot::channel();
        tokio::spawn(async move {
            let code = child.wait().await.ok().and_then(|status| status.code());
            live_pid.store(0, std::sync::atomic::Ordering::SeqCst);
            let _ = exit_tx.send(code);
        });

        Ok(Spawned {
            kill,
            stdout,
            stderr,
            exit: exit_rx,
        })
    }
}

// ---------------------------------------------------------------------------
// Output pumping (JS `child.stdout.on('data', ...)` / `child.stderr.on(...)`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone)]
pub struct ChildChunk {
    pub stream: ChildStream,
    pub text: String,
}

/// Spawns reader tasks that forward decoded child output chunks to a channel,
/// mirroring the JS per-chunk `data` events.
pub fn spawn_output_readers(spawned: &mut Spawned) -> tokio::sync::mpsc::Receiver<ChildChunk> {
    use tokio::io::AsyncReadExt;

    let (tx, rx) = tokio::sync::mpsc::channel::<ChildChunk>(64);
    if let Some(stdout) = spawned.stdout.take() {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut stdout = stdout;
            let mut buffer = [0u8; 4096];
            loop {
                match stdout.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&buffer[..n]).into_owned();
                        if tx
                            .send(ChildChunk {
                                stream: ChildStream::Stdout,
                                text,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
    }
    if let Some(stderr) = spawned.stderr.take() {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut stderr = stderr;
            let mut buffer = [0u8; 4096];
            loop {
                match stderr.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&buffer[..n]).into_owned();
                        if tx
                            .send(ChildChunk {
                                stream: ChildStream::Stderr,
                                text,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
    }
    rx
}

/// Test fake: records probes/spawns, feeds scripted output chunks.
#[cfg(test)]
pub mod testutil {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    type SpawnRecord = (String, Vec<String>, EnvMap);

    pub struct FakeRunner {
        probe_result: Option<ProbeOutcome>,
        probe_for_config_check: Option<ProbeOutcome>,
        spawns: Mutex<Vec<SpawnRecord>>,
        stdout_script: Vec<String>,
        stderr_script: Vec<String>,
        killed: Arc<AtomicBool>,
    }

    impl FakeRunner {
        pub fn new() -> Self {
            Self {
                probe_result: Some(ProbeOutcome {
                    status: Some(0),
                    stdout: "fake 1.0.0".to_string(),
                    stderr: String::new(),
                }),
                probe_for_config_check: None,
                spawns: Mutex::new(Vec::new()),
                stdout_script: Vec::new(),
                stderr_script: Vec::new(),
                killed: Arc::new(AtomicBool::new(false)),
            }
        }

        /// Probes fail (binary missing).
        pub fn unavailable() -> Self {
            Self {
                probe_result: None,
                ..Self::new()
            }
        }

        pub fn with_probe_stdout(mut self, stdout: impl Into<String>) -> Self {
            self.probe_result = Some(ProbeOutcome {
                status: Some(0),
                stdout: stdout.into(),
                stderr: String::new(),
            });
            self
        }

        /// What `config check` probes return (ngrok authtoken path); unset
        /// means the probe spawn itself failed.
        pub fn with_config_check(mut self, outcome: ProbeOutcome) -> Self {
            self.probe_for_config_check = Some(outcome);
            self
        }

        pub fn with_stdout_chunk(mut self, chunk: impl Into<String>) -> Self {
            self.stdout_script.push(chunk.into());
            self
        }

        pub fn with_stderr_chunk(mut self, chunk: impl Into<String>) -> Self {
            self.stderr_script.push(chunk.into());
            self
        }

        pub fn recorded_spawns(&self) -> Vec<SpawnRecord> {
            self.spawns
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }

        pub fn kill_called(&self) -> bool {
            self.killed.load(Ordering::SeqCst)
        }
    }

    impl CommandRunner for FakeRunner {
        fn resolve(
            &self,
            command: &str,
        ) -> Option<crate::tunnels::executable_search::LaunchTarget> {
            Some(crate::tunnels::executable_search::LaunchTarget {
                command: command.to_string(),
                env: EnvMap::new(),
            })
        }

        fn probe(&self, command: &str, args: &[&str], _env: &EnvMap) -> Option<ProbeOutcome> {
            if args.first() == Some(&"config") {
                return self.probe_for_config_check.clone();
            }
            let _ = command;
            self.probe_result.clone()
        }

        fn spawn(&self, command: &str, args: &[String], env: &EnvMap) -> std::io::Result<Spawned> {
            self.spawns.lock().unwrap_or_else(|e| e.into_inner()).push((
                command.to_string(),
                args.to_vec(),
                env.clone(),
            ));

            let (stdout_tx, stdout_rx) = tokio::io::duplex(64 * 1024);
            let (stderr_tx, stderr_rx) = tokio::io::duplex(64 * 1024);
            use tokio::io::AsyncWriteExt;

            let stdout_script = self.stdout_script.clone();
            if !stdout_script.is_empty() {
                tokio::spawn(async move {
                    let mut writer = stdout_tx;
                    for chunk in stdout_script {
                        let _ = writer.write_all(chunk.as_bytes()).await;
                        let _ = writer.flush().await;
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                    // Keep the stream open: the fake child stays alive.
                });
            }
            let stderr_script = self.stderr_script.clone();
            if !stderr_script.is_empty() {
                tokio::spawn(async move {
                    let mut writer = stderr_tx;
                    for chunk in stderr_script {
                        let _ = writer.write_all(chunk.as_bytes()).await;
                        let _ = writer.flush().await;
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                });
            }

            let killed = self.killed.clone();
            let kill: Arc<dyn Fn() + Send + Sync> =
                Arc::new(move || killed.store(true, Ordering::SeqCst));

            let (_exit_tx, exit_rx) = oneshot::channel();
            // Keep the exit sender alive for the fake child's lifetime;
            // dropping it would resolve `exit` immediately.
            std::mem::forget(_exit_tx);
            Ok(Spawned {
                kill,
                stdout: Some(Box::pin(stdout_rx)),
                stderr: Some(Box::pin(stderr_rx)),
                exit: exit_rx,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_runner_probe_reports_missing_binary() {
        let runner = RealRunner;
        let outcome = runner.probe(
            "definitely-not-a-real-binary-xyz",
            &["--version"],
            &EnvMap::new(),
        );
        assert!(outcome.is_none());
    }
}
