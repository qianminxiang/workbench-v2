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
//! 3. **周聚合已下线**（2026-11-03 用户拍板「两边都去掉」）：播报与数据两侧整体移除，周一不再附加；
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
        format!("综合工作台 · 晨报 · {}", brief.brief_date)
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

/// 通知正文的公共事实段：三计数 + 最多三条要点 + 其余条数。
/// 准点/补发/版本事件三种来源共用，保证用户在任何入口看到的是同一批事实。
/// （周一附周聚合已随 2026-11-03 拍板整体下线。）
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
    lines
}

/// 语音播报要点（F10 上线）。与晨报结构一一对应：计数 → 三条要点 →
/// 补发说明，**读要点不逐字念全部**——播报是伴随行为，超过 30 秒
/// 就变成打扰。与 `notification_text` 共享同一批事实，只换措辞密度。
/// （周一附周聚合已随 2026-11-03 拍板整体下线，不再出现在播报里。）
pub fn speech_text(brief: &MorningBrief, decision: &BriefDecision) -> String {
    let mut parts: Vec<String> = Vec::new();
    if decision.verdict == BriefVerdict::DueLate {
        parts.push("早上好。现在是补发晨报。".to_string());
    } else {
        parts.push("早上好。今日晨报。".to_string());
    }
    // 当日日期：来源是 brief.brief_date（与界面晨报标题同源），解析后转口语读法。
    // 解析失败（理论上不会发生，brief_date 由 CivilDate::to_iso 生成）时退回 ISO
    // 原串——宁可读得生硬，也不能默默不念日期。
    let spoken_date = CivilDate::parse(&brief.brief_date)
        .map(|d| d.spoken_zh())
        .unwrap_or_else(|| brief.brief_date.clone());
    parts.push(format!("{}。", spoken_date));
    parts.push(format!(
        "今日到期 {} 项，临期 {} 项，逾期 {} 项。",
        brief.due_today.len(),
        brief.due_soon.len(),
        brief.overdue.len()
    ));
    // 要点按界面的栏序取：① 当日到期 → ② 临期 → ② 逾期，最多 3 条。
    // 旧实现漏了临期栏：界面明明列出「临期 2 项」，播报却只念计数不念任何标题，
    // 用户听到的就是「说了 2 项、一条都没提」——这正是「播报与实际数据不符」。
    let mut picks: Vec<String> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for (label, arr) in [
        ("今日到期", &brief.due_today),
        ("临期", &brief.due_soon),
        ("逾期", &brief.overdue),
    ] {
        for a in arr.iter() {
            if picks.len() >= 3 {
                break;
            }
            // 同一事项可能同时出现在两栏（界面也两份），但念两遍听感上就是故障。
            if seen.contains(&a.title.as_str()) {
                continue;
            }
            seen.push(a.title.as_str());
            // 口径（2026-11-03 用户拍板）：单条标题超 25 字截断到 25 字。
            // 播报是伴随行为，长标题逐字念完会突破 30 秒预算（见
            // long_titles_truncated_to_25_fit_time_budget 测试）；通知是看的，
            // 不受此限，仍保留完整标题。
            let title: String = a.title.chars().take(25).collect();
            picks.push(format!("{}：{}。", label, title));
        }
    }
    if !picks.is_empty() {
        parts.push(picks.join(""));
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
    fn monday_brief_no_longer_carries_weekly_aggregate() {
        // 2026-10-26 是周一。口径（2026-11-03 用户拍板「两边都去掉」）：
        // 周聚合整体下线——界面从不渲染、播报不再念、核心层不再算。
        // 测试数据故意用「周内启动/周内闭环」字样：标题里有「启动/闭环」
        // 也绝不能触发周聚合措辞，反向断言才算真的钉住了口径。
        let monday = CivilDate::new(2026, 10, 26);
        let items = vec![
            item("周内启动的甲事项", "2026-10-28", "2026-11-10", 0, ItemStatus::Pending),
            item("周内闭环的乙事项", "2026-09-01", "2026-10-30", 50, ItemStatus::Active),
        ];
        let brief = morning_brief(monday, &items, &Ruleset::default());
        let dec = decide(monday, 9 * 60, &DeliveryState::default());
        let (title, body) = notification_text(&brief, &dec);
        assert!(title.contains("晨报"));
        assert!(!body.contains("应闭环"), "周聚合已下线，周一通知不得再念：{}", body);
        assert!(!body.contains("应启动"), "周聚合已下线，周一通知不得再念：{}", body);
        let s = speech_text(&brief, &dec);
        assert!(!s.contains("应闭环"), "周聚合已下线，周一播报不得再念：{}", s);
        assert!(!s.contains("应启动"), "周聚合已下线，周一播报不得再念：{}", s);
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
    fn speech_text_covers_counts_and_picks_without_full_readout() {
        // F10：播报要点与晨报结构对齐——计数、要点、补发说明
        // （周聚合已随 2026-11-03 拍板整体下线，不再属于播报覆盖面）
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
        assert!(!s.contains("本周应闭环"), "周聚合已整体下线，周一播报不得再念：{}", s);
        // 要点最多 3 条：绝不逐字念全部
        let due = s.matches("今日到期：").count() + s.matches("逾期：").count();
        assert!(due <= 3, "播报要点必须截断在 3 条：{}", s);
    }

    #[test]
    fn speech_text_stays_under_design_time_budget() {
        // 模块文档写明「播报超过 30 秒就变成打扰」。这里守的是字数上限：
        // 按中文 5 字/秒折算（**经验估算，未在真机实测**），150 字 ≈ 30 秒。
        // 加日期后单条播报 89 字（测试实测字符数），仍有余量；此断言用于
        // 防止后续再往播报里塞内容时无声超时。
        let d = CivilDate::new(2026, 10, 30);
        let mut items = vec![];
        for i in 0..9 {
            items.push(item(
                &format!("第{}项需要今天处理的事项标题", i),
                "2026-10-30",
                "2026-10-30",
                50,
                ItemStatus::Active,
            ));
        }
        let brief = morning_brief(d, &items, &Ruleset::default());
        let dec = decide(d, 9 * 60, &DeliveryState::default());
        let s = speech_text(&brief, &dec);
        let n = s.chars().count();
        assert!(n <= 150, "播报字数 {} 超出 30 秒预算（按 5 字/秒估算）：{}", n, s);
    }

    #[test]
    fn speech_text_does_not_repeat_same_title_across_sections() {
        // 同一事项可同时落在「今日到期」与「临期」两栏（界面也分别列出）。
        // 界面重复是合理的（两栏各有语义），但念出来会变成「同一句话念两遍」，
        // 听众只会以为出错了。播报去重，不改变播报内容与界面的一致性。
        let d = CivilDate::new(2026, 10, 30);
        let items = vec![
            item("政府数据共享平台验收", "2026-10-30", "2026-10-30", 60, ItemStatus::Active),
        ];
        let brief = morning_brief(d, &items, &Ruleset::default());
        let dec = decide(d, 9 * 60, &DeliveryState::default());
        let s = speech_text(&brief, &dec);
        let n = s.matches("政府数据共享平台验收").count();
        assert_eq!(n, 1, "同一标题在播报里只能出现一次，实际 {} 次：{}", n, s);
    }

    #[test]
    fn speech_text_reads_duesoon_titles_when_only_duesoon_exists() {
        // 验收第 4 条要求「至少 2 条公告要点」与界面一致。界面四个列表里
        // ② 临期 是独立一栏，但旧实现只从 due_today / overdue 取标题，
        // 于是「界面有两条临期、播报一条标题都不念」——计数说 2 项，内容却空。
        let d = CivilDate::new(2026, 10, 30);
        let items = vec![
            item("临期事项甲", "2026-09-01", "2026-11-02", 50, ItemStatus::Active),
            item("临期事项乙", "2026-09-01", "2026-11-03", 50, ItemStatus::Active),
        ];
        let brief = morning_brief(d, &items, &Ruleset::default());
        assert_eq!(brief.due_soon.len(), 2, "前置：两天后到期应先落入临期栏");
        assert!(brief.due_today.is_empty() && brief.overdue.is_empty(), "前置：不得到期/逾期");
        let dec = decide(d, 9 * 60, &DeliveryState::default());
        let s = speech_text(&brief, &dec);
        assert!(s.contains("临期事项甲"), "临期标题必须念出，否则计数与内容对不上：{}", s);
    }

    #[test]
    fn speech_text_reads_today_date_and_weekday() {
        // P1 缺陷（用户 2026-10-30：「播报语音和实际数据不符合」）的验收第 4 条：
        // 播报内容必须与界面晨报数据逐项一致，其中「日期」是点名项。
        // 界面晨报标题渲染的是 brief.date（= brief_date），播报若不念日期，
        // 用户听到的就是一段没有任何时间锚点的话——这是实现漏项，不是规格。
        let d = CivilDate::new(2026, 10, 30); // 周五
        let brief = morning_brief(d, &[], &Ruleset::default());
        let dec = decide(d, 9 * 60, &DeliveryState::default());
        let s = speech_text(&brief, &dec);
        // 断言「念出来的那一天」必须就是界面显示的那一天：
        // 从 brief_date 反解出的口语串必须原样出现在播报里，而不是另算一个日期。
        let spoken = CivilDate::parse(&brief.brief_date)
            .expect("brief_date 必须是合法 ISO 日期")
            .spoken_zh();
        assert!(
            s.contains(&spoken),
            "播报必须读出当日日期（与界面同源的 brief_date → {}）：{}",
            spoken,
            s
        );
        assert_eq!(spoken, "2026年10月30日，周五", "口语日期口径固定：{}", spoken);
        assert!(s.starts_with("早上好。今日晨报。"), "日期应紧跟问候，不打断原开场：{}", s);
    }

    #[test]
    fn speech_text_marks_late_catch_up() {
        let d = CivilDate::new(2026, 10, 27);
        let brief = morning_brief(d, &[], &Ruleset::default());
        let dec = decide(d, 14 * 60 + 30, &DeliveryState::default());
        let s = speech_text(&brief, &dec);
        assert!(s.starts_with("早上好。现在是补发晨报。"), "补发播报开头必须说明：{}", s);
        // 补发分支同样必须念出当日日期：补发的意义是「补哪一天」，没日期等于白补。
        let spoken = CivilDate::parse(&brief.brief_date)
            .expect("brief_date 必须是合法 ISO 日期")
            .spoken_zh();
        assert!(s.contains(&spoken), "补发播报必须读出当日日期（{}）：{}", spoken, s);
    }


    #[test]
    fn speech_text_matches_ui_brief_on_date_counts_and_titles() {
        // 用户 2026-10-30 P1 缺陷「播报语音和实际数据不符合」的验收第 4 条原文：
        // 「开启语音包播一次晨报，验证日期、条数、≥2 条公告要点与 UI 一致」。
        // 现有测试是拆开守的（日期一条、计数一条、临期标题一条），但它们分别成立
        // 不代表「同一次播报里三样都对得上」。这条测试把三样放在同一次调用里核，
        // 且核的是 UI 真正渲染的那三个数组（app.js:342/344/346 → b.due_today /
        // b.due_soon / b.overdue），不是另算一遍规则口径——否则测的是规则，不是一致性。
        let d = CivilDate::new(2026, 10, 30); // 周五
        let items = vec![
            item("当日到期甲", "2026-10-01", "2026-10-30", 40, ItemStatus::Active),
            item("当日到期乙", "2026-10-02", "2026-10-30", 20, ItemStatus::Pending),
            item("临期丙", "2026-10-05", "2026-11-01", 50, ItemStatus::Active),
            item("远期丁", "2026-11-01", "2026-12-31", 0, ItemStatus::Pending),
        ];
        let brief = morning_brief(d, &items, &Ruleset::default());
        let dec = decide(d, 9 * 60, &DeliveryState::default());
        let s = speech_text(&brief, &dec);

        // ① 日期：必须与界面晨报标题同源（brief_date），不能另算一个日期
        let spoken = CivilDate::parse(&brief.brief_date)
            .expect("brief_date 必须是合法 ISO 日期")
            .spoken_zh();
        assert!(s.contains(&spoken), "播报日期必须与界面一致（{}）：{}", spoken, s);

        // ② 条数：三个桶的计数必须逐个按 UI 数组长度念出，一个都不能错位
        let ui_counts = format!(
            "今日到期 {} 项，临期 {} 项，逾期 {} 项。",
            brief.due_today.len(),
            brief.due_soon.len(),
            brief.overdue.len()
        );
        assert!(
            s.contains(&ui_counts),
            "播报条数必须与 UI 三个列表逐项一致：期望「{}」，实际播报「{}」",
            ui_counts,
            s
        );

        // ③ 要点：至少 2 条标题要念出来，且每条都必须真的在 UI 列表里
        let ui_titles: Vec<&str> = brief
            .due_today
            .iter()
            .chain(brief.due_soon.iter())
            .chain(brief.overdue.iter())
            .map(|a| a.title.as_str())
            .collect();
        let spoken_count = ui_titles.iter().filter(|t| s.contains(**t)).count();
        assert!(
            spoken_count >= 2,
            "验收要求至少念出 2 条要点，实际念出 {} 条（UI 共 {} 条）：{}",
            spoken_count,
            ui_titles.len(),
            s
        );
        // 反向也要成立：同一标题不得念两遍（听感上就是故障）
        for t in &ui_titles {
            let n = s.matches(t).count();
            assert!(n <= 1, "同一标题最多念一次，{} 念了 {} 次：{}", t, n, s);
        }
    }


    #[test]
    fn long_titles_truncated_to_25_fit_time_budget() {
        // 口径（2026-11-03 用户拍板）：单条标题超 25 字截断到 25 字。
        // 背景是 P1 缺陷验收第 4 条「≥2 条要点与 UI 一致」——实现按条数截断
        // （最多 3 条）但没有约束单条标题长度，既有预算测试用的标题只有 15 字
        // 所以一直绿；真实长度的业务标题（50 字上下）会突破 30 秒预算：
        //   不截断 → 187 字 ≈ 37.4 秒（超）；单条截 25 字 → 140 字 ≈ 28.0 秒（达标）。
        // 按 5 字/秒折算（与既有预算测试同一口径，属经验估算、未真机实测）。
        // 本测试 2026-11-03 起启用（原 #[ignore] 的已知缺口已按拍板口径修复）。
        let d = CivilDate::new(2026, 10, 30);
        let titles = [
            "关于推进全省政务数据共享交换平台二期工程建设并完成省级部门数据目录编制与质量核查工作的专项事项",
            "关于开展2026年度长三角一体化发展重点项目资金申报材料汇总审核并完成上报工作的专项事项",
            "关于组织市级政务云资源池扩容改造并完成等保三级测评整改闭环的专项事项",
        ];
        let items: Vec<_> = titles
            .iter()
            .map(|t| item(t, "2026-10-30", "2026-10-30", 60, ItemStatus::Active))
            .collect();
        let brief = morning_brief(d, &items, &Ruleset::default());
        let dec = decide(d, 9 * 60, &DeliveryState::default());
        let s = speech_text(&brief, &dec);
        let n = s.chars().count();
        assert!(n <= 150, "三条真实长度标题的播报 {} 字，超出 30 秒预算：{}", n, s);
        // 截断口径本身也要被钉住：完整长标题不得出现，前 25 字必须出现
        for t in &titles {
            assert!(!s.contains(*t), "超 25 字标题未被截断，整条念出：{}", t);
            let head: String = t.chars().take(25).collect();
            assert!(s.contains(&head), "截断应保留前 25 字：{} / 播报：{}", head, s);
        }
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
