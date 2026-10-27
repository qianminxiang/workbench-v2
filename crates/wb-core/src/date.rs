//! 日期规范化：raw / date / precision 三元组（架构 §5.2）。
//!
//! 三条语义铁律，全部由本模块显式实现：
//! 1. 「2月」类无日号文本 → 起点锚 **当月 1 号**（PRD F1 定稿口径），
//!    但闭环锚取 **区间末端**（月末），否则所有任务时间窗被系统性压缩一个月。
//! 2. 闭环早于开启 → 闭环年份 +1（跨年推断），否则年底计划全被判逾期。
//! 3. 解析不出或歧义 → `precision = unknown`、`date = None`，进待确认队列，**不做静默猜测**。

use serde::{Deserialize, Serialize};

/// 日期粒度。`month` / `quarter` / `ongoing` / `unknown` 决定提醒窗口宽度，
/// 也决定时间线渲染成区间带还是点。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Precision {
    /// 精确到日
    Day,
    /// 精确到月，锚点为当月首日（起点）或末日（闭环）
    Month,
    /// 精确到季度/半年，锚点为季首（起点）或季末（闭环）
    Quarter,
    /// 「持续」类常驻项：无闭环，不参与临期提醒
    Ongoing,
    /// 无法解析，进待确认队列
    Unknown,
}

impl Precision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Precision::Day => "day",
            Precision::Month => "month",
            Precision::Quarter => "quarter",
            Precision::Ongoing => "ongoing",
            Precision::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Precision {
        match s {
            "day" => Precision::Day,
            "month" => Precision::Month,
            "quarter" => Precision::Quarter,
            "ongoing" => Precision::Ongoing,
            _ => Precision::Unknown,
        }
    }

    /// 该粒度是否已知（可参与时间计算）
    pub fn is_known(&self) -> bool {
        !matches!(self, Precision::Unknown)
    }
}

/// 锚点角色：起点取区间首日，闭环取区间末日。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Start,
    End,
}

/// 规范化结果：`date` 为 `YYYY-MM-DD`，`precision` 为粒度，`raw` 为原文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedDate {
    pub date: Option<String>,
    pub precision: Precision,
    pub raw: String,
    /// 进入待确认队列的原因（`precision == Unknown` 时非空）
    pub reason: Option<String>,
}

impl NormalizedDate {
    fn unknown(raw: &str, reason: &str) -> Self {
        NormalizedDate {
            date: None,
            precision: Precision::Unknown,
            raw: raw.trim().to_string(),
            reason: Some(reason.to_string()),
        }
    }

    pub fn anchor(&self) -> Option<CivilDate> {
        self.date.as_deref().and_then(CivilDate::parse)
    }
}

/// 极简公历日期，避免为几个函数引入整个日历库。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CivilDate {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

impl CivilDate {
    pub fn new(year: i32, month: u32, day: u32) -> Self {
        CivilDate { year, month, day }
    }

    pub fn parse(s: &str) -> Option<CivilDate> {
        let mut it = s.split('-');
        let y = it.next()?.parse().ok()?;
        let m: u32 = it.next()?.parse().ok()?;
        let d: u32 = it.next()?.parse().ok()?;
        if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
            return None;
        }
        Some(CivilDate::new(y, m, d))
    }

    pub fn to_iso(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    pub fn is_leap(year: i32) -> bool {
        (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
    }

    pub fn days_in_month(year: i32, month: u32) -> u32 {
        match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 => {
                if CivilDate::is_leap(year) {
                    29
                } else {
                    28
                }
            }
            _ => 0,
        }
    }

    /// 序号式换算（Howard Hinnant days_from_civil），用于差值计算。
    pub fn to_days(self) -> i64 {
        let y = if self.month <= 2 { self.year - 1 } else { self.year } as i64;
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let m = self.month as i64;
        let d = self.day as i64;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    pub fn from_days(z: i64) -> CivilDate {
        let z = z + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
        CivilDate::new((if m <= 2 { y + 1 } else { y }) as i32, m, d)
    }

    pub fn add_days(self, days: i64) -> CivilDate {
        CivilDate::from_days(self.to_days() + days)
    }

    /// b - a 的整天差
    pub fn diff_days(a: CivilDate, b: CivilDate) -> i64 {
        b.to_days() - a.to_days()
    }
}

/// 中文数字（一~三十）
fn cn_number(s: &str) -> Option<u32> {
    const DIGITS: [char; 10] = ['零', '一', '二', '三', '四', '五', '六', '七', '八', '九'];
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<u32>() {
        return Some(n);
    }
    let chars: Vec<char> = s.chars().collect();
    let digit = |c: char| DIGITS.iter().position(|&d| d == c).map(|p| p as u32);
    match chars.len() {
        1 => {
            // 「十」单独出现读作 10
            if chars[0] == '十' {
                return Some(10);
            }
            digit(chars[0])
        }
        2 => {
            if chars[0] == '十' {
                // 十X → 1X
                return digit(chars[1]).map(|d| d + 10);
            }
            if chars[1] == '十' {
                // X十 → X0
                return digit(chars[0]).map(|d| d * 10);
            }
            None
        }
        3 => {
            // X十Y → XY（如 二十三 → 23、十二日 已由 2 位覆盖）
            if chars[1] == '十' {
                let tens = digit(chars[0])?;
                let ones = digit(chars[2])?;
                return Some(tens * 10 + ones);
            }
            None
        }
        _ => None,
    }
}

fn num(token: &str) -> Option<u32> {
    cn_number(token.trim())
}

/// 校验月/日是否构成合法日期
fn build_date(year: i32, month: u32, day: u32) -> Option<CivilDate> {
    if !(1..=12).contains(&month) {
        return None;
    }
    if day < 1 || day > CivilDate::days_in_month(year, month) {
        return None;
    }
    Some(CivilDate::new(year, month, day))
}

/// 区间切分。
///
/// 分两类处理，顺序不能颠倒：
/// 1. 中文/波浪/破折号这类**无歧义**分隔符（`至`/`到`/`~`/`—`/`–`）直接切；
/// 2. 连字符 `-` 有歧义：它同时是 ISO 日期的分隔符（`2026-04-10`）。
///    因此连字符只在**左右两段各自都能解析成完整日期**时才当区间符——
///    否则 `2026-04-10` 会被切成「2026」和「04-10」，误判成无法解析进待确认队列。
///    两段合法性用真实 fiscal_year 预检（与后续正式解析同口径），
///    避免闰日等年份敏感输入在预检与正式解析间出现判定漂移。
fn split_range(raw: &str, fiscal_year: i32) -> Option<(&str, &str)> {
    for sep in ["至", "到", "~", "—", "–"] {
        if let Some(idx) = raw.find(sep) {
            let (a, b) = raw.split_at(idx);
            let b = &b[sep.len()..];
            if !a.trim().is_empty() && !b.trim().is_empty() {
                return Some((a, b));
            }
        }
    }
    // 连字符：取第一个能让左右两段都成为完整日期的位置
    let mut best: Option<(&str, &str)> = None;
    for (idx, _) in raw.match_indices('-') {
        let (a, rest) = raw.split_at(idx);
        let b = &rest[1..];
        if a.trim().is_empty() || b.trim().is_empty() {
            continue;
        }
        let left_ok = parse_single(a, fiscal_year, Role::Start).is_some();
        let right_ok = parse_single(b, fiscal_year, Role::End).is_some();
        if left_ok && right_ok {
            best = Some((a, b));
            break;
        }
    }
    best
}

/// 单个日期文本规范化（不含区间拆分）。
fn parse_single(raw: &str, year: i32, role: Role) -> Option<(CivilDate, Precision)> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }

    // 持续
    if matches!(s, "持续" | "常驻" | "长期") {
        return Some((CivilDate::new(year, 1, 1), Precision::Ongoing));
    }

    // 年-月-日 / 年/月/日 / 年月日
    let normalized = s.replace(['/', '.'], "-");
    let parts: Vec<&str> = normalized.split('-').filter(|p| !p.is_empty()).collect();
    if parts.len() == 3 {
        if let (Ok(y), Some(m), Some(d)) = (parts[0].trim().parse::<i32>(), num(parts[1]), num(parts[2])) {
            if (1900..=2200).contains(&y) {
                if let Some(date) = build_date(y, m, d) {
                    return Some((date, Precision::Day));
                }
            }
        }
    }

    // 季度：Q2 / 二季度 / 第2季度
    let lower = s.to_lowercase();
    let q_candidate = if let Some(rest) = lower.strip_prefix('q') {
        Some(rest.to_string())
    } else if let Some(rest) = s.strip_prefix('第') {
        Some(rest.trim_end_matches("季度").to_string())
    } else if let Some(rest) = s.strip_suffix("季度") {
        Some(rest.to_string())
    } else {
        None
    };
    if let Some(t) = q_candidate {
        if let Some(q) = num(&t) {
            if (1..=4).contains(&q) {
                let start_month = (q - 1) * 3 + 1;
                let (m, d) = match role {
                    Role::Start => (start_month, 1),
                    Role::End => (start_month + 2, CivilDate::days_in_month(year, start_month + 2)),
                };
                return Some((CivilDate::new(year, m, d), Precision::Quarter));
            }
        }
    }

    // 上半年 / 下半年
    if s.contains("上半年") {
        return Some(match role {
            Role::Start => (CivilDate::new(year, 1, 1), Precision::Quarter),
            Role::End => (CivilDate::new(year, 6, 30), Precision::Quarter),
        });
    }
    if s.contains("下半年") {
        return Some(match role {
            Role::Start => (CivilDate::new(year, 7, 1), Precision::Quarter),
            Role::End => (CivilDate::new(year, 12, 31), Precision::Quarter),
        });
    }

    // 月旬：M月M旬 —— 上旬1 / 中旬11 / 下旬21，粒度按 month（架构 §5.2）
    if let Some(pos) = s.find('旬') {
        let head = &s[..pos];
        let xun = if head.ends_with('上') {
            1
        } else if head.ends_with('中') {
            11
        } else if head.ends_with('下') {
            21
        } else {
            0
        };
        if xun > 0 {
            let month_part = head.trim_end_matches(['上', '中', '下']);
            let mtok = month_part.trim_end_matches('月');
            if let Some(m) = num(mtok) {
                if (1..=12).contains(&m) {
                    let d = if role == Role::End && xun >= 21 {
                        CivilDate::days_in_month(year, m)
                    } else {
                        xun
                    };
                    return Some((CivilDate::new(year, m, d), Precision::Month));
                }
            }
        }
    }

    // M月D日 / M月D号 / M月
    if let Some(pos) = s.find('月') {
        let mtok = &s[..pos];
        let rest = &s[pos + '月'.len_utf8()..];
        let rest = rest.trim_end_matches(['日', '号']).trim();
        if let Some(m) = num(mtok) {
            if rest.is_empty() {
                if (1..=12).contains(&m) {
                    let d = match role {
                        Role::Start => 1,
                        Role::End => CivilDate::days_in_month(year, m),
                    };
                    return Some((CivilDate::new(year, m, d), Precision::Month));
                }
            } else if let Some(d) = num(rest) {
                if let Some(date) = build_date(year, m, d) {
                    return Some((date, Precision::Day));
                }
            }
        }
    }

    None
}

/// 规范化一个日期字段。
///
/// * `raw` —— 原文（如「2月」「3月8日」「持续」）
/// * `fiscal_year` —— 计划年度，**必须显式传入**，不从系统时间推断
/// * `role` —— 起点或闭环
pub fn normalize(raw: &str, fiscal_year: i32, role: Role) -> NormalizedDate {
    let s = raw.trim();
    if s.is_empty() || matches!(s, "待定" | "待确认" | "TBD" | "-" | "—" | "/") {
        return NormalizedDate::unknown(raw, "空值或待定标记，进入待确认队列");
    }

    if let Some((a, b)) = split_range(s, fiscal_year) {
        let side = match role {
            Role::Start => a,
            Role::End => b,
        };
        match parse_single(side, fiscal_year, role) {
            Some((date, precision)) => NormalizedDate {
                date: Some(date.to_iso()),
                precision,
                raw: s.to_string(),
                reason: None,
            },
            None => NormalizedDate::unknown(raw, "区间端点无法解析，进入待确认队列"),
        }
    } else {
        match parse_single(s, fiscal_year, role) {
            Some((date, precision)) => NormalizedDate {
                date: Some(date.to_iso()),
                precision,
                raw: s.to_string(),
                reason: None,
            },
            None => NormalizedDate::unknown(raw, "无法识别日期格式，进入待确认队列（不做静默猜测）"),
        }
    }
}

/// 相对日期基准：手工录入用「下月3号」「下周五」这类写法时必须有一个明确参照日，
/// **不允许从系统时钟隐式推断**——否则单测无法复现，跨零点也会得到两个结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelativeBase {
    pub today: CivilDate,
}

impl RelativeBase {
    pub fn new(today: CivilDate) -> Self {
        RelativeBase { today }
    }

    /// ISO 日期字符串构造（`YYYY-MM-DD`）。解析失败返回 `None`，由调用方决定提示措辞。
    pub fn from_iso(s: &str) -> Option<Self> {
        CivilDate::parse(s).map(RelativeBase::new)
    }
}

/// 解析相对日期文本。返回（目标日期, 粒度）。
///
/// 覆盖手工录入最常出现的几种写法：
/// - `今天` / `明天` / `后天` / `大后天`
/// - `下周X` / `下下周X` / `本周X`（X 为一~日/天）
/// - `下月D号` / `本月D号` / `月底` / `下月底`
/// - `N天后` / `N周后`
fn parse_relative(raw: &str, base: CivilDate, role: Role) -> Option<(CivilDate, Precision)> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }

    // 单日偏移
    let day_offsets: [(&str, i64); 4] =
        [("大后天", 3), ("后天", 2), ("明天", 1), ("今天", 0)];
    for (kw, off) in day_offsets {
        if s == kw {
            return Some((base.add_days(off), Precision::Day));
        }
    }

    // N天后 / N周后 / N个月后
    if let Some(head) = s.strip_suffix("天后") {
        if let Some(n) = num(head) {
            return Some((base.add_days(n as i64), Precision::Day));
        }
    }
    if let Some(head) = s.strip_suffix("周后") {
        if let Some(n) = num(head) {
            return Some((base.add_days(n as i64 * 7), Precision::Day));
        }
    }

    // 下周X / 下下周X / 本周X / 下周/下下周（不带具体日）
    // 周一 = 0；用基准日回推本周一，再加周偏移
    let weekday_idx = |c: char| -> Option<i64> {
        match c {
            '一' => Some(0),
            '二' => Some(1),
            '三' => Some(2),
            '四' => Some(3),
            '五' => Some(4),
            '六' => Some(5),
            '日' | '天' => Some(6),
            _ => None,
        }
    };
    for (kw, week_off) in [("下下周", 2i64), ("下周", 1), ("本周", 0), ("这周", 0)] {
        if let Some(rest) = s.strip_prefix(kw) {
            let base_monday = base.add_days(-((base.to_days() + 3).rem_euclid(7)));
            let rest = rest.trim();
            if rest.is_empty() {
                // 只给到「下周」：粒度是周，锚点取该周周一
                return Some((base_monday.add_days(week_off * 7), Precision::Month));
            }
            if let Some(d) = rest.chars().next().and_then(weekday_idx) {
                return Some((base_monday.add_days(week_off * 7 + d), Precision::Day));
            }
        }
    }

    // 下月底 / 本月底 / 月底
    if s.ends_with("月底") {
        let month_off: i64 = if s.starts_with("下下") {
            2
        } else if s.starts_with("下") {
            1
        } else {
            0
        };
        let m0 = base.month as i64 + month_off;
        let (y, m) = (base.year as i64 + (m0 - 1) / 12, ((m0 - 1) % 12 + 1) as u32);
        let _ = Role::Start; // 月底语义固定取月末，与 role 无关
        return Some((
            CivilDate::new(y as i32, m, CivilDate::days_in_month(y as i32, m)),
            Precision::Month,
        ));
    }

    // 下月D号 / 本月D号 / 下月 / 本月
    for (kw, month_off) in [("下下月", 2i64), ("下月", 1), ("本月", 0), ("这月", 0)] {
        if let Some(rest) = s.strip_prefix(kw) {
            let m0 = base.month as i64 + month_off;
            let y = (base.year as i64 + (m0 - 1) / 12) as i32;
            let m = ((m0 - 1) % 12 + 1) as u32;
            let rest = rest.trim().trim_start_matches('个').trim();
            let rest = rest.trim_end_matches(['日', '号']).trim();
            if rest.is_empty() {
                let d = match role {
                    Role::Start => 1,
                    Role::End => CivilDate::days_in_month(y, m),
                };
                return Some((CivilDate::new(y, m, d), Precision::Month));
            }
            if let Some(d) = num(rest) {
                if let Some(date) = build_date(y, m, d) {
                    return Some((date, Precision::Day));
                }
            }
        }
    }

    None
}

/// 带基准日的规范化：先按绝对写法解析，失败再按相对写法解析。
///
/// 顺序不可反转——「3月8日」若先过相对解析会被当成别的东西；绝对优先才不歧义。
pub fn normalize_with_base(raw: &str, fiscal_year: i32, role: Role, base: RelativeBase) -> NormalizedDate {
    let abs = normalize(raw, fiscal_year, role);
    if abs.precision != Precision::Unknown {
        return abs;
    }
    let s = raw.trim();
    if let Some((date, precision)) = parse_relative(s, base.today, role) {
        // 相对日期可能跨年（12 月的「下月3号」），跨年时同步修正 fiscal_year 维度
        return NormalizedDate {
            date: Some(date.to_iso()),
            precision,
            raw: s.to_string(),
            reason: if date.year != fiscal_year {
                Some(format!("按录入基准日推断落在 {} 年", date.year))
            } else {
                None
            },
        };
    }
    abs
}

/// 带基准日的起止日期规范化（相对写法的成对版本）。
pub fn normalize_pair_with_base(
    raw_start: &str,
    raw_end: &str,
    fiscal_year: i32,
    base: RelativeBase,
) -> (NormalizedDate, NormalizedDate) {
    let mut start = normalize_with_base(raw_start, fiscal_year, Role::Start, base);
    let mut end = normalize_with_base(raw_end, fiscal_year, Role::End, base);
    if let (Some(s), Some(e)) = (start.anchor(), end.anchor()) {
        if e < s {
            let next = CivilDate::new(e.year + 1, e.month, e.day);
            end.date = Some(next.to_iso());
            end.reason = Some("闭环早于开启，已按跨年推断置为次年".to_string());
        }
    }
    // 相对日期被推断到别的年份时，reason 里要留痕（不静默改年度）
    if start.reason.is_none() {
        if let Some(a) = start.anchor() {
            if a.year != fiscal_year {
                start.reason = Some(format!("按录入基准日推断落在 {} 年", a.year));
            }
        }
    }
    (start, end)
}

/// 对一对起止日期做跨年修正：闭环早于开启则闭环年份 +1。
pub fn normalize_pair(
    raw_start: &str,
    raw_end: &str,
    fiscal_year: i32,
) -> (NormalizedDate, NormalizedDate) {
    let start = normalize(raw_start, fiscal_year, Role::Start);
    let mut end = normalize(raw_end, fiscal_year, Role::End);

    if let (Some(s), Some(e)) = (start.anchor(), end.anchor()) {
        if e < s {
            let next = CivilDate::new(e.year + 1, e.month, e.day);
            end.date = Some(next.to_iso());
            end.reason = Some("闭环早于开启，已按跨年推断置为次年".to_string());
        }
    }
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_start_anchor_is_first_day() {
        // 验收： 「2月」→ 当月 1 号
        let r = normalize("2月", 2026, Role::Start);
        assert_eq!(r.date.as_deref(), Some("2026-02-01"));
        assert_eq!(r.precision, Precision::Month);
    }

    #[test]
    fn month_end_anchor_is_month_last_day() {
        // 闭环取区间末端，否则时间窗被压缩一个月
        let r = normalize("6月", 2026, Role::End);
        assert_eq!(r.date.as_deref(), Some("2026-06-30"));
        assert_eq!(r.precision, Precision::Month);
    }

    #[test]
    fn day_precision() {
        let r = normalize("3月8日", 2026, Role::Start);
        assert_eq!(r.date.as_deref(), Some("2026-03-08"));
        assert_eq!(r.precision, Precision::Day);
    }

    #[test]
    fn chinese_numerals() {
        assert_eq!(normalize("三月八日", 2026, Role::Start).date.as_deref(), Some("2026-03-08"));
        assert_eq!(normalize("十月一日", 2026, Role::Start).date.as_deref(), Some("2026-10-01"));
        assert_eq!(normalize("十二月十二日", 2026, Role::Start).date.as_deref(), Some("2026-12-12"));
    }

    #[test]
    fn quarter_and_half_year() {
        assert_eq!(normalize("Q2", 2026, Role::Start).date.as_deref(), Some("2026-04-01"));
        assert_eq!(normalize("二季度", 2026, Role::End).date.as_deref(), Some("2026-06-30"));
        assert_eq!(normalize("下半年", 2026, Role::Start).date.as_deref(), Some("2026-07-01"));
        assert_eq!(normalize("上半年", 2026, Role::End).date.as_deref(), Some("2026-06-30"));
    }

    #[test]
    fn xun_and_range() {
        assert_eq!(normalize("3月中旬", 2026, Role::Start).date.as_deref(), Some("2026-03-11"));
        assert_eq!(normalize("3月下旬", 2026, Role::Start).date.as_deref(), Some("2026-03-21"));
        assert_eq!(normalize("2月-5月", 2026, Role::End).date.as_deref(), Some("2026-05-31"));
    }

    #[test]
    fn ongoing_has_no_deadline_semantics() {
        let r = normalize("持续", 2026, Role::Start);
        assert_eq!(r.precision, Precision::Ongoing);
    }

    #[test]
    fn illegal_month_goes_to_review_queue() {
        let r = normalize("2026-13月", 2026, Role::Start);
        assert_eq!(r.precision, Precision::Unknown);
        assert!(r.date.is_none());
        assert!(r.reason.is_some());
    }

    #[test]
    fn cross_year_inference() {
        let (s, e) = normalize_pair("12月", "1月", 2026);
        assert_eq!(s.date.as_deref(), Some("2026-12-01"));
        assert_eq!(e.date.as_deref(), Some("2027-01-31"));
    }

    #[test]
    fn no_cross_year_when_order_ok() {
        let (s, e) = normalize_pair("2月", "6月", 2026);
        assert_eq!(s.date.as_deref(), Some("2026-02-01"));
        assert_eq!(e.date.as_deref(), Some("2026-06-30"));
    }

    #[test]
    fn leap_year_month_end() {
        assert_eq!(normalize("2月", 2028, Role::End).date.as_deref(), Some("2028-02-29"));
    }

    #[test]
    fn empty_is_unknown_not_guessed() {
        let r = normalize("", 2026, Role::Start);
        assert_eq!(r.precision, Precision::Unknown);
        assert!(r.date.is_none());
    }

    #[test]
    fn single_iso_date_is_not_split_as_range() {
        // 回归：`2026-04-10` 曾被抓进区间切分，「2026」+「04-10」两段都不合法 → 误进待确认
        let n = normalize("2026-04-10", 2026, Role::Start);
        assert_eq!(n.date.as_deref(), Some("2026-04-10"));
        assert_eq!(n.precision, Precision::Day);

        let n2 = normalize("2026-4-1", 2026, Role::Start);
        assert_eq!(n2.date.as_deref(), Some("2026-04-01"));
    }

    #[test]
    fn real_ranges_still_split() {
        // 修 ISO 不能把真正的区间弄坏
        let (a, b) = normalize_pair("2026-04-10 至 2026-05-01", "2026-05-01", 2026);
        assert_eq!(a.date.as_deref(), Some("2026-04-10"), "区间左端要解析出来");
        assert_eq!(b.date.as_deref(), Some("2026-05-01"));

        let (c, d) = normalize_pair("3月-5月", "5月", 2026);
        assert!(c.date.is_some() && d.date.is_some(), "中文区间仍应正常切分");
    }
}

