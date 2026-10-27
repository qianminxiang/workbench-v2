//! 本地时钟换算纯函数（B7① 修复层）。
//!
//! **缺陷根因**：壳的 `today()`/`now_minutes()` 曾直接截取 `now_iso()` 的
//! UTC 字段——UTC+8 下晨报 9:00 要拖到本地 17:00 才成立、免打扰窗口整体
//! 错 8 小时（本地 08:00–16:59 之间完全不投递）、00:00–07:59 日期判成前一天。
//!
//! **修复边界（不可破）**：`store::now_iso()` 本体不动——sync LWW 依赖它输出
//! 带 Z 的 UTC 单调串。本地时区偏移只在本模块与壳的两个换算点生效：
//! 壳用系统层取偏移分钟数，把换算委托给这里的纯函数，从而 UTC+8 语义可以
//! 在 test-core 里被穷举单测，而不是只能在真机上碰运气。
//!
//! 偏移口径与 POSIX `timezone`/chrono `local_minus_utc` 一致：东八区 = +480。

use crate::date::CivilDate;

/// UTC ISO 时刻（`store::now_iso()` 的输出格式 `YYYY-MM-DDTHH:MM:SSZ`）
/// + 本地偏移分钟 → 本地自然日与本地当日分钟数（0–1439）。
///
/// 跨日、跨月、跨年、负偏移（西半球）全部通过 `CivilDate::add_days`
/// 走序号换算，不手写历法。解析失败返回 None（调用方回退旧口径）。
pub fn civil_and_minutes(utc_iso: &str, offset_minutes: i32) -> Option<(CivilDate, u32)> {
    let bytes = utc_iso.as_bytes();
    if bytes.len() < 16 || bytes[10] != b'T' {
        return None;
    }
    let date = CivilDate::parse(&utc_iso[..10])?;
    let hh: i32 = utc_iso[11..13].parse().ok()?;
    let mm: i32 = utc_iso[14..16].parse().ok()?;
    if !(0..=23).contains(&hh) || !(0..=59).contains(&mm) {
        return None;
    }
    let total = hh * 60 + mm + offset_minutes;
    let day_shift = total.div_euclid(24 * 60);
    let minute_of_day = total.rem_euclid(24 * 60);
    Some((date.add_days(day_shift as i64), minute_of_day as u32))
}

/// `HH:MM` → 当日分钟数。设置页免打扰 time input 的唯一解析通道，
/// 与浏览器 `<input type="time">` 的 value 格式一一对应。
pub fn parse_hhmm(s: &str) -> Option<u32> {
    let s = s.trim();
    if s.len() != 5 || s.as_bytes()[2] != b':' {
        return None;
    }
    let h: u32 = s[0..2].parse().ok()?;
    let m: u32 = s[3..5].parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

/// 当日分钟数 → `HH:MM`（`parse_hhmm` 的逆，用于把存量值回填给 time input）。
pub fn fmt_hhmm(minute_of_day: u32) -> String {
    let m = minute_of_day.min(24 * 60 - 1);
    format!("{:02}:{:02}", m / 60, m % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brief::{self, BriefVerdict, DeliveryState, QuietHours};

    /// UTC+8 偏移（中国大陆全境，无夏令时）。
    const UTC8: i32 = 8 * 60;

    #[test]
    fn utc_0100_plus8_is_local_0900_brief_fires_on_time() {
        // B7① 主回归：UTC 01:00 = 本地 09:00，晨报必须准点成立。
        let (d, m) = civil_and_minutes("2026-10-27T01:00:00Z", UTC8).unwrap();
        assert_eq!(d, CivilDate::new(2026, 10, 27));
        assert_eq!(m, 9 * 60);
        let dec = brief::decide(d, m, &DeliveryState::default());
        assert_eq!(dec.verdict, BriefVerdict::Due);
    }

    #[test]
    fn utc_0059_plus8_is_local_0859_brief_waits() {
        // 修复前：UTC 00:59 被判成本地 00:59 → 晨报在免打扰窗口里被吞到 17:00；
        // 修复后：本地 08:59，未到 9:00，安静地等。
        let (d, m) = civil_and_minutes("2026-10-27T00:59:00Z", UTC8).unwrap();
        assert_eq!(m, 8 * 60 + 59);
        let dec = brief::decide(d, m, &DeliveryState::default());
        assert_eq!(dec.verdict, BriefVerdict::NotYetDue);
        // 且该时刻不在免打扰窗口内（默认 22:00–8:00），通知不会被抑制
        assert!(!QuietHours::default().contains(m));
    }

    #[test]
    fn utc_1600_plus8_rolls_to_next_local_day() {
        // 本地 00:00（次日）必须落到正确的自然日：00:00–07:59 全程免打扰。
        let (d, m) = civil_and_minutes("2026-10-27T16:00:00Z", UTC8).unwrap();
        assert_eq!(d, CivilDate::new(2026, 10, 28));
        assert_eq!(m, 0);
        assert!(QuietHours::default().contains(m));
    }

    #[test]
    fn utc_2000_plus8_on_dec31_rolls_into_next_year() {
        let (d, m) = civil_and_minutes("2026-12-31T20:00:00Z", UTC8).unwrap();
        assert_eq!(d, CivilDate::new(2027, 1, 1));
        assert_eq!(m, 4 * 60);
    }

    #[test]
    fn negative_offset_west_of_utc_shifts_back_a_day() {
        // UTC-5（纽约冬令时）：UTC 03:00 = 当地 22:00（前一天）
        let (d, m) = civil_and_minutes("2026-10-27T03:00:00Z", -5 * 60).unwrap();
        assert_eq!(d, CivilDate::new(2026, 10, 26));
        assert_eq!(m, 22 * 60);
    }

    #[test]
    fn zero_offset_is_identity_and_malformed_input_returns_none() {
        let (d, m) = civil_and_minutes("2026-10-27T09:30:00Z", 0).unwrap();
        assert_eq!((d, m), (CivilDate::new(2026, 10, 27), 9 * 60 + 30));
        assert!(civil_and_minutes("bad", 480).is_none());
        assert!(civil_and_minutes("2026-10-27T9:30:00Z", 480).is_none());
        assert!(civil_and_minutes("2026-10-27T24:00:00Z", 480).is_none());
        assert!(civil_and_minutes("2026-10-27T09:60:00Z", 480).is_none());
    }

    #[test]
    fn quiet_window_follows_local_clock_not_utc() {
        // 免打扰窗口按本地时间：本地 06:30 免打扰（修复前 UTC 06:30 = 本地 14:30 不免打扰）
        let (_, m) = civil_and_minutes("2026-10-26T22:30:00Z", UTC8).unwrap();
        assert_eq!(m, 6 * 60 + 30);
        assert!(QuietHours::default().contains(m));
    }

    #[test]
    fn hhmm_roundtrip_and_validation() {
        assert_eq!(parse_hhmm("22:00"), Some(22 * 60));
        assert_eq!(parse_hhmm("08:00"), Some(8 * 60));
        assert_eq!(parse_hhmm("0:00"), None); // 必须 0 填充两位
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("12:60"), None);
        assert_eq!(parse_hhmm("12:00:00"), None);
        assert_eq!(fmt_hhmm(22 * 60), "22:00");
        assert_eq!(fmt_hhmm(8 * 60), "08:00");
        assert_eq!(fmt_hhmm(0), "00:00");
        for m in [0u32, 59, 8 * 60, 12 * 60 + 1, 23 * 60 + 59] {
            assert_eq!(parse_hhmm(&fmt_hhmm(m)), Some(m));
        }
    }
}
