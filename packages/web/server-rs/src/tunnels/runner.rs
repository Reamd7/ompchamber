//! Child-process seam for the tunnel providers. The JS spawns cloudflared /
//! ngrok with `child_process`; this trait is the injectable boundary the JS
//! tests fake and the Rust unit tests use to assert argv shapes.
//! （中文说明）子进程执行接缝：tunnel provider 通过 `CommandRunner`
//! trait 完成 --version/config check 探测并拉起 cloudflared/ngrok，
//! 对应 JS 里直接调用 `child_process` 的位置。生产实现走真实进程，
//! 单测用 FakeRunner 脚本化输出并断言 argv 形状。

use std::pin::Pin;
use std::sync::Arc;

use tokio::io::AsyncRead;
use tokio::sync::oneshot;

use super::executable_search::EnvMap;

/// `spawnSync(target, args, { env })` outcome. `None` mirrors a thrown spawn.
///
/// 同步探测（JS `spawnSync`）的结果封装；spawn 本身失败（如二进制
/// 不存在）以 None 表达，对应 JS 里抛异常的分支。
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    /// 子进程退出码；被信号终止等拿不到码的情况为 None。
    pub status: Option<i32>,
    /// 标准输出全文（UTF-8 有损解码，供版本号/URL 提取）。
    pub stdout: String,
    /// 标准错误全文（供错误摘要与诊断）。
    pub stderr: String,
}

/// A running tunnel child process: piped output, a kill handle, and an exit
/// signal (exit code; `None` = terminated by signal / unknown).
///
/// 由 `spawn` 返回的"活着的子进程"句柄：输出流供读取器泵送，kill
/// 闭包负责终止，exit 一次性通道在进程结束后送出退出码。
pub struct Spawned {
    /// 终止子进程的回调：实现应发 SIGINT（JS 的 killSignal 配置）；可重复调用。
    pub kill: Arc<dyn Fn() + Send + Sync>,
    /// 子进程 stdout（piped）；会被 `spawn_output_readers` 取走消费。
    pub stdout: Option<Pin<Box<dyn AsyncRead + Send + Unpin>>>,
    /// 子进程 stderr（piped）；同上。
    pub stderr: Option<Pin<Box<dyn AsyncRead + Send + Unpin>>>,
    /// 退出码接收端；进程被信号终止或状态未知时收到 None。
    pub exit: oneshot::Receiver<Option<i32>>,
}

/// 子进程执行接缝：provider 的全部进程操作（解析路径、探测、拉起）
/// 都经由此 trait，使 argv 组装与就绪轮询逻辑可在无真实二进制的
/// 测试中验证。
pub trait CommandRunner: Send + Sync {
    /// `resolveExecutableLaunchTarget(command)`: real PATH lookup by
    /// default; fakes pin the command so probes run without a real binary.
    ///
    /// 把命令名解析为可执行路径与配套环境；默认实现走真实 PATH
    /// 查找，FakeRunner 直接回传命令名以跳过磁盘查找。
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
    ///
    /// 阻塞式同步探测：等待进程结束并收集全部输出；命令无法启动
    /// （依赖缺失）时返回 None，由调用方转为"未安装"结论。
    fn probe(&self, command: &str, args: &[&str], env: &EnvMap) -> Option<ProbeOutcome>;
    /// JS `spawn` with `stdio: ['ignore', 'pipe', 'pipe']` and a full env map.
    ///
    /// 异步拉起子进程：stdin 关闭、stdout/stderr 管道化，返回可
    /// 泵送输出、可终止、可等退出的 `Spawned` 句柄。
    fn spawn(&self, command: &str, args: &[String], env: &EnvMap) -> std::io::Result<Spawned>;
}

/// Sends SIGINT (`killSignal: 'SIGINT'` in the JS spawns). `kill -INT` on
/// Unix, `taskkill` on Windows; a no-op if the pid already left.
///
/// 对指定 pid 发送中断信号：Unix 用 `kill -INT`，Windows 用
/// `taskkill /F /T` 连同进程树；pid 已退出时静默忽略。
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

/// 生产实现：probe 走同步 `std::process`，spawn 走 tokio 异步进程并
/// 开启 kill_on_drop 兜底，防止句柄泄漏留下孤儿进程。
pub struct RealRunner;

/// 真实进程版的接缝实现。
impl CommandRunner for RealRunner {
    /// 同步执行并等待结束：清空继承环境只注入传入的 env，丢弃 stdin，
    /// 管道收集输出；启动失败（如 NotFound）返回 None。
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

    /// 异步拉起：记录存活 pid 供 kill 闭包发 SIGINT；后台任务等待退出
    /// 后清零 pid 并经 oneshot 送出退出码。
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

/// 输出块的来源流：stdout 或 stderr，供消费者区分日志级别与去噪。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildStream {
    /// 标准输出流。
    Stdout,
    /// 标准错误流。
    Stderr,
}

/// 一段子进程输出文本及其来源流，对应 JS `data` 事件回调的参数形态。
#[derive(Debug, Clone)]
pub struct ChildChunk {
    /// 该文本块来自哪个流。
    pub stream: ChildStream,
    /// UTF-8 有损解码后的文本块。
    pub text: String,
}

/// Spawns reader tasks that forward decoded child output chunks to a channel,
/// mirroring the JS per-chunk `data` events.
///
/// 取走 `Spawned` 的两个输出流并各起一个读取任务，把 4KB 块解码后
/// 送入容量 64 的 mpsc 通道；接收端全部丢弃导致发送失败时即结束
/// 读取。对应 JS 里对 stdout/stderr 逐块挂 `data` 监听的写法。
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
///
/// 测试替身：按脚本回放 probe 结果与输出块，记录每次 spawn 的 argv
/// 与环境，并把 kill 调用记为标志位供超时断言使用。
#[cfg(test)]
pub mod testutil {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// 一次 spawn 的记录：命令、参数列表、环境快照，供测试断言 argv。
    type SpawnRecord = (String, Vec<String>, EnvMap);

    /// 可脚本化的 `CommandRunner` 替身（builder 风格配置）。
    pub struct FakeRunner {
        /// 普通探测（--version）的固定返回；None 表示二进制缺失。
        probe_result: Option<ProbeOutcome>,
        /// `config check` 探测的固定返回；None 表示该探测本身失败。
        probe_for_config_check: Option<ProbeOutcome>,
        /// 已记录的 spawn 列表，按发生顺序保存。
        spawns: Mutex<Vec<SpawnRecord>>,
        /// 每次 spawn 后向 stdout 管道回放的文本块脚本。
        stdout_script: Vec<String>,
        /// 每次 spawn 后向 stderr 管道回放的文本块脚本。
        stderr_script: Vec<String>,
        /// kill 闭包是否被调用过（AtomicBool，跨任务共享）。
        killed: Arc<AtomicBool>,
    }

    /// builder 式配置方法与结果查询。
    impl FakeRunner {
        /// 默认探测成功（"fake 1.0.0"、退出码 0），无输出脚本、未 kill。
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
        ///
        /// 依赖缺失场景：所有普通探测返回 None。
        pub fn unavailable() -> Self {
            Self {
                probe_result: None,
                ..Self::new()
            }
        }

        /// 设置普通探测的成功返回与版本输出（status 固定为 0）。
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
        ///
        /// 设置 `config check` 探测的返回；不调用则该探测按失败处理。
        pub fn with_config_check(mut self, outcome: ProbeOutcome) -> Self {
            self.probe_for_config_check = Some(outcome);
            self
        }

        /// 追加一段 spawn 后回放到 stdout 的文本块。
        pub fn with_stdout_chunk(mut self, chunk: impl Into<String>) -> Self {
            self.stdout_script.push(chunk.into());
            self
        }

        /// 追加一段 spawn 后回放到 stderr 的文本块。
        pub fn with_stderr_chunk(mut self, chunk: impl Into<String>) -> Self {
            self.stderr_script.push(chunk.into());
            self
        }

        /// 返回至今所有 spawn 的（命令、argv、环境）记录副本。
        pub fn recorded_spawns(&self) -> Vec<SpawnRecord> {
            self.spawns
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }

        /// spawn 返回的 kill 句柄是否已被调用。
        pub fn kill_called(&self) -> bool {
            self.killed.load(Ordering::SeqCst)
        }
    }

    /// 替身实现：不触碰真实进程。
    impl CommandRunner for FakeRunner {
        /// 恒回传命令名本身与空环境，使探测无需真实二进制即可进行。
        fn resolve(
            &self,
            command: &str,
        ) -> Option<crate::tunnels::executable_search::LaunchTarget> {
            Some(crate::tunnels::executable_search::LaunchTarget {
                command: command.to_string(),
                env: EnvMap::new(),
            })
        }

        /// argv 首参为 "config" 时返回 config check 脚本，否则返回普通
        /// 探测脚本；脚本为 None 即模拟启动失败。
        fn probe(&self, command: &str, args: &[&str], _env: &EnvMap) -> Option<ProbeOutcome> {
            if args.first() == Some(&"config") {
                return self.probe_for_config_check.clone();
            }
            let _ = command;
            self.probe_result.clone()
        }

        /// 记录（命令、argv、环境），用 duplex 管道回放输出脚本且保持
        /// 流不关闭（模拟子进程存活）；kill 只置标志位，exit 通道永不完成。
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

/// 真实 runner 的冒烟验证。
#[cfg(test)]
mod tests {
    use super::*;

    /// 行为契约：二进制不存在时 probe 返回 None（依赖缺失分支）。
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
