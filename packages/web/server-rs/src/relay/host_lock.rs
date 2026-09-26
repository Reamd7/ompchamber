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

use std::path::PathBuf;

use serde_json::Value;

/// `kill(pid, 0)` probe outcome (JS `isPidAlive`): EPERM counts as alive —
/// only ESRCH means the claim is stale.
pub type PidProbe = std::sync::Arc<dyn Fn(u32) -> bool + Send + Sync>;

/// Real probe: `/bin/kill -0 <pid>` (same seam as the engine module's signal
/// helper — no libc dependency). Exit 0 → alive; stderr "Operation not
/// permitted" (EPERM) → alive; anything else (ESRCH) → dead.
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

pub fn system_pid() -> u32 {
    std::process::id()
}

pub struct RelayHostLock {
    lock_file_path: PathBuf,
    self_pid: u32,
    is_pid_alive: PidProbe,
}

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

impl RelayHostLock {
    pub fn new(lock_file_path: PathBuf, self_pid: u32, is_pid_alive: PidProbe) -> Self {
        Self {
            lock_file_path,
            self_pid,
            is_pid_alive,
        }
    }

    /// The production constructor.
    pub fn for_data_dir(data_dir: &std::path::Path) -> Self {
        Self::new(
            data_dir.join("relay-host.lock"),
            system_pid(),
            std::sync::Arc::new(kill_zero_probe),
        )
    }

    fn read_claim(&self) -> Option<u32> {
        let raw = std::fs::read_to_string(&self.lock_file_path).ok()?;
        let parsed: Value = serde_json::from_str(&raw).ok()?;
        let pid = parsed.get("pid").and_then(Value::as_u64)?;
        if pid == 0 || pid > u32::MAX as u64 {
            return None;
        }
        Some(pid as u32)
    }

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
    pub fn try_claim(&self) -> bool {
        match self.live_claimant_pid() {
            Some(holder) if holder != self.self_pid => false,
            _ => self.write_claim(),
        }
    }

    /// Unconditional claim — explicit user intent (pairing) overrides any
    /// holder.
    pub fn force_claim(&self) -> bool {
        self.write_claim()
    }

    /// True while this process is the live claimant.
    pub fn holds_claim(&self) -> bool {
        self.live_claimant_pid() == Some(self.self_pid)
    }

    /// Release only our own claim; never delete another process's.
    pub fn release(&self) {
        if self.read_claim() != Some(self.self_pid) {
            return;
        }
        let _ = std::fs::remove_file(&self.lock_file_path);
    }
}
