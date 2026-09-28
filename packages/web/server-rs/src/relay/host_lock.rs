//! Port of `server/lib/relay/host-lock.js` — per-machine relay-host claim.
//!
//! Every OMPChamber instance on a machine shares the same data dir and
//! therefore the same relay signing key / serverId, so if two processes run a
//! relay host at once they fight over the single host slot at the relay worker
//! (each new connection closes the previous one with "4001: Control replaced")
//! and paired devices land on whichever instance won last. The claim file
//! (`relay-host.lock` in the shared data dir) makes the contest deterministic:
//!
//! - an instance only starts its relay host when there is no LIVE claimant (a
//!   dead claimant's stale file is ignored);
//! - explicit user intent (creating a pairing link) claims unconditionally;
//! - a running host that discovers another live process has claimed backs off
//!   instead of reconnecting, which is what ends the replace/reconnect fight.
//!
//! This is a cooperative claim, not an OS lock: correctness does not depend on
//! atomicity (the relay worker still enforces a single host); the claim only
//! decides which process KEEPS retrying and which stands down.
//!
//! 中文概述：基于 claim 文件（`relay-host.lock`）的协作式主机声明。
//! 同机多实例共享数据目录与 serverId，通过「读 claim → 探活持有者
//! pid → 决定接管/退让」避免互相顶掉 relay 连接。它不是 OS 级锁，
//! 最终仲裁仍由 relay worker 的单主机规则保证，claim 只决定哪个进程
//! 继续重连、哪个让位。

use std::path::PathBuf;

use serde_json::Value;

/// `kill(pid, 0)` probe outcome (JS `isPidAlive`): EPERM counts as alive —
/// only ESRCH means the claim is stale.
///
/// 中文补充：把探活做成注入点，测试可用假探针模拟「持有者存活/已死」。
pub type PidProbe = std::sync::Arc<dyn Fn(u32) -> bool + Send + Sync>;

/// Real probe: `/bin/kill -0 <pid>` (same seam as the engine module's signal
/// helper — no libc dependency). Exit 0 → alive; stderr "Operation not
/// permitted" (EPERM) → alive; anything else (ESRCH) → dead.
///
/// 中文补充：spawn 外部 `/bin/kill` 而非绑定 libc；pid 为 0 一律视为
/// 不存在，避免误探当前进程组。
pub fn kill_zero_probe(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let output = std::process::Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .output();
    match output {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
            stderr.contains("operation not permitted") || stderr.contains("not permitted")
        }
        Err(_) => false,
    }
}

/// 当前进程 pid（写入 claim 文件的持有者标识）。
pub fn system_pid() -> u32 {
    std::process::id()
}

/// relay 主机声明的读写器：一份 claim 文件 + 自身 pid + 可注入探针。
pub struct RelayHostLock {
    /// claim 文件路径（数据目录下的 relay-host.lock）。
    lock_file_path: PathBuf,
    /// 本进程 pid，写 claim 时使用。
    self_pid: u32,
    /// pid 探活函数（生产为 kill -0 探针，测试可替换）。
    is_pid_alive: PidProbe,
}

/// 生成 UTC ISO-8601 时间戳（毫秒精度、`Z` 结尾），写入 claim 的
/// `claimedAt` 字段。纯整数换算实现，不依赖 chrono。
fn iso_timestamp_now() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let seconds = millis.div_euclid(1000);
    let millis_part = millis.rem_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let secs_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis_part:03}Z",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

/// Civil-from-days algorithm (Howard Hinnant) for ISO date rendering.
///
/// 中文补充：输入为自 1970-01-01 起的天数，仅服务上方时间戳渲染，
/// 覆盖公元 1 年起的任意有效 SystemTime。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 声明的读取、写入与持有判定操作。
impl RelayHostLock {
    /// 全参构造器：测试用它注入自定义路径、pid 与探针。
    pub fn new(lock_file_path: PathBuf, self_pid: u32, is_pid_alive: PidProbe) -> Self {
        Self {
            lock_file_path,
            self_pid,
            is_pid_alive,
        }
    }

    /// The production constructor.
    ///
    /// 中文补充：claim 文件固定为数据目录下的 relay-host.lock，
    /// pid 与探针取生产实现。
    pub fn for_data_dir(data_dir: &std::path::Path) -> Self {
        Self::new(
            data_dir.join("relay-host.lock"),
            system_pid(),
            std::sync::Arc::new(kill_zero_probe),
        )
    }

    /// 读取 claim 文件中的 pid。文件缺失、JSON 损坏、pid 为 0 或超出
    /// u32 范围都视为无有效声明（返回 None）。
    fn read_claim(&self) -> Option<u32> {
        let raw = std::fs::read_to_string(&self.lock_file_path).ok()?;
        let parsed: Value = serde_json::from_str(&raw).ok()?;
        let pid = parsed.get("pid").and_then(Value::as_u64)?;
        if pid == 0 || pid > u32::MAX as u64 {
            return None;
        }
        Some(pid as u32)
    }

    /// 以当前 pid 覆写 claim 文件（含 claimedAt 时间戳）。写失败只打
    /// warn 并仍返回 true——claim 是优化而非正确性前提，此时退化为
    /// 无锁行为，由 relay worker 仲裁。
    fn write_claim(&self) -> bool {
        let claim = serde_json::json!({
            "pid": self.self_pid,
            "claimedAt": iso_timestamp_now(),
        });
        match std::fs::write(
            &self.lock_file_path,
            serde_json::to_string(&claim).unwrap_or_default(),
        ) {
            Ok(()) => true,
            // An unwritable data dir must not take the relay down with it —
            // fall back to pre-lock behavior (start the host, let the relay
            // worker arbitrate).
            Err(error) => {
                tracing::warn!("[Relay] could not write host claim file: {error}");
                true
            }
        }
    }

    /// The pid of the current live claimant, or `None` when the claim is
    /// free/stale.
    ///
    /// 中文补充：声明存在且持有进程仍存活才算 live；陈旧声明视同空闲，
    /// 可被直接接管。
    pub fn live_claimant_pid(&self) -> Option<u32> {
        let claim = self.read_claim()?;
        if (self.is_pid_alive)(claim) {
            Some(claim)
        } else {
            None
        }
    }

    /// Claim unless another LIVE process already holds it. Re-claiming our own
    /// is a no-op refresh.
    ///
    /// 中文补充：别的活进程持有则放弃（返回 false）；自己持有或无人
    /// 持有时刷新覆写。
    pub fn try_claim(&self) -> bool {
        match self.live_claimant_pid() {
            Some(holder) if holder != self.self_pid => false,
            _ => self.write_claim(),
        }
    }

    /// Unconditional claim — explicit user intent (pairing) overrides any
    /// holder.
    ///
    /// 中文补充：不检查现有持有者，直接覆写 claim 文件。
    pub fn force_claim(&self) -> bool {
        self.write_claim()
    }

    /// True while this process is the live claimant.
    ///
    /// 中文补充：运行中的主机周期性调用它检测是否已被抢占。
    pub fn holds_claim(&self) -> bool {
        self.live_claimant_pid() == Some(self.self_pid)
    }

    /// Release only our own claim; never delete another process's.
    ///
    /// 中文补充：只在 claim 仍指向自己时删除文件；删除失败静默忽略
    /// （陈旧声明会被探活判定自然淘汰）。
    pub fn release(&self) {
        if self.read_claim() != Some(self.self_pid) {
            return;
        }
        let _ = std::fs::remove_file(&self.lock_file_path);
    }
}
