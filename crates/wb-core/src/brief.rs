//! 晨报送达语义（架构 §6.2 规则 D / PRD B7）。
//!
//! 这一层刻意只做**判定与措辞**，不碰系统通知、不碰线程、不读系统时钟：
//! 系统层（Tauri 壳）负责定时与通知，本模块只回答两个问题——
//! 「此刻该不该送达」「送达的内容该长什么样」。这样验收语义可以被单测穷举，
//! 而不是只能在实机上碰运气。
//!
//! 四条不可妥协的口径（与 PRD B7 验收条款逐条对应）：
//! 1. **各端本地时钟**：只在本地 9:00 触发，不依赖服务端或网络（断网照常）；
//! 2. **迟到补发**：9:00 之后才启动/唤醒，当天晨报未送达就立即补发，并标明「补发」；
//! 3. **周一附周聚合**：周聚合由 `rules::morning_brief` 提供，本模块只决定是否附上；
//! 4. **免打扰**：22:00–8:00 不弹系统通知，但晨报本身不丢，改为应用内提示。

use serde::{Deserialize, Serialize};
use crate::date::CivilDate;
use crate::rules::MorningBrief;

/// 晨报固定触发时刻：本地 9:00（PRD B7 验收①）。
pub const BRIEF_HOUR: u32 = 9;
pub const BRIEF_MINUTE: u32 = 0;

/// 免打扰窗口（设置页可改，首版默认 22:00–8:00）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuietHours {
    pub start_minute: u32,
    pub end_minute: u32,
}

impl Default for QuietHours {
    fn default() -> Self {
        QuietHours { start_minute: 22 * 60, end_minute: 8 * 60 }
    }
}

impl QuietHours {
    /// 是否落在免打扰窗口内。跨零点用「或」而不是区间比较，否则 22:00–8:00 会被判成空集。
    pub fn contains(&self, minute_of_day: u32) -> bool {
        if self.start_minute == self.end_minute {
            return false;
        }
        if self.start_minute < self.end_minute {
            minute_of_day >= self.start_minute && minute_of_day < self.end_minute
        } else {
            minute_of_day >= self.start_minute || minute_of_day < self.end_minute
        }
    }
}

/// 送达状态（落地为数据目录下的 `last_brief.json`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeliveryState {
    /// 最近一次**成功送达**的晨报日期（YYYY-MM-DD）。缺省表示从未送达。
    pub last_delivered_date: Option<String>,
    /// 最近一次尝试送达的时刻（ISO），仅用于排查，不参与判定。
    pub last_attempt_at: Option<String>,
}

/// 此刻的送达判定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BriefVerdict {
    /// 今天已送达，本轮无事可做
    AlreadyDelivered,
    /// 未到 9:00，等定时器
    NotYetDue,
    /// 应送达（准点）
    Due,
    /// 应补发（已过 9:00 但今天未送达）
    DueLate,
}

impl BriefVerdict {
    pub fn is_due(&self) -> bool {
        matches!(self, BriefVerdict::Due | BriefVerdict::DueLate)
    }
}

/// 判定结果，连同需要送出的晨报日期（补发时就是当天，不做倒补多天——
/// 过期的盘点清单没有行动价值，反而制造噪音）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefDecision {
    pub verdict: BriefVerdict,
    /// 晨报所属日期（YYYY-MM-DD）
    pub brief_date: String,
    /// 距应触发时刻迟到了多少分钟（准点为 0）
    pub late_minutes: u32,
    /// 是否因应用未运行而跨过了至少一整天（用于「昨日未送达」提示）
    pub missed_previous_day: bool,
}

/// 送达判定。`now_minutes` 是本地时间的「当日分钟数」（0–1439）。
pub fn decide(today: CivilDate, now_minutes: u32, state: &DeliveryState) -> BriefDecision {
    let iso = today.to_iso();
    let delivered_today = state.last_delivered_date.as_deref() == Some(iso.as_str());
    let due_minute = BRIEF_HOUR * 60 + BRIEF_MINUTE;

    let missed_previous_day = match state.last_delivered_date.as_deref() {
        None => true,
        Some(d) => CivilDate::parse(d).map(|d| d < today).unwrap_or(true),
    };

    let verdict = if delivered_today {
        BriefVerdict::AlreadyDelivered
    } else if now_minutes < due_minute {
        BriefVerdict::NotYetDue
    } else if now_minutes == due_minute {
        BriefVerdict::Due
    } else {
        BriefVerdict::DueLate
    };

    BriefDecision {
        verdict,
        brief_date: iso,
        late_minutes: now_minutes.saturating_sub(due_minute),
        missed_previous_day,
    }
}

/// 通知是否应抑制（免打扰）。抑制通知不等于取消晨报：`decision` 仍然 due，
/// 系统层要把「未弹通知」如实反映到应用内提示，绝不静默吞掉。
pub fn notification_suppressed(now_minutes: u32, quiet: QuietHours) -> bool {
    quiet.contains(now_minutes)
}

/// 一天里距离下一次 9:00 触发还有多少分钟（用于定时器排程，0 表示就是现在）。
pub fn minutes_until_next_fire(now_minutes: u32) -> u32 {
    let due = BRIEF_HOUR * 60 + BRIEF_MINUTE;
    if now_minutes < due {
        due - now_minutes
    } else {
        // 已过 9:00：下一次是明天 9:00
        24 * 60 - now_minutes + due
    }
}

/// 系统通知的标题与正文（B7 送达内容）。措辞只为「看一眼就够行动」：
/// 三个计数 + 最高优先级的少数几条标题，不把 49 条逾期全塞进通知。
pub fn notification_text(brief: &MorningBrief, decision: &BriefDecision) -> (String, String) {
    let title = if decision.verdict == BriefVerdict::DueLate {
        format!("晨报（补发）· {}", brief.brief_date)
    } else {
        format!("个人工作台 · 晨报 · {}", brief.brief_date)
    };

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "今日到期 {} · 临期 {} · 逾期 {}",
        brief.due_today.len(),
        brief.due_soon.len(),
        brief.overdue.len()
    ));

    // 先给今天必须动的，再给逾期的：通知里最多三条，多则只说条数
    let mut picks: Vec<String> = Vec::new();
    for a in brief.due_today.iter() {
        picks.push(format!("今日到期：{}", a.title));
        if picks.len() >= 3 {
            break;
        }
    }
    if picks.len() < 3 {
        for a in brief.overdue.iter() {
            picks.push(format!("逾期：{}", a.title));
            if picks.len() >= 3 {
                break;
            }
        }
    }
    if !picks.is_empty() {
        lines.push(picks.join("\n"));
    }
    let rest = brief.due_today.len() + brief.due_soon.len() + brief.overdue.len();
    if rest > picks.len() {
        lines.push(format!("其余 {} 项见应用内晨报页", rest - picks.len()));
    }

    // 周一附周聚合（规则 D：周一只出周聚合）
    if let Some(w) = brief.weekly.as_ref() {
        lines.push(format!(
            "本周（{} 至 {}）应闭环 {} · 应启动 {}",
            w.week_start,
            w.week_end,
            w.should_close.len(),
            w.should_start.len()
        ));
    }

    if decision.verdict == BriefVerdict::DueLate {
        lines.push(format!(
            "应用当时未运行，已于 {} 分钟后补发",
            decision.late_minutes
        ));
    }

    (title, lines.join("\n"))
}

/// 语音播报要点（F10 上线）。与晨报四段结构一一对应：计数 → 三条要点 →
/// 周聚合 → 补发说明，**读要点不逐字念全部**——播报是伴随行为，超过 30 秒
/// 就变成打扰。与 `notification_text` 共享同一批事实，只换措辞密度。
pub fn speech_text(brief: &MorningBrief, decision: &BriefDecision) -> String {
    let mut parts: Vec<String> = Vec::new();
    if decision.verdict == BriefVerdict::DueLate {
        parts.push("早上好。现在是补发晨报。".to_string());
    } else {
        parts.push("早上好。今日晨报。".to_string());
    }
    parts.push(format!(
        "今日到期 {} 项，临期 {} 项，逾期 {} 项。",
        brief.due_today.len(),
        brief.due_soon.len(),
        brief.overdue.len()
    ));
    let mut picks: Vec<String> = Vec::new();
    for a in brief.due_today.iter() {
        picks.push(format!("今日到期：{}。", a.title));
        if picks.len() >= 3 {
            break;
        }
    }
    if picks.len() < 3 {
        for a in brief.overdue.iter() {
            picks.push(format!("逾期：{}。", a.title));
            if picks.len() >= 3 {
                break;
            }
        }
    }
    if !picks.is_empty() {
        parts.push(picks.join(""));
    }
    if let Some(w) = brief.weekly.as_ref() {
        parts.push(format!(
            "本周应闭环 {} 项，应启动 {} 项。",
            w.should_close.len(),
            w.should_start.len()
        ));
    }
    parts.join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Board, ItemStatus, Priority, WorkItem};
    use crate::rules::{morning_brief, Ruleset};

    fn item(title: &str, start: &str, end: &str, progress: i64, status: ItemStatus) -> WorkItem {
        WorkItem {
            id: format!("fy2026-{}", title.len()),
            seq_no: 1,
            board: Board::Gov,
            title: title.into(),
            category: None,
            priority: Some(Priority::A),
            target_quant: None,
            planned_start_date: Some(start.into()),
            planned_start_precision: "day".into(),
            planned_start_raw: Some(start.into()),
            planned_end_date: Some(end.into()),
            planned_end_precision: "day".into(),
            planned_end_raw: Some(end.into()),
            progress_pct: progress,
            status: Some(status),
            milestone_stage: None,
            next_action: None,
            owner: None,
            source_sheet: None,
            fiscal_year: 2026,
            ext: Default::default(),
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn quiet_hours_cross_midnight() {
        let q = QuietHours::default(); // 22:00–8:00
        assert!(q.contains(23 * 60));
        assert!(q.contains(0));
        assert!(q.contains(7 * 60 + 59));
        assert!(!q.contains(8 * 60)); // 端点开区间
        assert!(!q.contains(9 * 60)); // 晨报时刻不在免打扰内
    }

    #[test]
    fn nine_oclock_is_due_and_earlier_waits() {
        let d = CivilDate::new(2026, 10, 27);
        let s = DeliveryState::default();
        assert_eq!(decide(d, 8 * 60 + 59, &s).verdict, BriefVerdict::NotYetDue);
        assert_eq!(decide(d, 9 * 60, &s).verdict, BriefVerdict::Due);
    }

    #[test]
    fn delivered_today_is_not_redelivered() {
        let d = CivilDate::new(2026, 10, 27);
        let s = DeliveryState { last_delivered_date: Some("2026-10-27".into()), ..Default::default() };
        assert_eq!(decide(d, 15 * 60, &s).verdict, BriefVerdict::AlreadyDelivered);
    }

    #[test]
    fn late_start_is_caught_up_and_marked_late() {
        // 应用未运行到 14:30 才启动：当天晨报必须补发，并记录迟到分钟数
        let d = CivilDate::new(2026, 10, 27);
        let s = DeliveryState { last_delivered_date: Some("2026-10-26".into()), ..Default::default() };
        let dec = decide(d, 14 * 60 + 30, &s);
        assert_eq!(dec.verdict, BriefVerdict::DueLate);
        assert_eq!(dec.late_minutes, 330);
        assert!(dec.missed_previous_day);
    }

    #[test]
    fn next_fire_rolls_over_to_tomorrow() {
        assert_eq!(minutes_until_next_fire(8 * 60), 60);
        assert_eq!(minutes_until_next_fire(9 * 60), 24 * 60);
        assert_eq!(minutes_until_next_fire(23 * 60), 10 * 60);
    }

    #[test]
    fn monday_brief_carries_weekly_and_notification_says_so() {
        // 2026-10-26 是周一（规则 D：周一只出周聚合）
        let monday = CivilDate::new(2026, 10, 26);
        let items = vec![
            item("本周应启动的事项", "2026-10-28", "2026-11-10", 0, ItemStatus::Pending),
            item("本周应闭环的事项", "2026-09-01", "2026-10-30", 50, ItemStatus::Active),
        ];
        let brief = morning_brief(monday, &items, &Ruleset::default());
        assert!(brief.weekly.is_some(), "周一必须带周聚合");
        let dec = decide(monday, 9 * 60, &DeliveryState::default());
        let (title, body) = notification_text(&brief, &dec);
        assert!(title.contains("晨报"));
        assert!(body.contains("本周"), "通知正文应带周聚合：{}", body);

        // 非周一不带
        let tuesday = CivilDate::new(2026, 10, 27);
        let b2 = morning_brief(tuesday, &items, &Ruleset::default());
        assert!(b2.weekly.is_none());
    }

    #[test]
    fn late_delivery_text_explains_catch_up() {
        let d = CivilDate::new(2026, 10, 27);
        let brief = morning_brief(d, &[], &Ruleset::default());
        let dec = decide(d, 11 * 60, &DeliveryState::default());
        let (title, body) = notification_text(&brief, &dec);
        assert!(title.contains("补发"), "补发必须在标题里说清楚：{}", title);
        assert!(body.contains("补发"));
    }

    #[test]
    fn speech_text_covers_four_sections_without_full_readout() {
        // F10：播报要点与晨报四段对齐——计数、要点、周聚合、补发说明
        let monday = CivilDate::new(2026, 10, 26);
        let items = vec![
            item("今日必须动的关键事项", "2026-10-26", "2026-11-10", 0, ItemStatus::Pending),
            item("已经逾期的遗留事项", "2026-09-01", "2026-10-01", 30, ItemStatus::Active),
        ];
        let brief = morning_brief(monday, &items, &Ruleset::default());
        let dec = decide(monday, 9 * 60, &DeliveryState::default());
        let s = speech_text(&brief, &dec);
        assert!(s.contains("今日晨报"));
        // 计数措辞必须与晨报结构同源（不硬猜规则口径，防止测试脆弱）
        let counts = format!(
            "今日到期 {} 项，临期 {} 项，逾期 {} 项。",
            brief.due_today.len(),
            brief.due_soon.len(),
            brief.overdue.len()
        );
        assert!(s.contains(&counts), "播报必须带计数段：{} / {}", s, counts);
        assert!(s.contains("本周应闭环"), "周一必须播报周聚合：{}", s);
        // 要点最多 3 条：绝不逐字念全部
        let due = s.matches("今日到期：").count() + s.matches("逾期：").count();
        assert!(due <= 3, "播报要点必须截断在 3 条：{}", s);
    }

    #[test]
    fn speech_text_marks_late_catch_up() {
        let d = CivilDate::new(2026, 10, 27);
        let brief = morning_brief(d, &[], &Ruleset::default());
        let dec = decide(d, 14 * 60 + 30, &DeliveryState::default());
        let s = speech_text(&brief, &dec);
        assert!(s.starts_with("早上好。现在是补发晨报。"), "补发播报开头必须说明：{}", s);
    }
}
