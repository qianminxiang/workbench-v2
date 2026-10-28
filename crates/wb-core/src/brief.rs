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
    /// 最近一次成功送达时的应用版本号（FR-6，R6-1：只认 tauri.conf.json
    /// 注入的包版本，窗口标题字符串不参与判定）。**任何一次成功送达**（准点/
    /// 补发/版本事件）都会更新它——语义是「上一次送达发生在哪个版本」。
    /// 缺省表示从未送达过：首装不算版本事件，走现有 DueLate（AC-6-4）。
    #[serde(default)]
    pub last_delivered_version: Option<String>,
    /// 最近一次尝试送达的时刻（ISO），仅用于排查，不参与判定。
    pub last_attempt_at: Option<String>,
}

/// FR-6 版本即时启动的触发信息（升级或降级，AC-6-6 同规则）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionBootEvent {
    pub previous_version: String,
    pub current_version: String,
}

/// FR-6 版本即时启动判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionBootDecision {
    /// 不触发：未到 9:00（AC-6-3）/ 从未有版本送达记录（首装，AC-6-4）/
    /// 本版本已送达过（AC-6-2 每版本至多一次）
    Skip,
    /// 立即送达一次，标题/正文标注来源「新版本」（AC-6-1 / AC-6-6）
    Deliver(VersionBootEvent),
}

/// FR-6：应用启动时的「晨报随新版本即时启动」判定。
///
/// 与 `decide` 相互独立：当日已在旧版本送达过（`decide` 返回
/// AlreadyDelivered）正是本判定要解决的核心空窗（AC-6-1）；反过来首装没有
/// 版本记录，本判定返回 Skip，交给现有 DueLate 补发，不叠加二次推送
/// （AC-6-4，R6-2：版本事件与补发冲突时只送达一次）。
pub fn decide_version_boot(
    now_minutes: u32,
    app_version: &str,
    state: &DeliveryState,
) -> VersionBootDecision {
    // AC-6-3：未到 9:00 不即时启动，由现有 9:00 调度线程正常触发
    if now_minutes < BRIEF_HOUR * 60 + BRIEF_MINUTE {
        return VersionBootDecision::Skip;
    }
    match state.last_delivered_version.as_deref() {
        // AC-6-4：首次安装（从未记录过送达版本）不算版本事件
        None => VersionBootDecision::Skip,
        // AC-6-2：同一版本重复启动不重复推
        Some(prev) if prev == app_version => VersionBootDecision::Skip,
        // AC-6-1 / AC-6-6：版本变化（升级、降级同规则）→ 立即送达一次
        Some(prev) => VersionBootDecision::Deliver(VersionBootEvent {
            previous_version: prev.to_string(),
            current_version: app_version.to_string(),
        }),
    }
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
    let mut lines = brief_body_lines(brief);
    if decision.verdict == BriefVerdict::DueLate {
        lines.push(format!(
            "应用当时未运行，已于 {} 分钟后补发",
            decision.late_minutes
        ));
    }
    (title, lines.join("\n"))
}

/// FR-6 版本事件晨报的通知：与常规晨报共享同一批事实（`brief_body_lines`），
/// 差异只有两处——标题来源标记「新版本」，正文末行说明触发原因。
pub fn version_boot_notification(brief: &MorningBrief, ev: &VersionBootEvent) -> (String, String) {
    let title = format!("晨报（新版本）· {}", brief.brief_date);
    let mut lines = brief_body_lines(brief);
    lines.push(format!(
        "检测到版本更新 {} → {}，已即时送达本次晨报",
        ev.previous_version, ev.current_version
    ));
    (title, lines.join("\n"))
}

/// 通知正文的公共事实段：三计数 + 最多三条要点 + 其余条数 + 周一附周聚合。
/// 准点/补发/版本事件三种来源共用，保证用户在任何入口看到的是同一批事实。
fn brief_body_lines(brief: &MorningBrief) -> Vec<String> {
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
    lines
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

    // ── FR-6 晨报随新版本即时启动（AC-6-1 ~ AC-6-7 纯函数部分）──

    fn vstate(date: Option<&str>, version: Option<&str>) -> DeliveryState {
        DeliveryState {
            last_delivered_date: date.map(String::from),
            last_delivered_version: version.map(String::from),
            last_attempt_at: None,
        }
    }

    #[test]
    fn version_boot_fires_after_same_day_delivery_on_old_version() {
        // AC-6-1：当日 9:00 后、晨报已在旧版本送达、升级启动 → 立即送达一次。
        // 这正是 decide() 返回 AlreadyDelivered 的空窗——两条判定要同时成立。
        let today = CivilDate::new(2026, 10, 28);
        let st = vstate(Some("2026-10-28"), Some("2.1.0"));
        assert_eq!(decide(today, 14 * 60 + 30, &st).verdict, BriefVerdict::AlreadyDelivered);
        assert_eq!(
            decide_version_boot(14 * 60 + 30, "2.2.0", &st),
            VersionBootDecision::Deliver(VersionBootEvent {
                previous_version: "2.1.0".into(),
                current_version: "2.2.0".into(),
            })
        );
    }

    #[test]
    fn version_boot_same_version_not_repeated() {
        // AC-6-2：送达后重启同一版本 → 不重复触发。
        let st = vstate(Some("2026-10-28"), Some("2.2.0"));
        assert_eq!(decide_version_boot(15 * 60, "2.2.0", &st), VersionBootDecision::Skip);
        // 次日同一版本也不算版本事件（AC-6-7：次日只走一次正常 9:00 送达）
        let st_next_day = vstate(Some("2026-10-28"), Some("2.2.0"));
        assert_eq!(decide_version_boot(9 * 60, "2.2.0", &st_next_day), VersionBootDecision::Skip);
    }

    #[test]
    fn version_boot_before_nine_waits_for_timer() {
        // AC-6-3：本地时间 8:59，版本已升级 → 不即时送达，交给现有调度线程。
        let st = vstate(Some("2026-10-27"), Some("2.1.0"));
        assert_eq!(decide_version_boot(8 * 60 + 59, "2.2.0", &st), VersionBootDecision::Skip);
        // 9:00 整点起版本事件才可接管（与 BRIEF_HOUR 口径一致）
        assert!(matches!(
            decide_version_boot(9 * 60, "2.2.0", &st),
            VersionBootDecision::Deliver(_)
        ));
    }

    #[test]
    fn version_boot_first_install_defers_to_due_late() {
        // AC-6-4：首次安装（无 last_delivered_version）10:00 启动 → 不算版本事件，
        // 由现有 DueLate 补发；两路判定此刻一虚一实，不叠加二次推送。
        let today = CivilDate::new(2026, 10, 28);
        let st = vstate(None, None);
        assert_eq!(decide_version_boot(10 * 60, "2.2.0", &st), VersionBootDecision::Skip);
        assert_eq!(decide(today, 10 * 60, &st).verdict, BriefVerdict::DueLate);
    }

    #[test]
    fn version_boot_downgrade_counts_as_change() {
        // AC-6-6：降级（v2.2.0 → v2.1.0，回滚演练）判定为版本变化，同规则送达。
        let st = vstate(Some("2026-10-28"), Some("2.2.0"));
        assert_eq!(
            decide_version_boot(14 * 60 + 30, "2.1.0", &st),
            VersionBootDecision::Deliver(VersionBootEvent {
                previous_version: "2.2.0".into(),
                current_version: "2.1.0".into(),
            })
        );
    }

    #[test]
    fn version_boot_fires_even_when_today_undelivered() {
        // R6-2：昨天在旧版本送达、今天 10:00 升级启动（今天还没送）——
        // 版本事件优先、只送达一次：版本判定 Deliver，常规判定 DueLate，
        // 上层先走版本事件，送达后两路同时满足（不再重复送）。
        let today = CivilDate::new(2026, 10, 28);
        let st = vstate(Some("2026-10-27"), Some("2.1.0"));
        assert_eq!(decide(today, 10 * 60, &st).verdict, BriefVerdict::DueLate);
        assert!(matches!(
            decide_version_boot(10 * 60, "2.2.0", &st),
            VersionBootDecision::Deliver(_)
        ));
    }

    #[test]
    fn version_boot_notification_marks_source() {
        // PRD 输出形态：标题「晨报（新版本）· 日期」，正文末行说明触发原因。
        let d = CivilDate::new(2026, 10, 28);
        let brief = morning_brief(d, &[], &Ruleset::default());
        let ev = VersionBootEvent {
            previous_version: "2.1.0".into(),
            current_version: "2.2.0".into(),
        };
        let (title, body) = version_boot_notification(&brief, &ev);
        assert_eq!(title, format!("晨报（新版本）· {}", brief.brief_date));
        assert!(
            body.contains("检测到版本更新 2.1.0 → 2.2.0，已即时送达本次晨报"),
            "正文末行必须说明触发原因：{}",
            body
        );
        // 共享事实段：与常规通知同一批计数
        assert!(body.contains(&format!(
            "今日到期 {} · 临期 {} · 逾期 {}",
            brief.due_today.len(),
            brief.due_soon.len(),
            brief.overdue.len()
        )));
    }

    #[test]
    fn delivery_state_deserializes_legacy_json_without_version() {
        // 旧 last_brief.json（v2.1 落盘，无 last_delivered_version 字段）必须能
        // 无损读入：版本字段缺省为 None，首启等价于「无版本记录」走 DueLate。
        let legacy = serde_json::json!({
            "last_delivered_date": "2026-10-27",
            "last_attempt_at": "2026-10-27T09:00:05+08:00"
        });
        let st: DeliveryState = serde_json::from_value(legacy).expect("旧版 JSON 必须可读");
        assert_eq!(st.last_delivered_date.as_deref(), Some("2026-10-27"));
        assert_eq!(st.last_delivered_version, None);
    }
}
