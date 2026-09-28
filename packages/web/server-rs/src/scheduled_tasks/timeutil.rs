//! Civil-date math and IANA timezone offsets for the scheduled-tasks port.
//!
//! The JS module leans on `luxon` (`DateTime`, `IANAZone`) and `cron-parser`
//! for timezone-aware scheduling. This crate has no tz database crate, so the
//! minimal honest equivalent is implemented here:
//!
//! - Howard Hinnant's `days_from_civil` / `civil_from_days` algorithms for
//!   UTC ↔ civil (Y/M/D h:m:s) conversion.
//! - A compact TZif (RFC 8536) reader that resolves a zone's UTC offset at a
//!   given instant from `/usr/share/zoneinfo` (overridable via `TZDIR`).
//! - Offsets are resolved once per evaluation and then held fixed ("DST-free
//!   UTC math"): DST-transition day arithmetic of luxon/cron-parser is not
//!   reproduced. Gap noted in PORT-MANIFEST.md.
//!
//! civil 日期运算与 IANA 时区偏移解析（计划任务移植的自主实现）：
//! JS 版依赖 luxon/cron-parser 的 tz 数据库；本 crate 不引入此类依赖，
//! 自带 Hinnant civil 算法、精简 TZif (RFC 8536) 读取器（从
//! /usr/share/zoneinfo 或 TZDIR 解析），并采用“每次求值只解析一次、
//! 之后按固定偏移计算”的 DST-free 语义；与 luxon 在 DST 切换日的
//! 差异记录于 PORT-MANIFEST.md。
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

/// 每秒的毫秒数。
pub const MS_PER_SECOND: i64 = 1_000;
/// 每分钟的毫秒数。
pub const MS_PER_MINUTE: i64 = 60 * MS_PER_SECOND;
/// 每小时的毫秒数。
pub const MS_PER_HOUR: i64 = 60 * MS_PER_MINUTE;
/// 每天的毫秒数。
pub const MS_PER_DAY: i64 = 24 * MS_PER_HOUR;

/// Days since 1970-01-01 for a civil date (Hinnant `days_from_civil`).
/// 公历 (Y/M/D) 到 1970-01-01 的天数；era 算法对任意整数年份（含负）成立。
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m as i64 + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Civil date for days since 1970-01-01 (Hinnant `civil_from_days`).
/// days_from_civil 的逆运算：epoch 天数转为 (年, 月, 日)。
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Weekday (0 = Sunday .. 6 = Saturday) for days since the epoch.
/// 1970-01-01 was a Thursday (= 4).
/// 由 epoch 天数求星期序号：0=周日..6=周六。
pub fn weekday_from_days(days: i64) -> u32 {
    (days + 4).rem_euclid(7) as u32
}

/// 闰年判定（4/100/400 规则）。
pub fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// 某年某月的天数；月份非法时返回 0。
pub fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(y) => 29,
        2 => 28,
        _ => 0,
    }
}

/// A civil timestamp broken out of a UTC-or-fixed-offset instant.
/// 从 UTC 或固定偏移时刻拆出的民用时间字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    /// 年（可为负）。
    pub year: i64,
    /// 月 1..=12。
    pub month: u32,
    /// 日 1..=31。
    pub day: u32,
    /// 时 0..=23。
    pub hour: u32,
    /// 分 0..=59。
    pub minute: u32,
    /// 秒 0..=59。
    pub second: u32,
}

/// Civil 的派生量计算。
impl Civil {
    /// 该日期对应的 epoch 天数。
    pub fn days(&self) -> i64 {
        days_from_civil(self.year, self.month, self.day)
    }

    /// Weekday 0 = Sunday .. 6 = Saturday.
    /// 星期序号：0=周日..6=周六。
    pub fn weekday(&self) -> u32 {
        weekday_from_days(self.days())
    }
}

/// Split a UTC instant (ms) under a fixed offset (seconds) into civil fields.
/// 把 UTC 毫秒时刻在固定偏移（秒）下拆成 Civil 字段；
/// 用欧几里得除法，负时刻同样正确。
pub fn civil_from_utc_ms(utc_ms: i64, offset_seconds: i64) -> Civil {
    let local_ms = utc_ms + offset_seconds * MS_PER_SECOND;
    let days = local_ms.div_euclid(MS_PER_DAY);
    let rem = local_ms.rem_euclid(MS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    Civil {
        year,
        month,
        day,
        hour: (rem / MS_PER_HOUR) as u32,
        minute: (rem % MS_PER_HOUR / MS_PER_MINUTE) as u32,
        second: (rem % MS_PER_MINUTE / MS_PER_SECOND) as u32,
    }
}

/// Compose a civil timestamp under a fixed offset (seconds) back to UTC ms.
/// civil_from_utc_ms 的逆运算：Civil 字段加固定偏移还原 UTC 毫秒。
pub fn utc_ms_from_civil(c: &Civil, offset_seconds: i64) -> i64 {
    c.days() * MS_PER_DAY
        + c.hour as i64 * MS_PER_HOUR
        + c.minute as i64 * MS_PER_MINUTE
        + c.second as i64 * MS_PER_SECOND
        - offset_seconds * MS_PER_SECOND
}

/// `yyyy-LL-dd HH:mm` in the fixed-offset zone (luxon `toFormat` shape).
/// 在固定偏移时区下格式化为 yyyy-LL-dd HH:mm（对齐 luxon toFormat 输出）。
pub fn format_stamp(utc_ms: i64, offset_seconds: i64) -> String {
    let c = civil_from_utc_ms(utc_ms, offset_seconds);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        c.year, c.month, c.day, c.hour, c.minute
    )
}

// ---------------------------------------------------------------------------
// TZif (RFC 8536) reading
// ---------------------------------------------------------------------------

/// TZif 的一种时区类型：这里只保留 UTC 偏移。
#[derive(Debug)]
struct TzType {
    /// 相对 UTC 的偏移秒数。
    utoff: i64,
}

/// 一个时区文件解析出的全部偏移数据。
#[derive(Debug)]
struct ZoneData {
    /// Transition instants (UTC seconds) and the type index in effect after.
    /// (过渡时刻 UTC 秒, 生效类型下标) 列表，按时间升序。
    transitions: Vec<(i64, usize)>,
    /// 类型表（各偏移值）。
    types: Vec<TzType>,
    /// Type used before the first transition (first non-DST type, else 0).
    /// 首个过渡之前生效的类型下标（优先取非 DST 类型，否则 0）。
    first_type: usize,
}

/// 大端读取 4 字节 u32；越界返回 None。
fn read_u32(b: &[u8], at: usize) -> Option<u32> {
    let bytes: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

/// 大端读取 4 字节 i32；越界返回 None。
fn read_i32(b: &[u8], at: usize) -> Option<i32> {
    let bytes: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
    Some(i32::from_be_bytes(bytes))
}

/// 大端读取 8 字节 i64；越界返回 None。
fn read_i64(b: &[u8], at: usize) -> Option<i64> {
    let bytes: [u8; 8] = b.get(at..at + 8)?.try_into().ok()?;
    Some(i64::from_be_bytes(bytes))
}

/// 解析 TZif 字节流：v1 只有 32 位过渡块；v2+ 先跳过 v1 块再解析 64 位
/// 块；魔数不符或任一字段越界返回 None。
fn parse_tzif(bytes: &[u8]) -> Option<ZoneData> {
    // 解析单个 TZif 数据块，返回 (解析结果, 块尾偏移)。
    let parse_block = |buf: &[u8], time_size: usize| -> Option<(ZoneData, usize)> {
        if buf.len() < 44 || &buf[0..4] != b"TZif" {
            return None;
        }
        let _isutcnt = read_u32(buf, 20)? as usize;
        let isstdcnt = read_u32(buf, 24)? as usize;
        let leapcnt = read_u32(buf, 28)? as usize;
        let timecnt = read_u32(buf, 32)? as usize;
        let typecnt = read_u32(buf, 36)? as usize;
        let charcnt = read_u32(buf, 40)? as usize;
        let mut at = 44usize;
        let mut transitions = Vec::with_capacity(timecnt);
        for _ in 0..timecnt {
            let t = if time_size == 8 {
                read_i64(buf, at)?
            } else {
                read_i32(buf, at)? as i64
            };
            at += time_size;
            transitions.push(t);
        }
        let mut idx = Vec::with_capacity(timecnt);
        for _ in 0..timecnt {
            idx.push(*buf.get(at)? as usize);
            at += 1;
        }
        let mut raw_types = Vec::with_capacity(typecnt);
        for _ in 0..typecnt {
            let utoff = read_i32(buf, at)? as i64;
            let is_dst = *buf.get(at + 4)? != 0;
            at += 6;
            raw_types.push((utoff, is_dst));
        }
        at += charcnt;
        at += leapcnt * (time_size + 4);
        at += isstdcnt;
        at += _isutcnt;
        let types = raw_types
            .iter()
            .map(|(utoff, _)| TzType { utoff: *utoff })
            .collect::<Vec<_>>();
        let pairs = transitions.into_iter().zip(idx).collect::<Vec<_>>();
        let first_type = raw_types
            .iter()
            .position(|(_, is_dst)| !is_dst)
            .unwrap_or(0);
        Some((
            ZoneData {
                transitions: pairs,
                types,
                first_type,
            },
            at,
        ))
    };

    let version = *bytes.get(4)?;
    if version == 0 {
        // v1: 32-bit transition times only.
        return parse_block(bytes, 4).map(|(zone, _)| zone);
    }
    // v2+: skip the v1 block, parse the 64-bit block.
    let (_, v1_len) = parse_block(bytes, 4)?;
    let rest = bytes.get(v1_len..)?;
    parse_block(rest, 8).map(|(zone, _)| zone)
}

/// zoneinfo 根目录：优先非空 TZDIR 环境变量，默认 /usr/share/zoneinfo。
fn zoneinfo_root() -> PathBuf {
    std::env::var("TZDIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/share/zoneinfo"))
}

/// 进程级时区缓存表的访问入口（LazyLock 只初始化一次）。
fn zone_cache() -> &'static Mutex<HashMap<String, Arc<ZoneData>>> {
    // zone 名 → 已解析 ZoneData 的共享缓存。
    static CACHE: LazyLock<Mutex<HashMap<String, Arc<ZoneData>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &CACHE
}

/// `luxon.IANAZone.isValidZone` equivalent: the zone resolves to a readable
/// TZif file. Path traversal is rejected; `UTC` always validates.
/// 校验 IANA 时区名：非空、不超 100 字符，拒绝绝对路径/反斜杠/`..`
/// 穿越，`UTC` 恒真；其余以能否读到合法 TZif 文件为准。
pub fn is_valid_zone(name: &str) -> bool {
    if name.is_empty() || name.len() > 100 {
        return false;
    }
    if name == "UTC" {
        return true;
    }
    if name.contains("..") || name.starts_with('/') || name.contains('\\') {
        return false;
    }
    load_zone(name).is_some()
}

/// 加载（并缓存）一个时区的 ZoneData；读文件或解析失败返回 None。
fn load_zone(name: &str) -> Option<Arc<ZoneData>> {
    if let Some(hit) = zone_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
    {
        return Some(Arc::clone(hit));
    }
    let path = zoneinfo_root().join(name);
    let bytes = std::fs::read(&path).ok()?;
    // Basic size sanity: a TZif file is at least a header.
    if bytes.len() < 44 || !path.is_file() {
        return None;
    }
    let zone = Arc::new(parse_tzif(&bytes)?);
    zone_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name.to_string(), Arc::clone(&zone));
    Some(zone)
}

/// UTC offset (seconds) in effect for `zone` at the given instant (UTC ms).
/// `None` when the zone cannot be resolved; callers decide the fallback.
/// 求时区在给定 UTC 时刻的偏移秒数：沿过渡表线性扫描，取最后一个
/// 不晚于该时刻的过渡；时区无法解析返回 None，由调用方决定回退。
pub fn zone_offset_seconds(zone: &str, utc_ms: i64) -> Option<i64> {
    let zone = load_zone(zone)?;
    let t = utc_ms.div_euclid(MS_PER_SECOND);
    let mut current = zone.first_type;
    for (trans, idx) in &zone.transitions {
        if *trans <= t {
            current = *idx;
        } else {
            break;
        }
    }
    zone.types.get(current).map(|ty| ty.utoff)
}

/// Offset for a zone treated as a fixed offset (`Europe/Kyiv` style zones are
/// DST-free here). Unresolvable zones fall back to UTC (offset 0).
/// zone_offset_seconds 的容错版：时区不可解析时回退 UTC（偏移 0）。
pub fn offset_or_utc(zone: &str, utc_ms: i64) -> i64 {
    zone_offset_seconds(zone, utc_ms).unwrap_or(0)
}

/// The server's local IANA zone name (`DateTime.local().zoneName` analog):
/// `TZ` env (name form), then the `/etc/localtime` symlink, else `UTC`.
/// 服务器本地时区名：先取 TZ 环境变量（名称形式且需通过校验），
/// 再看 /etc/localtime 符号链接的 zoneinfo 后缀，最后回退 "UTC"。
pub fn local_zone_name() -> String {
    if let Some(name) = std::env::var("TZ").ok().and_then(|v| {
        let v = v.trim().trim_start_matches(':').to_string();
        (!v.is_empty() && !v.starts_with('/')).then_some(v)
    }) && is_valid_zone(&name)
    {
        return name;
    }
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        let suffix = zoneinfo_suffix(&target);
        if let Some(name) = suffix
            && is_valid_zone(&name)
        {
            return name;
        }
    }
    "UTC".to_string()
}

/// 从符号链接目标里截取 `zoneinfo/` 之后的时区名；不含该标记返回 None。
fn zoneinfo_suffix(target: &Path) -> Option<String> {
    let text = target.to_string_lossy();
    let marker = "zoneinfo/";
    let idx = text.rfind(marker)?;
    let name = text[idx + marker.len()..].to_string();
    (!name.is_empty()).then_some(name)
}

/// civil 运算与时区解析的单元测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 测试辅助：由 civil 字段拼一个 UTC 毫秒时刻。
    fn utc(y: i64, m: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
        utc_ms_from_civil(
            &Civil {
                year: y,
                month: m,
                day: d,
                hour: h,
                minute: mi,
                second: s,
            },
            0,
        )
    }

    /// 验证 civil 与 epoch 天数在参考日期（含闰日）上互为逆运算。
    #[test]
    fn civil_roundtrip_matches_reference_dates() {
        // 1970-01-01, 2000-02-29 (leap), 2026-09-26.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(days_from_civil(2000, 2, 29)), (2000, 2, 29));
        assert_eq!(civil_from_days(days_from_civil(2026, 9, 26)), (2026, 9, 26));
    }

    /// 验证已知日期的星期序号（周一=1、周日=0、epoch 日=周四 4）。
    #[test]
    fn weekday_matches_known_days() {
        // 2025-01-06 is a Monday, 2026-01-04 a Sunday.
        assert_eq!(weekday_from_days(days_from_civil(2025, 1, 6)), 1);
        assert_eq!(weekday_from_days(days_from_civil(2026, 1, 4)), 0);
        assert_eq!(weekday_from_days(days_from_civil(1970, 1, 1)), 4);
    }

    /// 验证固定偏移下 civil 与 UTC 毫秒往返一致，及 format_stamp 的呈现。
    #[test]
    fn fixed_offset_roundtrip() {
        let ms = utc(2025, 1, 1, 8, 0, 0);
        // Kyiv standard offset +2h: 10:00 local.
        let c = civil_from_utc_ms(ms, 2 * 3600);
        assert_eq!((c.hour, c.day), (10, 1));
        assert_eq!(utc_ms_from_civil(&c, 2 * 3600), ms);
        assert_eq!(format_stamp(ms, 0), "2025-01-01 08:00");
        assert_eq!(format_stamp(ms, 2 * 3600), "2025-01-01 10:00");
    }

    /// 验证从真实 tzdata 解析 Kyiv/纽约冬夏偏移与时区名校验；
    /// 机器无 tzdata 时跳过（UTC 回退仍成立）。
    #[test]
    fn resolves_zone_offsets_from_tzdata() {
        if !Path::new("/usr/share/zoneinfo/Europe/Kyiv").exists() {
            return; // no tzdata on this machine; UTC fallback still holds
        }
        let jan = utc(2025, 1, 15, 0, 0, 0);
        let jul = utc(2025, 7, 15, 0, 0, 0);
        let winter = zone_offset_seconds("Europe/Kyiv", jan).expect("kyiv resolves");
        let summer = zone_offset_seconds("Europe/Kyiv", jul).expect("kyiv resolves");
        // Kyiv still observes DST (EET +2 / EEST +3) in current tzdata.
        assert_eq!(winter, 2 * 3600);
        assert_eq!(summer, 3 * 3600);

        let ny_jul = zone_offset_seconds("America/New_York", jul).expect("nyc resolves");
        let ny_jan = zone_offset_seconds("America/New_York", jan).expect("nyc resolves");
        assert_eq!(ny_jul, -4 * 3600);
        assert_eq!(ny_jan, -5 * 3600);

        assert!(is_valid_zone("Europe/Kyiv"));
        assert!(is_valid_zone("UTC"));
        assert!(!is_valid_zone("Not/AZone"));
        assert!(!is_valid_zone("../etc/passwd"));
        assert_eq!(zone_offset_seconds("Not/AZone", jan), None);
        assert_eq!(offset_or_utc("Not/AZone", jan), 0);
    }

    /// 验证闰年规则下二月天数（1900 平、2000 闰）。
    #[test]
    fn days_in_month_handles_leap_years() {
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2025, 2), 28);
        assert_eq!(days_in_month(2000, 2), 29);
        assert_eq!(days_in_month(1900, 2), 28);
    }
}
